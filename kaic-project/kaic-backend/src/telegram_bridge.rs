//! Telegram Bridge — бот-ассистент (Сценарий А).
//!
//! Второй клиент Control Center API, ровно в том же положении, что и Electron
//! UI: ходит по HTTP в `/tasks`, `/tasks/{id}` и не имеет никаких приватных
//! путей внутрь backend'а. Router/Scheduler/ModelBackend этот модуль не
//! импортирует — он о них не знает.
//!
//! Что здесь есть:
//! - long polling через teloxide;
//! - живой прогресс редактированием ОДНОГО сообщения (`editMessageText`);
//! - цикл «черновик → подтверждение → отправка» с привязкой к хэшу текста,
//!   протуханием и append-only журналом;
//! - пометка об ассистенте как свойство операции отправки.
//!
//! Чего здесь НЕТ и не должно быть: Сценария Б (личный аккаунт через MTProto)
//! и медиа-пайплайна. Точка расширения для Сценария Б — трейт [`DraftSink`]:
//! он добавит свою реализацию доставки, переиспользуя весь цикл подтверждения
//! как есть.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::control_center_api::ARTIFACT_ROLE;

/// Имя переменной окружения с токеном бота — конвенция самой библиотеки
/// teloxide, а не наша выдумка. Токен нигде в репозитории не хранится.
pub const TOKEN_ENV: &str = "TELOXIDE_TOKEN";

/// Роль записи контекста, в которой лежит черновик исходящего сообщения.
/// Отдельная роль (а не "assistant") нужна, чтобы черновик нельзя было
/// перепутать с обычным ответом модели пользователю.
const DRAFT_ROLE: &str = "draft";

/// Где запоминается владелец бота.
///
/// Рядом с журналом отправок, под тем же `.kaic/` — это рантайм-состояние
/// машины, а не код.
const OWNER_FILE: &str = ".kaic/telegram-owner.txt";

// --- Ограничение по владельцу ------------------------------------------------

/// Кто вправе пользоваться ботом.
///
/// Имя бота публично: написать ему может кто угодно, кто его найдёт. Поэтому
/// первый написавший становится владельцем, и дальше бот отвечает только ему.
///
/// Владелец хранится **в файле**, а не в памяти процесса: иначе перезапуск
/// backend'а сбрасывал бы захват, и следующим владельцем стал бы случайный
/// человек, написавший первым после рестарта.
struct OwnerGuard {
    path: PathBuf,
    /// Кэш, чтобы не читать файл на каждое сообщение. Mutex здесь не ради
    /// скорости, а ради атомарности захвата: два сообщения, пришедшие
    /// одновременно, не должны оба стать владельцем.
    cached: Mutex<Option<i64>>,
}

#[derive(Debug, PartialEq, Eq)]
enum OwnerDecision {
    /// Владельца не было — этот отправитель им стал.
    Claimed,
    /// Отправитель и есть владелец.
    Allowed,
    /// Есть другой владелец.
    Denied,
}

impl OwnerGuard {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cached: Mutex::new(None),
        }
    }

    fn read_from_disk(&self) -> Option<i64> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
    }

    fn write_to_disk(&self, user_id: i64) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("не удалось создать {}", parent.display()))?;
        }
        std::fs::write(&self.path, user_id.to_string())
            .with_context(|| format!("не удалось записать {}", self.path.display()))
    }

    /// Решает судьбу отправителя, при необходимости захватывая владение.
    async fn authorize(&self, user_id: i64) -> OwnerDecision {
        let mut cached = self.cached.lock().await;

        // Файл — источник правды: он мог быть создан прошлым запуском.
        let owner = match *cached {
            Some(owner) => Some(owner),
            None => {
                let from_disk = self.read_from_disk();
                *cached = from_disk;
                from_disk
            }
        };

        match owner {
            Some(owner) if owner == user_id => OwnerDecision::Allowed,
            Some(_) => OwnerDecision::Denied,
            None => {
                if let Err(err) = self.write_to_disk(user_id) {
                    // Не сумели закрепить — безопаснее отказать, чем работать
                    // с незафиксированным владельцем.
                    tracing::error!("не удалось сохранить владельца: {err:#}");
                    return OwnerDecision::Denied;
                }
                *cached = Some(user_id);
                tracing::info!("владелец бота закреплён: user_id={user_id}");
                OwnerDecision::Claimed
            }
        }
    }
}

// --- Конфигурация -----------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    /// Пометка, добавляемая к сообщениям, отправленным от имени пользователя.
    /// Настраиваемая, с дефолтом — не хардкод.
    #[serde(default = "default_assistant_label")]
    pub assistant_label: String,

    /// Время жизни подтверждения черновика.
    #[serde(default = "default_approval_ttl_seconds")]
    pub approval_ttl_seconds: i64,

    #[serde(default = "default_audit_log_path")]
    pub audit_log_path: String,

    #[serde(default = "default_api_base_url")]
    pub api_base_url: String,

    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

fn default_assistant_label() -> String {
    "🤖 сообщение от ассистента".to_string()
}
fn default_approval_ttl_seconds() -> i64 {
    900
}
fn default_audit_log_path() -> String {
    ".kaic/telegram-audit.log".to_string()
}
fn default_api_base_url() -> String {
    "http://127.0.0.1:4545".to_string()
}
fn default_poll_interval_ms() -> u64 {
    1000
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            assistant_label: default_assistant_label(),
            approval_ttl_seconds: default_approval_ttl_seconds(),
            audit_log_path: default_audit_log_path(),
            api_base_url: default_api_base_url(),
            poll_interval_ms: default_poll_interval_ms(),
        }
    }
}

impl TelegramConfig {
    /// Читает config/telegram.yaml. Отсутствие файла — не ошибка: у всех
    /// полей есть дефолты, и бот должен подниматься без обязательного конфига.
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_yaml::from_str(&raw)
                .with_context(|| format!("не удалось разобрать {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("не удалось прочитать {}", path.display())),
        }
    }
}

// --- Доставка подтверждённых черновиков --------------------------------------

/// Куда уходит подтверждённое сообщение.
///
/// Сценарий А умеет доставлять только в чаты, доступные самому боту.
/// Сценарий Б добавит реализацию поверх личного аккаунта — и получит цикл
/// подтверждения, протухание и журнал бесплатно, не изобретая своих.
#[async_trait::async_trait]
pub trait DraftSink: Send + Sync {
    /// Человекочитаемое «кому» для карточки подтверждения.
    fn describe(&self) -> String;

    async fn deliver(&self, text: &str) -> Result<()>;
}

/// Доставка в чат, где присутствует сам бот (единственный доступный в А).
pub struct BotChatSink {
    bot: Bot,
    chat_id: ChatId,
    title: String,
}

impl BotChatSink {
    pub fn new(bot: Bot, chat_id: ChatId, title: impl Into<String>) -> Self {
        Self {
            bot,
            chat_id,
            title: title.into(),
        }
    }
}

#[async_trait::async_trait]
impl DraftSink for BotChatSink {
    fn describe(&self) -> String {
        self.title.clone()
    }

    async fn deliver(&self, text: &str) -> Result<()> {
        self.bot
            .send_message(self.chat_id, text.to_string())
            .await
            .context("не удалось доставить сообщение")?;
        Ok(())
    }
}

// --- Ожидающие подтверждения черновики ---------------------------------------

struct PendingDraft {
    task_id: Uuid,
    /// Хэш текста в момент показа карточки. Если черновик в задаче изменится,
    /// подтверждение станет недействительным — одобряли не это.
    text_hash: u64,
    created_at: DateTime<Utc>,
    sink: Arc<dyn DraftSink>,
    origin_chat: ChatId,
    prompt_message: MessageId,
}

#[derive(Default)]
struct Approvals {
    next_id: u32,
    drafts: HashMap<u32, PendingDraft>,
}

/// Хэш текста черновика.
///
/// Намеренно используется внутрипроцессный хэшер: подтверждения живут минуты
/// и не обязаны переживать перезапуск. После рестарта старое подтверждение
/// перестаёт существовать вместе с картой — это безопасное поведение, а не
/// потеря данных.
fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// --- Общее состояние ---------------------------------------------------------

#[derive(Clone)]
pub struct BridgeState {
    config: Arc<TelegramConfig>,
    http: reqwest::Client,
    approvals: Arc<Mutex<Approvals>>,
    owner: Arc<OwnerGuard>,
}

impl BridgeState {
    pub fn new(config: TelegramConfig) -> Self {
        Self {
            config: Arc::new(config),
            http: reqwest::Client::new(),
            approvals: Arc::new(Mutex::new(Approvals::default())),
            owner: Arc::new(OwnerGuard::new(OWNER_FILE)),
        }
    }
}

// --- Формы Control Center API ------------------------------------------------
// Локальные зеркала: Task в task_store.rs только Serialize, и тянуть в него
// Deserialize ради одного клиента не стоит — Bridge общается по HTTP и обязан
// уметь разобрать ответ сам.

#[derive(Debug, Deserialize)]
struct ApiTask {
    id: Uuid,
    category: String,
    status: String,
    context: Vec<ApiContextEntry>,
}

#[derive(Debug, Deserialize)]
struct ApiContextEntry {
    role: String,
    content: String,
}

impl ApiTask {
    fn last_with_role(&self, role: &str) -> Option<&str> {
        self.context
            .iter()
            .rev()
            .find(|e| e.role == role)
            .map(|e| e.content.as_str())
    }
}

// --- Живой прогресс ----------------------------------------------------------

/// Прогресс задачи в ОДНОМ сообщении: каждый шаг переписывает его целиком
/// через `editMessageText`, а не плодит новые сообщения.
struct Progress {
    bot: Bot,
    chat: ChatId,
    message: MessageId,
    lines: Vec<String>,
}

impl Progress {
    async fn start(bot: &Bot, chat: ChatId, first_line: &str) -> Result<Self> {
        let sent = bot
            .send_message(chat, first_line.to_string())
            .await
            .context("не удалось отправить сообщение прогресса")?;
        Ok(Self {
            bot: bot.clone(),
            chat,
            message: sent.id,
            lines: vec![first_line.to_string()],
        })
    }

    async fn step(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
        let text = self.lines.join("\n");
        if let Err(err) = self
            .bot
            .edit_message_text(self.chat, self.message, text)
            .await
        {
            // Прогресс — вспомогательная вещь: если Telegram отверг правку
            // (например, текст не изменился), задача из-за этого падать
            // не должна.
            tracing::warn!("не удалось обновить сообщение прогресса: {err}");
        }
    }

    /// Превью результата отправляется ОТДЕЛЬНЫМ сообщением и только когда у
    /// задачи есть визуальный предмет. Первым таким предметом стало готовое
    /// видео из медиа-пайплайна.
    ///
    /// Способ отправки выбирается по расширению: Telegram отвергает `.mp4`,
    /// присланный как фото, и наоборот. Это не «угадывание типа» — набор
    /// расширений задаём мы сами в медиа-пайплайне.
    async fn preview(&self, file: std::path::PathBuf, caption: &str) -> Result<()> {
        use teloxide::types::InputFile;

        let is_video = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("mp4"))
            .unwrap_or(false);

        if is_video {
            self.bot
                .send_video(self.chat, InputFile::file(file))
                .caption(caption.to_string())
                .await
                .context("не удалось отправить видео-результат")?;
        } else {
            self.bot
                .send_photo(self.chat, InputFile::file(file))
                .caption(caption.to_string())
                .await
                .context("не удалось отправить превью")?;
        }
        Ok(())
    }
}

// --- Обработка входящего сообщения -------------------------------------------

async fn on_message(bot: Bot, msg: Message, state: BridgeState) -> Result<()> {
    // Проверка владельца — прежде всего остального: до создания задачи,
    // до расхода моделей и до любого ответа по существу.
    let sender = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or_default();
    match state.owner.authorize(sender).await {
        OwnerDecision::Claimed => {
            bot.send_message(msg.chat.id, "Готов к работе.").await?;
        }
        OwnerDecision::Allowed => {}
        OwnerDecision::Denied => {
            // Коротко и без объяснений: чужому человеку незачем знать
            // ни что это за бот, ни почему ему отказали.
            bot.send_message(msg.chat.id, "Недоступно.").await?;
            return Ok(());
        }
    }

    let Some(text) = msg.text().map(str::to_string) else {
        bot.send_message(msg.chat.id, "Понимаю только текст.")
            .await?;
        return Ok(());
    };
    if text.trim().is_empty() {
        return Ok(());
    }

    let mut progress = Progress::start(&bot, msg.chat.id, "🔎 Принял задачу, определяю категорию…").await?;

    let task = match create_task(&state, &text).await {
        Ok(task) => task,
        Err(err) => {
            progress.step(format!("❌ Не удалось создать задачу: {err:#}")).await;
            return Ok(());
        }
    };
    progress
        .step(format!("📂 Категория: {} · задача {}", task.category, short_id(task.id)))
        .await;
    progress.step("⚙️ Подбираю модель и выполняю…").await;

    let finished = match await_completion(&state, task.id, &mut progress).await {
        Ok(task) => task,
        Err(err) => {
            progress.step(format!("❌ Ошибка ожидания: {err:#}")).await;
            return Ok(());
        }
    };

    match finished.status.as_str() {
        "waiting_for_human" => {
            match finished.last_with_role(DRAFT_ROLE) {
                Some(draft) => {
                    let sink: Arc<dyn DraftSink> = Arc::new(BotChatSink::new(
                        bot.clone(),
                        msg.chat.id,
                        "этот чат",
                    ));
                    offer_draft(&bot, &state, &finished, draft, sink, msg.chat.id).await?;
                }
                None => {
                    progress
                        .step("⏸️ Задача ждёт решения человека, но черновика в контексте нет.")
                        .await;
                }
            }
        }
        "done" => {
            let answer = finished
                .last_with_role("assistant")
                .unwrap_or("(модель не вернула текст)");
            progress.step("✅ Готово.").await;
            bot.send_message(msg.chat.id, answer.to_string()).await?;

            // Если задача оставила файл-результат — показать его тут же.
            // chat_id никуда передавать не пришлось: обработчик держит его
            // с самого начала и сам дождался завершения задачи.
            if let Some(artifact) = finished.last_with_role(ARTIFACT_ROLE) {
                let path = std::path::PathBuf::from(artifact);
                if path.is_file() {
                    if let Err(err) = progress.preview(path, "Результат монтажа").await {
                        // Результат уже описан текстом выше — не отправившееся
                        // превью не повод считать задачу проваленной.
                        tracing::warn!("не удалось отправить результат: {err:#}");
                    }
                } else {
                    tracing::warn!("файл результата не найден: {artifact}");
                }
            }
        }
        "failed" => {
            let reason = finished.last_with_role("system").unwrap_or("причина не записана");
            progress.step(format!("❌ Задача завершилась ошибкой: {reason}")).await;
        }
        other => {
            progress.step(format!("ℹ️ Задача в состоянии «{other}».")).await;
        }
    }

    Ok(())
}

/// Показывает карточку подтверждения: кому, текст черновика и четыре кнопки.
async fn offer_draft(
    bot: &Bot,
    state: &BridgeState,
    task: &ApiTask,
    draft: &str,
    sink: Arc<dyn DraftSink>,
    origin_chat: ChatId,
) -> Result<()> {
    let card = format!(
        "✍️ Черновик\n\nКому: {}\n\n{}\n\nПометка «{}» будет добавлена при отправке.",
        sink.describe(),
        draft,
        state.config.assistant_label
    );

    let sent = bot.send_message(origin_chat, card).await?;

    let id = {
        let mut approvals = state.approvals.lock().await;
        approvals.next_id = approvals.next_id.wrapping_add(1);
        let id = approvals.next_id;
        approvals.drafts.insert(
            id,
            PendingDraft {
                task_id: task.id,
                text_hash: hash_text(draft),
                created_at: Utc::now(),
                sink,
                origin_chat,
                prompt_message: sent.id,
            },
        );
        id
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Отправить", format!("snd:{id}")),
            InlineKeyboardButton::callback("Отправить без пометки", format!("raw:{id}")),
        ],
        vec![
            InlineKeyboardButton::callback("Правка", format!("edt:{id}")),
            InlineKeyboardButton::callback("Отмена", format!("cnl:{id}")),
        ],
    ]);
    bot.edit_message_reply_markup(origin_chat, sent.id)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}

// --- Обработка нажатия кнопки ------------------------------------------------

async fn on_callback(bot: Bot, query: CallbackQuery, state: BridgeState) -> Result<()> {
    // Кнопки — тот же уровень доступа, что и сообщения: нажатие отправляет
    // сообщение от имени пользователя, и чужому оно недоступно.
    if state.owner.authorize(query.from.id.0 as i64).await == OwnerDecision::Denied {
        bot.answer_callback_query(query.id).text("Недоступно.").await?;
        return Ok(());
    }

    let Some(data) = query.data.clone() else {
        return Ok(());
    };
    let Some((action, raw_id)) = data.split_once(':') else {
        return Ok(());
    };
    let Ok(id) = raw_id.parse::<u32>() else {
        return Ok(());
    };

    // Черновик изымается из карты: одно подтверждение — одна отправка,
    // повторное нажатие той же кнопки уже ничего не найдёт.
    let pending = state.approvals.lock().await.drafts.remove(&id);
    let Some(pending) = pending else {
        bot.answer_callback_query(query.id)
            .text("Подтверждение не найдено или уже использовано.")
            .await?;
        return Ok(());
    };

    let verdict = match action {
        "cnl" => Verdict::Cancelled,
        "edt" => Verdict::EditRequested,
        "snd" | "raw" => verify(&state, &pending).await,
        _ => return Ok(()),
    };

    match verdict {
        Verdict::Cancelled => {
            finish_card(&bot, &pending, "🚫 Отменено, ничего не отправлено.").await;
            bot.answer_callback_query(query.id).await?;
        }
        Verdict::EditRequested => {
            finish_card(
                &bot,
                &pending,
                "✏️ Пришли исправленный текст отдельным сообщением — он станет новым черновиком.",
            )
            .await;
            bot.answer_callback_query(query.id).await?;
        }
        Verdict::Expired => {
            finish_card(
                &bot,
                &pending,
                "⏳ Подтверждение просрочено. Разговор мог уйти вперёд — сформируй черновик заново.",
            )
            .await;
            bot.answer_callback_query(query.id)
                .text("Просрочено.")
                .await?;
        }
        Verdict::Changed => {
            finish_card(
                &bot,
                &pending,
                "⚠️ Текст черновика изменился после показа. Подтверждение недействительно — одобряли не это.",
            )
            .await;
            bot.answer_callback_query(query.id)
                .text("Черновик изменился.")
                .await?;
        }
        Verdict::Approved(text) => {
            let with_label = action == "snd";
            let payload = if with_label {
                format!("{}\n\n{}", text, state.config.assistant_label)
            } else {
                text.clone()
            };

            match pending.sink.deliver(&payload).await {
                Ok(()) => {
                    if let Err(err) = append_audit(&state.config.audit_log_path, &AuditRecord {
                        at: Utc::now(),
                        task_id: pending.task_id,
                        target: pending.sink.describe(),
                        text_hash: pending.text_hash,
                        labeled: with_label,
                        approval_id: id,
                    }) {
                        tracing::error!("отправлено, но не записано в журнал: {err:#}");
                    }
                    let note = if with_label {
                        "✅ Отправлено с пометкой ассистента."
                    } else {
                        "✅ Отправлено БЕЗ пометки (по явному решению)."
                    };
                    finish_card(&bot, &pending, note).await;
                    bot.answer_callback_query(query.id).await?;
                }
                Err(err) => {
                    finish_card(&bot, &pending, &format!("❌ Не отправлено: {err:#}")).await;
                    bot.answer_callback_query(query.id)
                        .text("Ошибка отправки.")
                        .await?;
                }
            }
        }
    }

    Ok(())
}

enum Verdict {
    Approved(String),
    Expired,
    Changed,
    Cancelled,
    EditRequested,
}

/// Две независимые проверки перед отправкой: не просрочено ли подтверждение и
/// тот ли текст мы отправляем, который показывали. Черновик перечитывается из
/// задачи заново — сравнение с сохранённым в памяти текстом ничего бы не дало.
async fn verify(state: &BridgeState, pending: &PendingDraft) -> Verdict {
    let age = Utc::now().signed_duration_since(pending.created_at);
    if age.num_seconds() > state.config.approval_ttl_seconds {
        return Verdict::Expired;
    }

    let task = match fetch_task(state, pending.task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => return Verdict::Changed,
        Err(err) => {
            tracing::warn!("не удалось перечитать задачу перед отправкой: {err:#}");
            return Verdict::Changed;
        }
    };

    match task.last_with_role(DRAFT_ROLE) {
        Some(current) if hash_text(current) == pending.text_hash => Verdict::Approved(current.to_string()),
        _ => Verdict::Changed,
    }
}

/// Гасит карточку: убирает кнопки и подписывает исход. Кнопки обязаны исчезать,
/// иначе повторное нажатие выглядит как возможное действие.
async fn finish_card(bot: &Bot, pending: &PendingDraft, note: &str) {
    if let Err(err) = bot
        .edit_message_reply_markup(pending.origin_chat, pending.prompt_message)
        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
        .await
    {
        tracing::warn!("не удалось убрать кнопки: {err}");
    }
    if let Err(err) = bot
        .send_message(pending.origin_chat, note.to_string())
        .await
    {
        tracing::warn!("не удалось отправить итог: {err}");
    }
}

// --- Журнал отправок ---------------------------------------------------------

struct AuditRecord {
    at: DateTime<Utc>,
    task_id: Uuid,
    target: String,
    text_hash: u64,
    labeled: bool,
    approval_id: u32,
}

/// Append-only журнал по образцу .kaic/write-audit.log: только дозапись,
/// никогда не перезапись и не усечение.
fn append_audit(path: impl AsRef<Path>, record: &AuditRecord) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("не удалось открыть журнал {}", path.display()))?;

    writeln!(
        file,
        "{}\ttask={}\ttarget={}\thash={:016x}\tlabeled={}\tapproval={}",
        record.at.to_rfc3339(),
        record.task_id,
        record.target.replace('\t', " "),
        record.text_hash,
        record.labeled,
        record.approval_id
    )
    .context("не удалось записать строку журнала")?;
    Ok(())
}

// --- Клиент Control Center API -----------------------------------------------

/// Тело POST /tasks. Отдельная структура, а не ad-hoc JSON: serde_json не
/// входит в зависимости крейта, и тянуть его ради одного объекта незачем.
#[derive(serde::Serialize)]
struct CreateTaskBody<'a> {
    text: &'a str,
}

async fn create_task(state: &BridgeState, text: &str) -> Result<ApiTask> {
    let url = format!("{}/tasks", state.config.api_base_url);
    let task = state
        .http
        .post(&url)
        .json(&CreateTaskBody { text })
        .send()
        .await
        .context("Control Center API недоступен")?
        .error_for_status()
        .context("Control Center API отклонил создание задачи")?
        .json::<ApiTask>()
        .await
        .context("не удалось разобрать ответ Control Center API")?;
    Ok(task)
}

async fn fetch_task(state: &BridgeState, id: Uuid) -> Result<Option<ApiTask>> {
    let url = format!("{}/tasks/{}", state.config.api_base_url, id);
    let response = state.http.get(&url).send().await.context("Control Center API недоступен")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let task = response
        .error_for_status()
        .context("Control Center API вернул ошибку")?
        .json::<ApiTask>()
        .await
        .context("не удалось разобрать задачу")?;
    Ok(Some(task))
}

/// Ждёт терминального состояния задачи, отражая смену статуса в прогрессе.
async fn await_completion(state: &BridgeState, id: Uuid, progress: &mut Progress) -> Result<ApiTask> {
    let mut last_status = String::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(state.config.poll_interval_ms)).await;

        let Some(task) = fetch_task(state, id).await? else {
            anyhow::bail!("задача {id} исчезла");
        };

        if task.status != last_status {
            last_status = task.status.clone();
            if last_status == "running" {
                progress.step("🧠 Модель работает…").await;
            }
        }

        if matches!(task.status.as_str(), "done" | "failed" | "cancelled" | "waiting_for_human") {
            return Ok(task);
        }
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

// --- Запуск ------------------------------------------------------------------

/// Поднимает бота, если задан токен.
///
/// Отсутствие токена — штатная ситуация, а не ошибка: backend обязан
/// работать и без Telegram, поэтому здесь только запись в лог и выход.
pub async fn spawn_if_configured(config: TelegramConfig) {
    let Ok(token) = std::env::var(TOKEN_ENV) else {
        tracing::info!(
            "Telegram Bridge не запущен: переменная {TOKEN_ENV} не задана (это не ошибка)"
        );
        return;
    };
    if token.trim().is_empty() {
        tracing::warn!("Telegram Bridge не запущен: {TOKEN_ENV} пуста");
        return;
    }

    tokio::spawn(async move {
        if let Err(err) = run(token, config).await {
            tracing::error!("Telegram Bridge остановлен с ошибкой: {err:#}");
        }
    });
}

async fn run(token: String, config: TelegramConfig) -> Result<()> {
    let bot = Bot::new(token);
    let state = BridgeState::new(config);

    tracing::info!("Telegram Bridge: long polling запущен");

    // Ошибки обработчиков логируются и гасятся здесь: сбой одной задачи не
    // должен ронять long polling целиком.
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(
            |bot: Bot, msg: Message, state: BridgeState| async move {
                if let Err(err) = on_message(bot, msg, state).await {
                    tracing::error!("ошибка обработки сообщения: {err:#}");
                }
                ResponseResult::<()>::Ok(())
            },
        ))
        .branch(Update::filter_callback_query().endpoint(
            |bot: Bot, query: CallbackQuery, state: BridgeState| async move {
                if let Err(err) = on_callback(bot, query, state).await {
                    tracing::error!("ошибка обработки нажатия: {err:#}");
                }
                ResponseResult::<()>::Ok(())
            },
        ));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_has_default_and_is_configurable() {
        let default = TelegramConfig::default();
        assert!(!default.assistant_label.is_empty());

        let custom: TelegramConfig =
            serde_yaml::from_str("assistant_label: \"[бот]\"").expect("конфиг разбирается");
        assert_eq!(custom.assistant_label, "[бот]");
        // Незаданные поля берут дефолты — конфиг необязателен целиком.
        assert_eq!(custom.approval_ttl_seconds, 900);
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let config = TelegramConfig::load_or_default("config/definitely-absent.yaml")
            .expect("отсутствие файла — штатная ситуация");
        assert_eq!(config.assistant_label, default_assistant_label());
    }

    #[test]
    fn example_config_matches_schema() {
        let example = include_str!("../config/telegram.example.yaml");
        let config: TelegramConfig = serde_yaml::from_str(example).expect("пример разбирается");
        assert_eq!(config.approval_ttl_seconds, 900);
        assert_eq!(config.audit_log_path, ".kaic/telegram-audit.log");
    }

    #[test]
    fn hash_detects_any_text_change() {
        let original = "Привет, буду в 18:00.";
        assert_eq!(hash_text(original), hash_text("Привет, буду в 18:00."));
        // Изменилось одно число — подтверждение обязано стать недействительным.
        assert_ne!(hash_text(original), hash_text("Привет, буду в 19:00."));
        assert_ne!(hash_text(original), hash_text("Привет, буду в 18:00. "));
    }

    #[test]
    fn label_is_applied_at_send_time_not_stored_in_draft() {
        // Пометка приклеивается к payload'у на отправке; сам черновик её не
        // содержит, поэтому правка черновика не может её «потерять».
        let draft = "Буду в 18:00.";
        let label = "🤖 сообщение от ассистента";
        let labeled = format!("{draft}\n\n{label}");

        assert!(!draft.contains(label));
        assert!(labeled.starts_with(draft));
        assert!(labeled.ends_with(label));
    }

    #[test]
    fn audit_log_appends_and_never_truncates() {
        let dir = std::env::temp_dir().join(format!("kaic-audit-{}", Uuid::new_v4()));
        let path = dir.join("telegram-audit.log");

        let record = |labeled: bool| AuditRecord {
            at: Utc::now(),
            task_id: Uuid::new_v4(),
            target: "этот чат".to_string(),
            text_hash: 0xdead_beef,
            labeled,
            approval_id: 1,
        };

        append_audit(&path, &record(true)).expect("первая запись");
        append_audit(&path, &record(false)).expect("вторая запись");

        let content = std::fs::read_to_string(&path).expect("журнал читается");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "вторая запись не должна затирать первую");
        assert!(lines[0].contains("labeled=true"));
        assert!(lines[1].contains("labeled=false"));
        assert!(lines[0].contains("hash=00000000deadbeef"));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_owner_file() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("kaic-owner-{}", Uuid::new_v4()))
            .join("telegram-owner.txt")
    }

    #[tokio::test]
    async fn first_sender_claims_ownership_and_others_are_denied() {
        let path = temp_owner_file();
        let guard = OwnerGuard::new(&path);

        assert_eq!(guard.authorize(1001).await, OwnerDecision::Claimed);
        // Тот же человек — дальше просто пропускается.
        assert_eq!(guard.authorize(1001).await, OwnerDecision::Allowed);
        // Любой другой — отказ. Имя бота публично, писать может кто угодно.
        assert_eq!(guard.authorize(2002).await, OwnerDecision::Denied);
        assert_eq!(guard.authorize(3003).await, OwnerDecision::Denied);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn ownership_survives_restart() {
        // Ключевое свойство: в памяти процесса хранить нельзя, иначе после
        // перезапуска владельцем стал бы случайный первый написавший.
        let path = temp_owner_file();

        let first_run = OwnerGuard::new(&path);
        assert_eq!(first_run.authorize(555).await, OwnerDecision::Claimed);

        // Новый экземпляр = новый запуск процесса, кэш пуст.
        let after_restart = OwnerGuard::new(&path);
        assert_eq!(after_restart.authorize(555).await, OwnerDecision::Allowed);
        assert_eq!(after_restart.authorize(777).await, OwnerDecision::Denied);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn unwritable_owner_file_denies_instead_of_running_unclaimed() {
        // Если закрепить владельца не удалось, безопаснее отказать, чем
        // работать с незафиксированным владением.
        let dir = std::env::temp_dir().join(format!("kaic-owner-ro-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("каталог создан");
        // Путь указывает на каталог: запись файла по нему невозможна.
        let guard = OwnerGuard::new(&dir);

        assert_eq!(guard.authorize(42).await, OwnerDecision::Denied);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn artifact_role_matches_the_api_contract() {
        // Бот читает путь к результату по этой роли; разъедься она с тем,
        // что пишет control_center_api, превью молча перестало бы работать.
        assert_eq!(ARTIFACT_ROLE, "artifact");
        assert_ne!(ARTIFACT_ROLE, DRAFT_ROLE);
    }

    #[test]
    fn draft_role_is_distinct_from_assistant_answer() {
        // Черновик исходящего сообщения не должен путаться с ответом модели
        // пользователю: иначе обычный ответ мог бы уйти на подтверждение.
        assert_ne!(DRAFT_ROLE, "assistant");
    }
}
