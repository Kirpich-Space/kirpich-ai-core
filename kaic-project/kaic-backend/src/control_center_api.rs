//! Control Center API — локальный HTTP-интерфейс поверх Task Store,
//! Scheduler и Resource Registry.
//!
//! Это единственный слой, который знает про HTTP. Task Store, Scheduler,
//! Router и Capability Registry ничего не знают о том, кто их вызывает —
//! сейчас это Electron (через `fetch()`), позже точно так же сможет быть
//! Telegram Bridge. Ни один из них не потребует переписывания этой схемы.
//!
//! Транспорт и маршруты живут в одном файле сознательно: разделять их
//! пока не на что — маршрутов немного, и они не разрастутся до отдельной
//! абстракции без реальной причины.
//!
//! Обновления статуса Electron получает поллингом (`GET /tasks`, `GET /status`
//! раз в несколько секунд), а не через WebSocket — для личного инструмента
//! разница в секунду-две не имеет значения, а поллинг не требует управления
//! соединениями и переподключениями.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tower_http::cors::{AllowHeaders, AllowMethods, CorsLayer};
use uuid::Uuid;

use crate::mcp::{McpClient, McpServerConfig};
use crate::media::credits::{plan_render, RenderPlan};
use crate::media::fetch as media_fetch;
use crate::media::gate::{admit, AdmittedAsset, Timeline};
use crate::media::openverse;
use crate::media::provenance::{Manifest, ManifestEntry};
use crate::media::query as media_query_extract;
use crate::media::render as media_render;
use crate::model_backend::{GenerateRequest, GenerateResponse, Message, Role};
use crate::resource_registry::ResourceRegistry;
// Алиас: axum тоже экспортирует тип `Router`, поэтому наш классификатор
// задач импортируется под другим именем — иначе имена конфликтуют.
use crate::router::{Category, Router as TaskRouter, TaskMetadata};
use crate::scheduler::Scheduler;
use crate::task_store::{ContextEntry, Task, TaskStatus, TaskStore};

/// Общее состояние, доступное всем обработчикам маршрутов.
#[derive(Clone)]
pub struct AppState {
    pub task_store: Arc<TaskStore>,
    pub scheduler: Arc<Scheduler>,
    pub resource_registry: Arc<ResourceRegistry>,
    pub control: TaskControl,
}

/// Связь между обработчиком маршрута и ИДУЩИМ пайплайном задачи.
///
/// Зачем это понадобилось. До этой правки `POST /tasks/:id/cancel` писал
/// `TaskStatus::Cancelled` в хранилище и возвращал 200 — и всё. Слово
/// `Cancelled` не читалось нигде: ни `run_task_pipeline`, ни `agent_steps`
/// в хранилище за ним не ходили. Сигнал терялся на первом же шаге, а
/// генерация досиживала до естественного конца. Измерено АРХИВ-105:
/// 46.141 с без отмены против 46.266 с с отменой — разница ноль.
///
/// Реестр в SQLite для этого не годится в принципе: он умеет хранить, но не
/// умеет БУДИТЬ. Нужен канал, по которому идущая задача узнаёт об отмене, не
/// опрашивая базу. `watch` выбран вместо `Notify` намеренно: он хранит
/// значение, поэтому отмена, пришедшая между двумя точками ожидания, не
/// теряется — поздний читатель всё равно увидит `true`.
#[derive(Clone, Default)]
pub struct TaskControl {
    running: Arc<tokio::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>>,
}

impl TaskControl {
    /// Атомарно занимает task ID. `None` означает, что pipeline уже идёт;
    /// существующий sender при этом не заменяется.
    pub async fn try_register(&self, id: Uuid) -> Option<tokio::sync::watch::Receiver<bool>> {
        let mut running = self.running.lock().await;
        if running.contains_key(&id) {
            return None;
        }
        let (tx, rx) = tokio::sync::watch::channel(false);
        running.insert(id, tx);
        Some(rx)
    }

    /// Снимает отметку. Вызывается на ЛЮБОМ исходе пайплайна, иначе
    /// завершившаяся задача навсегда осталась бы «идущей», и второй
    /// `continue` по ней уже никогда бы не запустился.
    pub async fn unregister(&self, id: Uuid) {
        self.running.lock().await.remove(&id);
    }

    /// Просит идущую задачу прекратить работу.
    ///
    /// Возвращает `true`, если было кого просить. `false` означает, что
    /// задача уже не идёт — и это не ошибка, а другой случай.
    pub async fn cancel(&self, id: Uuid) -> bool {
        match self.running.lock().await.get(&id) {
            Some(tx) => {
                let _ = tx.send(true);
                true
            }
            None => false,
        }
    }

    /// Идёт ли сейчас пайплайн по этой задаче.
    pub async fn is_running(&self, id: Uuid) -> bool {
        self.running.lock().await.contains_key(&id)
    }
}

/// Origin'ы, которым браузеру разрешено читать ответы Control Center.
///
/// Список явный, а не `permissive()`. Разница не косметическая:
/// `permissive()` отвечает `Access-Control-Allow-Origin: *`, то есть любая
/// открытая в браузере страница получала право ЧИТАТЬ очередь задач
/// пользователя. Здесь перечислено ровно то, что действительно есть:
/// Vite dev-сервер kaic-ui (5173) и сам Control Center (4545), на обеих
/// формах записи петлевого адреса — браузер считает `localhost` и
/// `127.0.0.1` разными origin'ами.
///
/// Electron в упакованном виде обращается с `file://` и origin не шлёт
/// вовсе; такие запросы CORS не касается — их пускает или не пускает
/// аутентификация, и это верное разделение обязанностей.
const ALLOWED_ORIGINS: [&str; 4] = [
    "http://127.0.0.1:5173",
    "http://localhost:5173",
    "http://127.0.0.1:4545",
    "http://localhost:4545",
];

/// Собирает маршруты Control Center API.
///
/// [`ControlToken`] — обязательный аргумент, и это главное в сигнатуре.
/// Роутер без токена невозможно собрать: не потому, что об этом помнят, а
/// потому, что нечего подставить. Ровно так же `execute_tool` принимает
/// только `ApprovedToolCall`.
pub fn build_router(state: AppState, token: crate::control_auth::ControlToken) -> axum::Router {
    let origins: Vec<axum::http::HeaderValue> = ALLOWED_ORIGINS
        .iter()
        .map(|o| {
            o.parse()
                .expect("список origin'ов задан константами и разбирается всегда")
        })
        .collect();

    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(AllowMethods::list([
            axum::http::Method::GET,
            axum::http::Method::POST,
        ]))
        .allow_headers(AllowHeaders::list([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ]));

    axum::Router::new()
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/:id", get(get_task))
        .route("/tasks/:id/continue", post(continue_task))
        .route("/tasks/:id/pause", post(pause_task))
        .route("/tasks/:id/cancel", post(cancel_task))
        .route("/tasks/:id/tool-decision", post(tool_decision))
        .route("/models", get(list_models))
        .route("/models/:name/temperature", post(set_model_temperature))
        .route("/status", get(status))
        // Порядок слоёв важен. `route_layer` с проверкой токена — ВНУТРИ,
        // CORS — СНАРУЖИ: браузер шлёт preflight `OPTIONS` без заголовка
        // `Authorization`, и отвечать на него должен CORS, а не 401.
        .route_layer(axum::middleware::from_fn_with_state(
            token,
            crate::control_auth::require_token,
        ))
        .layer(cors)
        .with_state(state)
}

/// Запускает Control Center API на указанном адресе.
/// Вызывается один раз из `main.rs`.
pub async fn serve(
    state: AppState,
    addr: SocketAddr,
    token: crate::control_auth::ControlToken,
) -> anyhow::Result<()> {
    let app = build_router(state, token);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Control Center API слушает на {addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("Control Center API завершил работу");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("получен сигнал завершения, инициирую graceful shutdown");
}

// --- Обработчики ---

async fn list_tasks(State(state): State<AppState>) -> Result<Json<Vec<TaskResponse>>, ApiError> {
    Ok(Json(
        state
            .task_store
            .list()
            .await?
            .into_iter()
            .map(TaskResponse::from)
            .collect(),
    ))
}

/// Тело запроса на создание новой задачи.
/// `category` — необязательное явное указание (override от Agent'а,
/// см. router.rs); если не задано, Router определяет категорию сам по `text`.
/// `allow_manual` — разрешает выбрать модели уровня `manual_only`
/// (например Qwen40B); по умолчанию `false`, должно включаться только
/// явным действием пользователя в интерфейсе ("максимальное качество").
#[derive(Deserialize)]
struct CreateTaskRequest {
    text: String,
    category: Option<String>,
    allow_manual: Option<bool>,
    /// Ярлык модели из Resource Registry; если не задан, выбор остаётся авто.
    model: Option<String>,
}

async fn create_task(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<Json<TaskResponse>, ApiError> {
    let category = match &body.category {
        Some(c) => c.clone(),
        None => {
            let metadata = TaskMetadata::default();
            TaskRouter::classify(&body.text, &metadata)
                .as_str()
                .to_string()
        }
    };
    let allow_manual = body.allow_manual.unwrap_or(false);
    let model_override =
        validate_model_override(state.resource_registry.as_ref(), body.model.as_deref())
            .map_err(ApiError::BadRequest);
    let model_override = model_override?;

    let task = state.task_store.create(&category).await?;
    state
        .task_store
        .append_context(
            task.id,
            ContextEntry {
                role: "user".to_string(),
                content: body.text,
                at: Utc::now(),
                tool_call_id: None,
            },
        )
        .await?;

    let refreshed = state.task_store.get(task.id).await?.ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("задача исчезла сразу после создания"))
    })?;

    // Текст задачи в лог НЕ идёт: он приходит в том числе из Telegram и
    // может содержать личные данные. В логе только идентификатор и категория.
    tracing::info!(
        "задача {} создана: категория '{}'{}",
        task.id,
        category,
        if body.category.is_some() {
            ""
        } else {
            " (определена Router'ом)"
        }
    );

    // Задача уже сохранена в статусе Running — Electron получает её сразу
    // и дальше следит за изменениями через GET /tasks/{id}. Сама работа
    // (подбор модели + генерация) идёт в фоне, а не блокирует этот запрос:
    // для L4/L5 моделей она может занять от секунд до минут, и Task Store
    // как раз для этого и существует.
    tokio::spawn(run_task_pipeline_with_model(
        state.task_store.clone(),
        state.scheduler.clone(),
        state.control.clone(),
        task.id,
        category,
        model_override,
        context_to_messages(&refreshed.context),
        allow_manual,
    ));

    Ok(Json(TaskResponse::from(refreshed)))
}

/// Результат одного синхронного хода терминального клиента.
///
/// Терминал получает данные из того же Task Store, что HTTP и Telegram, а не
/// из отдельного сокращённого agent loop. Поэтому статус и ответ здесь —
/// фактический результат `run_task_pipeline`.
pub struct TerminalTurn {
    pub task_id: Uuid,
    pub status: TaskStatus,
    pub reply: Option<String>,
    pub detail: Option<String>,
}

/// Результат ожидания terminal turn вместе с сигналом завершения.
pub enum TerminalTurnOutcome {
    Completed(TerminalTurn),
    Interrupted { task_id: Uuid },
}

/// Обычный программный вызов terminal turn без внешнего прерывания.
pub async fn run_terminal_turn(
    state: &AppState,
    task_id: Option<Uuid>,
    text: &str,
    allow_manual: bool,
) -> anyhow::Result<TerminalTurn> {
    match run_terminal_turn_interruptible(
        state,
        task_id,
        text,
        allow_manual,
        std::future::pending::<std::io::Result<()>>(),
    )
    .await?
    {
        TerminalTurnOutcome::Completed(turn) => Ok(turn),
        TerminalTurnOutcome::Interrupted { .. } => {
            unreachable!("pending interrupt future cannot complete")
        }
    }
}

/// Выполняет первый или последующий ход терминального диалога через штатный
/// pipeline. Pending tool approval нельзя обойти новой пользовательской
/// репликой: до явного решения задача остаётся `WaitingForHuman`.
///
/// Если `interrupt` срабатывает во время model/tool pipeline, задача сначала
/// сохраняется как `Cancelled`, затем штатный [`TaskControl`] будит cooperative
/// cancellation branch. Функция дожидается её cleanup и снимает регистрацию
/// до возврата вызывающему коду. Уже отправленный внешнему MCP-серверу вызов
/// нельзя транзакционно откатить; отмена гарантирует состояние KAIC, а не
/// rollback внешнего побочного эффекта.
pub async fn run_terminal_turn_interruptible<F>(
    state: &AppState,
    task_id: Option<Uuid>,
    text: &str,
    allow_manual: bool,
    interrupt: F,
) -> anyhow::Result<TerminalTurnOutcome>
where
    F: Future<Output = std::io::Result<()>>,
{
    run_terminal_turn_interruptible_with_model(state, task_id, text, None, allow_manual, interrupt)
        .await
}

pub async fn run_terminal_turn_interruptible_with_model<F>(
    state: &AppState,
    task_id: Option<Uuid>,
    text: &str,
    model_override: Option<&str>,
    allow_manual: bool,
    interrupt: F,
) -> anyhow::Result<TerminalTurnOutcome>
where
    F: Future<Output = std::io::Result<()>>,
{
    if text.trim().is_empty() {
        anyhow::bail!("пустой terminal-запрос не отправлен");
    }

    let (task_id, category, stored, mut cancel_rx) = match task_id {
        Some(id) => {
            let task = state
                .task_store
                .get(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("terminal-задача {id} не найдена"))?;
            if task.status == TaskStatus::WaitingForHuman {
                anyhow::bail!(
                    "terminal-задача {id} ожидает решения по инструменту; новая реплика не отправлена"
                );
            }
            let cancel_rx = state
                .control
                .try_register(id)
                .await
                .ok_or_else(|| anyhow::anyhow!("terminal-задача {id} уже выполняется"))?;
            let category = task.category;
            let stored = match state
                .task_store
                .append_context_and_set_status(
                    id,
                    ContextEntry {
                        role: "user".to_string(),
                        content: text.to_string(),
                        at: Utc::now(),
                        tool_call_id: None,
                    },
                    TaskStatus::Running,
                )
                .await
            {
                Ok(stored) => stored,
                Err(error) => {
                    state.control.unregister(id).await;
                    return Err(error);
                }
            };
            (id, category, stored, cancel_rx)
        }
        None => {
            let category = TaskRouter::classify(text, &TaskMetadata::default())
                .as_str()
                .to_string();
            let stored = state
                .task_store
                .create_with_context(
                    &category,
                    ContextEntry {
                        role: "user".to_string(),
                        content: text.to_string(),
                        at: Utc::now(),
                        tool_call_id: None,
                    },
                )
                .await?;
            let cancel_rx = state.control.try_register(stored.id).await.ok_or_else(|| {
                anyhow::anyhow!("новая terminal-задача неожиданно уже выполняется")
            })?;
            (stored.id, category, stored, cancel_rx)
        }
    };

    // Terminal ждёт и pipeline, и Ctrl+C одновременно. Регистрация сделана
    // до follow-up mutation и до select, поэтому конкурентный caller не может
    // заменить sender или дописать второй пользовательский ход.
    let pipeline = run_task_pipeline_inner(
        &state.task_store,
        &state.scheduler,
        &state.control,
        &mut cancel_rx,
        task_id,
        category,
        model_override.map(str::to_owned),
        context_to_messages(&stored.context),
        allow_manual,
    );
    tokio::pin!(pipeline);
    tokio::pin!(interrupt);

    let signal_result = tokio::select! {
        biased;
        signal = &mut interrupt => Some(signal),
        _ = &mut pipeline => None,
    };

    if let Some(signal_result) = signal_result {
        // Сначала фиксируем статус, затем будим тот же cooperative cancellation
        // path, который использует Control Center. Pipeline дожидается своей
        // cancellation branch, поэтому cleanup не зависит от drop родителя.
        let status_result = match &signal_result {
            Ok(()) => {
                state
                    .task_store
                    .set_status(task_id, TaskStatus::Cancelled)
                    .await
            }
            Err(error) => state
                .task_store
                .append_context_and_transition_if_running(
                    task_id,
                    ContextEntry {
                        role: "system".to_string(),
                        content: format!("Ошибка обработчика Ctrl+C: {error}"),
                        at: Utc::now(),
                        tool_call_id: None,
                    },
                    TaskStatus::Failed,
                )
                .await
                .map(|_| ()),
        };
        let _ = state.control.cancel(task_id).await;
        pipeline.as_mut().await;
        state.control.unregister(task_id).await;
        status_result?;

        return match signal_result {
            Ok(()) => Ok(TerminalTurnOutcome::Interrupted { task_id }),
            Err(error) => Err(error.into()),
        };
    }

    state.control.unregister(task_id).await;

    let finished = state
        .task_store
        .get(task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("terminal-задача исчезла после выполнения"))?;
    let reply = finished
        .context
        .iter()
        .rev()
        .find(|entry| entry.role == "assistant")
        .map(|entry| entry.content.clone());

    let detail = match finished.status {
        TaskStatus::Failed | TaskStatus::WaitingForHuman => finished
            .context
            .iter()
            .rev()
            .find(|entry| entry.role == "system")
            .map(|entry| entry.content.clone()),
        _ => None,
    };

    Ok(TerminalTurnOutcome::Completed(TerminalTurn {
        task_id: finished.id,
        status: finished.status,
        reply,
        detail,
    }))
}

// --- Медиа-пайплайн (Category::Video) ----------------------------------------

/// Лицензии, которые запрашиваются у источника.
///
/// Дублированием политики гейта это не является: здесь фильтр нужен, чтобы
/// заведомо негодные ассеты не приезжали вовсе, а решение всё равно принимает
/// `media::gate::admit` — он же отвергнет всё, что просочилось.
const REQUESTED_LICENSES: &[&str] = &["cc0", "pdm", "by"];

/// Сколько кандидатов запрашивать у источника за один проход.
const SEARCH_PAGE_SIZE: u8 = 5;

/// Роль записи контекста, несущей путь к готовому файлу-результату.
/// Клиенты (Telegram Bridge) читают её напрямую и не разбирают текст сводки.
pub const ARTIFACT_ROLE: &str = "artifact";

/// Роль записи контекста, несущей результат вызова инструмента.
///
/// Отдельная от `assistant` намеренно: результат инструмента — не ответ
/// модели, и Telegram-мост, читающий последнюю запись роли `assistant`,
/// не должен принять его за ответ человеку.
pub const TOOL_ROLE: &str = "tool";

/// Роль записи, несущей ЗАПРОС модели на вызов инструмента.
///
/// Отдельная от `assistant`: это не ответ человеку, а намерение, которое
/// ещё может быть отклонено. Содержимое — JSON `{name, arguments}`, вместе с
/// `tool_call_id` его хватает, чтобы восстановить историю для модели после
/// паузы на подтверждение.
pub const TOOL_REQUEST_ROLE: &str = "tool_request";

/// Роль записи, фиксирующей решение человека «да» по конкретному вызову.
///
/// Существует, потому что решение и исполнение разнесены во времени и по
/// процессам: маршрут подтверждения не держит `GenerationGuard` и исполнять
/// не вправе — иначе модель вытеснили бы из-под задачи. Отметка переживает
/// паузу и говорит возобновлённому пайплайну, что вызов разрешён.
///
/// Это НЕ обход гейта: отметка — свидетельство, а `ApprovedToolCall`
/// по-прежнему собирается только в `tool_gate::approve_by_human`.
pub const TOOL_APPROVAL_ROLE: &str = "tool_approval";

/// Роль персистентной записи фактического backend/model выбора.
pub const EXECUTION_METADATA_ROLE: &str = "execution_metadata";

/// Структурированная provenance-информация для API-клиентов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionMetadata {
    pub backend: String,
    pub model: String,
}

/// Публичное представление задачи. Хранилище остаётся неизменным: provenance
/// вычисляется из уже существующей записи контекста при сериализации ответа.
#[derive(Debug, Clone, Serialize)]
pub struct TaskResponse {
    #[serde(flatten)]
    pub task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionMetadata>,
}

impl From<Task> for TaskResponse {
    fn from(task: Task) -> Self {
        let execution = execution_metadata(&task.context);
        Self { task, execution }
    }
}

/// Разбирает legacy-формат `backend=<name>; model=<name>` без влияния на
/// старые/повреждённые записи: неполная запись просто не попадает в API.
fn execution_metadata(context: &[ContextEntry]) -> Option<ExecutionMetadata> {
    let entry = context
        .iter()
        .rev()
        .find(|entry| entry.role == EXECUTION_METADATA_ROLE)?;
    let mut backend = None;
    let mut model = None;
    for field in entry.content.split(';') {
        let (key, value) = field.split_once('=')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match key.trim() {
            "backend" => backend = Some(value.to_string()),
            "model" => model = Some(value.to_string()),
            _ => {}
        }
    }
    Some(ExecutionMetadata {
        backend: backend?,
        model: model?,
    })
}

/// Куда складываются скачанные ассеты.
///
/// Рядом с журналом отправок Telegram (`.kaic/`), по задаче на подкаталог:
/// файлы одной задачи не смешиваются с другой, а весь кэш удаляется одной
/// директорией. Временный каталог ОС не подходит — материалы должны пережить
/// перезапуск, иначе план будет ссылаться в никуда.
fn media_cache_dir(task_id: Uuid) -> std::path::PathBuf {
    std::path::PathBuf::from(".kaic")
        .join("media-cache")
        .join(task_id.to_string())
}

/// Куда кладётся готовое видео. Отдельно от кэша исходников: результат и
/// материалы имеют разный срок жизни — кэш можно снести, результат нужен.
fn media_render_dir(task_id: Uuid) -> std::path::PathBuf {
    std::path::PathBuf::from(".kaic")
        .join("renders")
        .join(task_id.to_string())
}

/// Сборка плана из записей манифеста. Чистая функция: ни сети, ни Task Store —
/// поэтому отказ гейта проверяется тестом без обращения к Openverse.
///
/// Возвращает план и список причин отказа по каждому непринятому ассету:
/// отказы не проглатываются, они попадают в контекст задачи.
fn admit_all(
    entries: Vec<ManifestEntry>,
) -> Result<(Manifest, Vec<AdmittedAsset>, Vec<String>), String> {
    if entries.is_empty() {
        return Err("источник не вернул ни одного ассета".to_string());
    }

    let mut manifest = Manifest::new();
    let mut asset_ids = Vec::new();
    for entry in entries {
        asset_ids.push(entry.asset_id.clone());
        manifest.insert(entry);
    }

    let mut admitted = Vec::new();
    let mut rejections = Vec::new();
    for asset_id in &asset_ids {
        match admit(&manifest, asset_id) {
            Ok(asset) => admitted.push(asset),
            // Отказ гейта — ожидаемый исход, а не сбой: непригодный ассет
            // просто не попадает на таймлайн, задача продолжается.
            Err(rejection) => rejections.push(rejection.to_string()),
        }
    }

    if admitted.is_empty() {
        return Err(format!(
            "гейт не допустил ни одного ассета из {}: {}",
            asset_ids.len(),
            rejections.join("; ")
        ));
    }

    Ok((manifest, admitted, rejections))
}

/// План строится из тех ассетов, что реально дошли до диска.
///
/// Порядок «гейт → загрузка → план», а не «гейт → план → загрузка», выбран
/// сознательно: план и титры обязаны описывать то, что есть на самом деле.
/// Если ассет не скачался, его строка в титрах была бы ложью, а его ID в
/// плане — ссылкой в никуда.
fn plan_from_admitted(admitted: Vec<AdmittedAsset>) -> Result<RenderPlan, String> {
    let mut timeline = Timeline::new();
    for asset in admitted {
        timeline.push(asset);
    }
    plan_render(&timeline).map_err(|err| format!("план не собран: {err}"))
}

/// Гейт + план без загрузки. Используется тестами: они проверяют политику
/// допуска, для которой сеть не нужна.
#[cfg(test)]
fn assemble_render_plan(entries: Vec<ManifestEntry>) -> Result<(RenderPlan, Vec<String>), String> {
    let (_manifest, admitted, rejections) = admit_all(entries)?;
    let plan = plan_from_admitted(admitted)?;
    Ok((plan, rejections))
}

/// Полный проход медиа-задачи: реальный поиск → гейт → манифест → план рендера.
///
/// ffmpeg здесь не запускается: план остаётся данными. Исполнение рендера —
/// отдельная задача.
async fn run_video_pipeline(
    task_store: Arc<TaskStore>,
    scheduler: Arc<Scheduler>,
    task_id: Uuid,
    messages: &[Message],
    model_override: Option<&str>,
) {
    let Some(raw_text) = media_query(messages) else {
        fail_task(
            &task_store,
            task_id,
            "в задаче нет пользовательского текста для поиска материалов".to_string(),
        )
        .await;
        return;
    };

    let query = match extract_search_term(
        &scheduler,
        &task_store,
        task_id,
        &raw_text,
        model_override,
    )
    .await
    {
        Ok(query) => query,
        Err(error) => {
            fail_task(
                &task_store,
                task_id,
                format!("model step failed: {error:#}"),
            )
            .await;
            return;
        }
    };

    note(
        &task_store,
        task_id,
        format!("медиа-пайплайн: ищу материалы в Openverse по запросу «{query}»"),
    )
    .await;

    // Один клиент на весь проход: и поиск в Openverse, и скачивание
    // ассетов. Без таймаута зависший источник оставлял бы задачу в
    // InProgress навсегда — статуса «повисла» в системе нет.
    let client = crate::http_client::build(
        crate::http_client::CONNECT_TIMEOUT,
        crate::http_client::MEDIA_TIMEOUT,
    );

    // Источник требует совпадения ВСЕХ слов запроса, поэтому лишнее слово не
    // уточняет выдачу, а обнуляет её. Пробуем от самого узкого варианта к
    // широкому, а в конце — исходный текст задачи дословно (прежнее поведение).
    let mut attempts = media_query_extract::narrowing_variants(&query);
    if !attempts.iter().any(|a| a == &raw_text) {
        attempts.push(raw_text.clone());
    }

    let mut entries = Vec::new();
    let mut used_query = query.clone();
    for attempt in &attempts {
        match openverse::search_images(&client, attempt, REQUESTED_LICENSES, SEARCH_PAGE_SIZE).await
        {
            Ok(found) if !found.is_empty() => {
                entries = found;
                used_query = attempt.clone();
                break;
            }
            Ok(_) => {
                note(
                    &task_store,
                    task_id,
                    format!("по запросу «{attempt}» ничего не найдено, расширяю поиск"),
                )
                .await;
            }
            Err(err) => {
                fail_task(
                    &task_store,
                    task_id,
                    format!("источник недоступен: {err:#}"),
                )
                .await;
                return;
            }
        }
    }

    note(
        &task_store,
        task_id,
        format!(
            "источник вернул {} кандидатов по запросу «{used_query}», проверяю гейтом",
            entries.len()
        ),
    )
    .await;

    let (manifest, admitted, rejections) = match admit_all(entries) {
        Ok(result) => result,
        Err(reason) => {
            fail_task(&task_store, task_id, reason).await;
            return;
        }
    };
    if !rejections.is_empty() {
        note(
            &task_store,
            task_id,
            format!("отклонено гейтом: {}", rejections.join("; ")),
        )
        .await;
    }

    // Скачиваются только допущенные гейтом ассеты: тянуть по сети то, что
    // всё равно нельзя использовать, бессмысленно.
    let target_dir = media_cache_dir(task_id);
    let mut downloaded = Vec::new();
    let mut usable = Vec::new();
    let mut fetch_failures = Vec::new();
    for asset in admitted {
        let Some(entry) = manifest.get(asset.asset_id()) else {
            continue;
        };
        match media_fetch::fetch_and_verify(&client, entry, &target_dir).await {
            Ok(file) => {
                downloaded.push(file);
                usable.push(asset);
            }
            // Сбой одного ассета не роняет задачу: причина записывается,
            // остальные продолжают путь.
            Err(err) => fetch_failures.push(err.to_string()),
        }
    }

    if !fetch_failures.is_empty() {
        note(
            &task_store,
            task_id,
            format!("не загружено: {}", fetch_failures.join("; ")),
        )
        .await;
    }
    if usable.is_empty() {
        fail_task(
            &task_store,
            task_id,
            "ни один допущенный ассет не удалось загрузить и верифицировать".to_string(),
        )
        .await;
        return;
    }

    note(
        &task_store,
        task_id,
        format!(
            "загружено и верифицировано {} из {} допущенных, каталог {}",
            downloaded.len(),
            downloaded.len() + fetch_failures.len(),
            target_dir.display()
        ),
    )
    .await;

    match plan_from_admitted(usable) {
        Ok(plan) => {
            let files = downloaded
                .iter()
                .map(|f| {
                    format!(
                        "  {} → {} ({} байт, {})",
                        f.asset_id,
                        f.path.display(),
                        f.bytes,
                        f.format.extension()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            let credits = if plan.credits_text().is_empty() {
                "(не требуются: только public domain)".to_string()
            } else {
                plan.credits_text().to_string()
            };
            // Рендер: план + локальные файлы → настоящий видеофайл.
            let render_dir = media_render_dir(task_id);
            let rendered =
                match media_render::render_slideshow(&downloaded, plan.credits_text(), &render_dir)
                    .await
                {
                    Ok(outcome) => outcome,
                    // Отказ ffmpeg — штатный Failed с причиной из stderr,
                    // а не паника: причина видна в контексте задачи.
                    Err(err) => {
                        fail_task(&task_store, task_id, format!("рендер не удался: {err}")).await;
                        return;
                    }
                };

            let summary = format!(
                "Видео готово.\nАссетов на таймлайне: {}\n\n\
                 Файлы (скачаны и верифицированы):\n{}\n\nТитры:\n{}\n\n\
                 Результат: {} ({} байт, рендер {} мс)",
                plan.asset_ids().len(),
                files,
                credits,
                rendered.path.display(),
                rendered.bytes,
                rendered.elapsed_ms
            );

            record_artifact_outcome(
                &task_store,
                task_id,
                rendered.path.to_string_lossy().to_string(),
                summary,
            )
            .await;
        }
        // Отказ гейта на всех ассетах — это Failed с внятной причиной, а не
        // паника: задача завершается штатно, причина видна в контексте.
        Err(reason) => fail_task(&task_store, task_id, reason).await,
    }
}

/// Превращает свободный текст задачи в поисковый термин силами модели.
///
/// В `auto` этот шаг деградирует к исходному тексту при недоступной extraction-
/// модели или непригодном ответе. Явно выбранная terminal-модель работает
/// fail-closed: её backend/model error возвращается пользователю, иначе `/model`
/// мог бы выглядеть успешным, хотя выбранная модель не запускалась.
async fn extract_search_term(
    scheduler: &Arc<Scheduler>,
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    raw_text: &str,
    model_override: Option<&str>,
) -> anyhow::Result<String> {
    let started = std::time::Instant::now();
    let request = media_query_extract::build_request(raw_text);
    let model = model_override.unwrap_or(media_query_extract::EXTRACTION_MODEL_LABEL);

    let raw = match scheduler.run_with_model(model, request).await {
        // Инструментов этому шагу не давали, поэтому ответ не-текстом здесь
        // невозможен. Если он всё же придёт — это отказ, а не повод
        // домысливать: шаг по построению деградирующий, берём дословный текст.
        Ok(response) => match response.text() {
            Some(text) => text.to_string(),
            None if model_override.is_some() => {
                anyhow::bail!(
                    "выбранная модель '{model}' вернула tool call вместо поискового термина"
                );
            }
            None => {
                note(
                    task_store,
                    task_id,
                    "модель запросила вызов инструмента там, где инструментов нет; \
                     ищу по тексту задачи"
                        .to_string(),
                )
                .await;
                return Ok(raw_text.to_string());
            }
        },
        Err(err) if model_override.is_some() => {
            anyhow::bail!("выбранная модель '{model}' недоступна: {err:#}");
        }
        Err(err) => {
            note(
                task_store,
                task_id,
                format!("извлечение термина не выполнено ({err:#}); ищу по тексту задачи"),
            )
            .await;
            return Ok(raw_text.to_string());
        }
    };

    task_store
        .append_context(
            task_id,
            ContextEntry {
                role: EXECUTION_METADATA_ROLE.to_string(),
                content: format!("backend={}; model={model}", scheduler.backend_name()),
                at: Utc::now(),
                tool_call_id: None,
            },
        )
        .await?;

    match media_query_extract::sanitize(&raw) {
        Some(term) => {
            note(
                task_store,
                task_id,
                format!(
                    "поисковый термин: «{term}» (из «{raw_text}», {} мс)",
                    started.elapsed().as_millis()
                ),
            )
            .await;
            Ok(term)
        }
        None if model_override.is_some() => {
            anyhow::bail!("выбранная модель '{model}' не дала пригодный поисковый термин");
        }
        None => {
            note(
                task_store,
                task_id,
                format!("модель не дала пригодный термин; ищу по тексту задачи «{raw_text}»"),
            )
            .await;
            Ok(raw_text.to_string())
        }
    }
}

/// Поисковый термин для медиа-задачи.
///
/// Берётся ПЕРВОЕ пользовательское сообщение задачи как есть, дословно.
/// Никакого разбора, выделения ключевых слов или интерпретации намерения
/// здесь нет и быть не должно: как свободный текст превращается в
/// структурированный медиа-запрос (термин + тип операции) — не решённый
/// вопрос, и решать его молча в точке подключения нельзя.
fn media_query(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| m.content.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Дописывает пояснение в контекст задачи, не меняя статус.
async fn note(task_store: &Arc<TaskStore>, task_id: Uuid, content: String) {
    if let Err(err) = task_store
        .append_context(
            task_id,
            ContextEntry {
                role: "system".to_string(),
                content,
                at: Utc::now(),
                tool_call_id: None,
            },
        )
        .await
    {
        tracing::error!("не удалось записать заметку задачи {task_id}: {err:#}");
    }
}

async fn fail_task(task_store: &Arc<TaskStore>, task_id: Uuid, reason: String) {
    tracing::warn!("медиа-задача {task_id} не выполнена: {reason}");
    note(task_store, task_id, format!("Ошибка: {reason}")).await;
    if !claim_finish(task_store, task_id, TaskStatus::Failed).await {
        tracing::warn!(
            "задача {task_id}: отказ пришёл после того, как статус сменил человек — \
             отмена не переписана на Failed"
        );
    }
}

/// Записывает итоговый статус фоновой работы, не затирая решение человека.
///
/// `true` — статус записан; `false` — задача уже не выполняется, потому что
/// её отменили, приостановили или продолжили, пока фон работал.
///
/// Ошибка записи возвращает `false` намеренно: неизвестно, применилась она
/// или нет, и считать её успехом значило бы дать вызывающему записать
/// результат ролью `assistant` под непонятным статусом.
async fn claim_finish(task_store: &Arc<TaskStore>, task_id: Uuid, status: TaskStatus) -> bool {
    match task_store.finish_if_still_running(task_id, status).await {
        Ok(applied) => applied,
        Err(err) => {
            tracing::error!("не удалось обновить статус задачи {task_id}: {err:#}");
            false
        }
    }
}

/// Атомарно публикует Video artifact, human summary и `Done`.
/// Если cancellation уже победил, машиночитаемый artifact не записывается,
/// а поздняя сводка сохраняется только диагностической `system`-заметкой.
async fn record_artifact_outcome(
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    artifact_path: String,
    summary: String,
) {
    let now = Utc::now();
    let entries = vec![
        ContextEntry {
            role: ARTIFACT_ROLE.to_string(),
            content: artifact_path,
            at: now,
            tool_call_id: None,
        },
        ContextEntry {
            role: "assistant".to_string(),
            content: summary.clone(),
            at: now,
            tool_call_id: None,
        },
    ];
    match task_store
        .append_contexts_and_transition_if_running(task_id, entries, TaskStatus::Done)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                "задача {task_id}: video result завершился после cancellation; artifact не опубликован"
            );
            let late = ContextEntry {
                role: "system".to_string(),
                content: format!(
                    "Работа завершилась уже после того, как задача была отменена или изменена. \
                     Artifact не опубликован. Ответом задачи этот текст не является:\n\n{summary}"
                ),
                at: Utc::now(),
                tool_call_id: None,
            };
            if let Err(err) = task_store.append_context(task_id, late).await {
                tracing::error!("не удалось сохранить поздний video result {task_id}: {err:#}");
            }
        }
        Err(err) => {
            tracing::error!("не удалось атомарно сохранить video result {task_id}: {err:#}");
        }
    }
}

/// Кладёт обычный текстовый результат ответом, если задача его ещё ждёт,
/// либо диагностической `system`-заметкой после cancellation.
async fn record_outcome(
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    status: TaskStatus,
    content: String,
) {
    let entry = if claim_finish(task_store, task_id, status).await {
        ContextEntry {
            role: "assistant".to_string(),
            content,
            at: Utc::now(),
            tool_call_id: None,
        }
    } else {
        tracing::warn!(
            "задача {task_id}: работа завершилась после того, как статус сменил человек — \
             результат сохранён пометкой, ответом не считается"
        );
        ContextEntry {
            role: "system".to_string(),
            content: format!(
                "Работа завершилась уже после того, как задача была отменена или изменена. \
                 Ответом задачи этот текст не является:\n\n{content}"
            ),
            at: Utc::now(),
            tool_call_id: None,
        }
    };

    if let Err(err) = task_store.append_context(task_id, entry).await {
        tracing::error!("не удалось сохранить результат задачи {task_id}: {err:#}");
    }
}

/// Переводит накопленный контекст задачи в историю сообщений для модели.
/// Роли, не относящиеся напрямую к диалогу (например "system" — заметки
/// об ошибках), сопоставляются с `Role::System`.
fn context_to_messages(context: &[ContextEntry]) -> Vec<Message> {
    context
        .iter()
        .filter(|entry| entry.role != EXECUTION_METADATA_ROLE)
        .map(|entry| match entry.role.as_str() {
            "user" => Message::new(Role::User, entry.content.clone()),
            "assistant" => Message::new(Role::Assistant, entry.content.clone()),
            // Результат инструмента обязан вернуться модели со СВОИМ
            // `tool_call_id`: без него провайдер не сопоставит его с вызовом
            // и отвергнет всю историю. Записи роли `tool` без идентификатора
            // в базе быть не может — его пишет тот же цикл, что и запись, —
            // но если она найдётся (старая задача, ручная правка), считаем её
            // обычной заметкой, а не притворяемся, что связь есть.
            "tool" => match &entry.tool_call_id {
                Some(id) => Message::tool_result(id.clone(), entry.content.clone()),
                None => Message::new(Role::System, entry.content.clone()),
            },
            // Запрос вызова восстанавливается в реплику ассистента с
            // `tool_calls` — без неё следующий за ней результат роли `tool`
            // повис бы без своего запроса, и провайдер отверг бы историю.
            TOOL_REQUEST_ROLE => match (&entry.tool_call_id, parse_tool_request(&entry.content)) {
                (Some(id), Some((name, arguments))) => {
                    Message::assistant_tool_calls(vec![crate::model_backend::ToolCall {
                        id: id.clone(),
                        name,
                        arguments,
                    }])
                }
                // Битая запись не притворяется вызовом: лучше заметка, чем
                // история, которую провайдер отвергнет целиком.
                _ => Message::new(Role::System, entry.content.clone()),
            },
            _ => Message::new(Role::System, entry.content.clone()),
        })
        .collect()
}

/// Инструкции, которые входят в каждый локальный agent session по умолчанию.
/// Они дополняют внешний skill `local-agent-execution`: модель должна реально
/// использовать встроенные workspace tools, а не описывать воображаемый результат.
const DEFAULT_LOCAL_AGENT_EXECUTION_INSTRUCTIONS: &str = r#"Режим локального выполнения:
- Если задача требует создать или изменить файл, реально используй доступные workspace tools. Не выдавай код в ответе вместо записи на диск.
- Работай по абсолютным или относительным путям, но только вне защищённых системных и секретных каталогов. Не используй `..` и symlink-пути. Если пользователь назвал абсолютный путь, передай его в `path` без изменений: сохрани начальный символ `/`; `home/...` и `/home/...` — разные пути.
- Для нового файла сначала создай каталог при необходимости, затем создай файл эксклюзивно; не перезаписывай существующие файлы без отдельного разрешения.
- После создания нового файла перечитай точный целевой файл через `workspace_read_text_file` и проверь содержимое. После создания каталога вызови `workspace_inspect_path`; не пытайся читать каталог как текст.
- После изменения выполни релевантный тест, сборку или smoke-проверку.
- Продолжай работу до достижения цели. Не сообщай «создано», «исправлено» или «готово» без подтверждённого read-back и результата проверки.
- Если нужного инструмента нет или вызов заблокирован, честно сообщи точный блокер и не симулируй выполнение.
- Не удаляй, не перемещай и не перезаписывай пользовательские файлы без явного разрешения. Не сохраняй секреты; используй [REDACTED]."#;

fn add_default_local_agent_instructions(messages: &mut Vec<Message>) {
    messages.insert(
        0,
        Message::new(Role::System, DEFAULT_LOCAL_AGENT_EXECUTION_INSTRUCTIONS),
    );
}

/// Предел шагов цикла «модель → инструмент → модель» на одну задачу.
///
/// Восемь, и число не круглое ради красоты. Замер вызова инструментов
/// (2026-08-13) показал, что модели уверенно держат двухшаговые цепочки:
/// GPTOSS20 — 3/3 за 7.1–7.9 с, Gemma27 — 3/3 за 30.1–39.3 с. Трёх- и
/// более шаговых цепочек никто не мерил, то есть верхней границы «сколько
/// модель осмысленно пройдёт» мы не знаем. Восемь — это вчетверо больше
/// измеренного и при этом заведомо конечное число: на Gemma27 худший случай
/// упирается примерно в пять минут, что укладывается в таймаут HTTP-клиента
/// (1800 с) с запасом.
///
/// Предел нужен не для экономии, а потому что зацикливание модели — это
/// наблюдаемое поведение, а не гипотеза: при исчерпании задача обязана
/// упасть с внятной причиной, а не крутиться, пока кто-нибудь не заметит.
const MAX_TOOL_ITERATIONS: usize = 8;

/// Где лежит описание MCP-серверов. Файла нет — инструментов нет, и весь
/// контур ведёт себя ровно так, как до их появления.
const MCP_CONFIG_PATH: &str = "config/mcp.yaml";

/// Цикл «модель → инструмент → модель».
///
/// Возвращает финальный текст ответа. Инструменты берутся у MCP-сервера,
/// поднятого на время задачи; если сервера нет, список пуст, и цикл
/// вырождается в один запрос — то самое поведение, что было до этой задачи.
async fn run_agent_loop(
    task_store: &Arc<TaskStore>,
    scheduler: &Arc<Scheduler>,
    task_id: Uuid,
    category: &str,
    model_override: Option<&str>,
    allow_manual: bool,
    mut messages: Vec<Message>,
) -> anyhow::Result<LoopOutcome> {
    add_default_local_agent_instructions(&mut messages);
    let config = McpServerConfig::load(MCP_CONFIG_PATH)?;
    // MCP keeps its explicit approval policy. Built-in workspace tools are
    // separately auto-approved because their implementation is capability-
    // sandboxed, creates only new entries, and cannot overwrite or delete.
    let mut auto_approve = config
        .as_ref()
        .map(|c| c.auto_approve.clone())
        .unwrap_or_default();
    for name in crate::workspace_tools::AUTO_APPROVED_TOOLS {
        if !auto_approve.iter().any(|approved| approved == name) {
            auto_approve.push((*name).to_string());
        }
    }

    let workspace = crate::workspace_tools::WorkspaceTools::new_anywhere()?;

    // Сервер поднимается ДО занятия модели: если он не встанет, незачем
    // держать модель занятой всё время его падения.
    let mcp = match config {
        Some(config) => match McpClient::connect(&config).await {
            Ok(client) => Some(client),
            // Недоступный сервер инструментов — не причина ронять задачу:
            // модель ответит текстом, как отвечала раньше. Но молчать об
            // этом нельзя, иначе «модель не воспользовалась инструментом»
            // не отличить от «инструментов ей не дали».
            Err(err) => {
                note(
                    task_store,
                    task_id,
                    format!("MCP-сервер недоступен ({err:#}); работаю без инструментов"),
                )
                .await;
                None
            }
        },
        None => None,
    };

    let server_name = mcp
        .as_ref()
        .map(|c| c.server_name().to_string())
        .unwrap_or_else(|| "-".to_string());

    let mut tools = crate::workspace_tools::specs();
    if let Some(client) = &mcp {
        let mcp_tools = client.list_tools().await?;
        note(
            task_store,
            task_id,
            format!(
                "встроенная рабочая область: {}; MCP-инструменты от '{}': {}. Без подтверждения разрешены: {}",
                crate::workspace_tools::configured_root().display(),
                client.server_name(),
                mcp_tools
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                auto_approve.join(", ")
            ),
        )
        .await;
        // Built-ins own their reserved names; an MCP server cannot shadow
        // them and redirect a capability-approved call outside the sandbox.
        for tool in mcp_tools {
            if !crate::workspace_tools::is_builtin(&tool.name) {
                tools.push(tool);
            }
        }
    } else {
        note(
            task_store,
            task_id,
            format!(
                "доступны встроенные инструменты по файловой системе / (защищённые системные каталоги исключены): {}",
                crate::workspace_tools::AUTO_APPROVED_TOOLS.join(", ")
            ),
        )
        .await;
    }

    // Сессия, а не `run`: guard обязан пережить все шаги цикла, включая
    // паузы на работу инструмента, иначе модель вытеснят между шагами.
    let session = match model_override {
        Some(model) => scheduler.begin_session_with_model(model).await?,
        None => scheduler.begin_session(category, allow_manual).await?,
    };

    // Записываем фактический выбор после всех fallback-правил Scheduler'а.
    task_store
        .append_context(
            task_id,
            ContextEntry {
                role: EXECUTION_METADATA_ROLE.to_string(),
                content: format!(
                    "backend={}; model={}",
                    scheduler.backend_name(),
                    session.model()
                ),
                at: Utc::now(),
                tool_call_id: None,
            },
        )
        .await?;

    let outcome = agent_steps(
        task_store,
        &session,
        task_id,
        &mut messages,
        &tools,
        mcp.as_ref(),
        &workspace,
        &auto_approve,
        &server_name,
    )
    .await;

    // Сервер завершается в любом случае — и на успехе, и на ошибке.
    // Подпроцесс, переживший задачу, накапливался бы молча.
    if let Some(client) = mcp {
        if let Err(err) = client.shutdown().await {
            tracing::warn!("MCP-сервер задачи {task_id} завершился некорректно: {err:#}");
        }
    }
    outcome
}

/// Собственно шаги цикла — вынесены, чтобы завершение MCP-сервера выше
/// выполнялось на любом исходе, а не только на успешном.
async fn agent_steps(
    task_store: &Arc<TaskStore>,
    session: &crate::scheduler::Session<'_>,
    task_id: Uuid,
    messages: &mut Vec<Message>,
    tools: &[crate::model_backend::ToolSpec],
    mcp: Option<&McpClient>,
    workspace: &crate::workspace_tools::WorkspaceTools,
    auto_approve: &[String],
    server_name: &str,
) -> anyhow::Result<LoopOutcome> {
    // Возобновление после «да»: подтверждённый вызов исполняется ДО того, как
    // модель спросят снова. Иначе она увидела бы собственный запрос без
    // результата и либо повторила бы его, либо получила бы отвергнутую
    // провайдером историю.
    if let Some(call) = approved_pending_call(
        &task_store
            .get(task_id)
            .await?
            .map(|t| t.context)
            .unwrap_or_default(),
    ) {
        // Единственный путь человека к `ApprovedToolCall`.
        let approved = crate::tool_gate::approve_by_human(call);
        let result = execute_tool(task_store, task_id, mcp, workspace, &approved, 0).await;
        messages.push(Message::tool_result(approved.call().id.clone(), result));
    }

    for step in 1..=MAX_TOOL_ITERATIONS {
        // История копируется, а не отдаётся: следующий шаг должен видеть всё
        // накопленное, включая собственные вызовы инструментов и их
        // результаты. Без полной истории модель на втором шаге не помнит, что
        // сама же и запрашивала.
        let request = GenerateRequest {
            messages: messages.clone(),
            // temperature подставит Scheduler: только он знает, какая модель
            // в итоге взяла задачу.
            temperature: None,
            tools: tools.to_vec(),
        };

        match session.generate(request).await? {
            GenerateResponse::Text(text) => {
                if text.trim().is_empty() {
                    anyhow::bail!("модель вернула пустой текст");
                }
                return Ok(LoopOutcome::Answer(text));
            }
            GenerateResponse::ToolCalls(calls) => {
                for call in &calls {
                    if !tools.iter().any(|tool| tool.name == call.name) {
                        anyhow::bail!(
                            "модель запросила невыданный инструмент '{}' — вызов отклонён",
                            call.name
                        );
                    }
                }
                // Реплика ассистента с вызовами обязана лечь в историю ПЕРЕД
                // результатами: схема требует, чтобы у каждого сообщения роли
                // `tool` был предшествующий запрос с тем же идентификатором.
                messages.push(Message::assistant_tool_calls(calls.clone()));
                // И в СОХРАНЁННЫЙ контекст тоже: задача может встать на
                // паузу ради подтверждения и продолжиться в другом процессе,
                // а история для модели восстанавливается из хранилища.
                for call in &calls {
                    record_tool_request(task_store, task_id, call).await;
                }

                for call in calls {
                    match crate::tool_gate::decide(auto_approve, call) {
                        crate::tool_gate::Decision::Approved(approved) => {
                            let result =
                                execute_tool(task_store, task_id, mcp, workspace, &approved, step)
                                    .await;
                            messages.push(Message::tool_result(approved.call().id.clone(), result));
                        }
                        // Разрешения нет — исполнять нечего и нечем: сырой
                        // вызов в точку исполнения не подходит по типу.
                        // Задача встаёт на паузу и ждёт человека.
                        crate::tool_gate::Decision::NeedsHuman(call) => {
                            park_for_approval(task_store, task_id, server_name, &call).await?;
                            return Ok(LoopOutcome::AwaitingHuman);
                        }
                    }
                }
            }
        }
    }

    anyhow::bail!(
        "модель не завершила задачу за {MAX_TOOL_ITERATIONS} шагов вызова инструментов \
         на модели '{}' — цикл прерван, чтобы не крутиться бесконечно",
        session.model()
    )
}

/// Разбирает содержимое записи роли `tool_request` обратно в имя и аргументы.
fn parse_tool_request(content: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    let arguments = value.get("arguments")?.as_str()?.to_string();
    Some((name, arguments))
}

/// Последний запрос вызова, ожидающий решения человека.
///
/// Ищется с конца: если задача успела попросить подтверждения несколько раз,
/// решается последний. Считается ожидающим, только если ПОСЛЕ него нет
/// записи роли `tool` с тем же идентификатором — то есть на него ещё не
/// ответили ни исполнением, ни отказом.
fn pending_tool_request(context: &[ContextEntry]) -> Option<crate::model_backend::ToolCall> {
    let request = context.iter().rev().find(|e| e.role == TOOL_REQUEST_ROLE)?;
    let id = request.tool_call_id.clone()?;
    let already_answered = context
        .iter()
        .any(|e| e.role == TOOL_ROLE && e.tool_call_id.as_deref() == Some(id.as_str()));
    if already_answered {
        return None;
    }
    let (name, arguments) = parse_tool_request(&request.content)?;
    Some(crate::model_backend::ToolCall {
        id,
        name,
        arguments,
    })
}

/// Вызов, который человек подтвердил, но который ещё не исполнен.
///
/// Ищется по трём условиям сразу: есть запрос, есть отметка подтверждения с
/// тем же идентификатором, и НЕТ результата с тем же идентификатором. Третье
/// условие существенно — без него возобновлённая задача исполняла бы один и
/// тот же подтверждённый вызов на каждом круге.
fn approved_pending_call(context: &[ContextEntry]) -> Option<crate::model_backend::ToolCall> {
    let call = pending_tool_request(context)?;
    let approved = context.iter().any(|e| {
        e.role == TOOL_APPROVAL_ROLE && e.tool_call_id.as_deref() == Some(call.id.as_str())
    });
    if approved {
        Some(call)
    } else {
        None
    }
}

/// Чем закончился проход цикла.
pub enum LoopOutcome {
    /// Модель ответила текстом — задача завершена.
    Answer(String),
    /// Цикл остановлен: нужен ответ человека на вызов инструмента.
    /// Задача уже переведена в `WaitingForHuman`, статус трогать не надо.
    AwaitingHuman,
}

/// Сохраняет в контекст задачи ЗАПРОС модели на вызов инструмента.
///
/// Нужен не для человека, а для продолжения: задача может встать на паузу и
/// возобновиться отдельным запросом, а история для модели восстанавливается
/// из хранилища. Без этой записи результат роли `tool` оказался бы ответом
/// на вызов, которого в истории нет, и провайдер отверг бы её целиком.
async fn record_tool_request(
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    call: &crate::model_backend::ToolCall,
) {
    let payload = serde_json::json!({ "name": call.name, "arguments": call.arguments });
    if let Err(err) = task_store
        .append_context(
            task_id,
            ContextEntry {
                role: TOOL_REQUEST_ROLE.to_string(),
                content: payload.to_string(),
                at: Utc::now(),
                tool_call_id: Some(call.id.clone()),
            },
        )
        .await
    {
        tracing::error!("не удалось сохранить запрос вызова {task_id}: {err:#}");
    }
}

/// Ставит задачу на паузу до решения человека.
///
/// Аргументы вызова уходят в контекст ЦЕЛИКОМ — этого требует спецификация
/// MCP («показать пользователю аргументы до отправки»), и обрезать их значило
/// бы просить решение по неполным данным.
async fn park_for_approval(
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    server_name: &str,
    call: &crate::model_backend::ToolCall,
) -> anyhow::Result<()> {
    let parked = task_store
        .append_context_and_transition_if_running(
            task_id,
            ContextEntry {
                role: "system".to_string(),
                content: crate::tool_gate::describe(server_name, call),
                at: Utc::now(),
                tool_call_id: None,
            },
            TaskStatus::WaitingForHuman,
        )
        .await?;
    if !parked {
        anyhow::bail!("задача {task_id} была остановлена до ожидания разрешения");
    }

    tracing::info!(
        "задача {task_id}: вызов '{}' ждёт решения человека ({} символов аргументов)",
        call.name,
        call.arguments.chars().count()
    );

    Ok(())
}

/// Исполняет один вызов инструмента и возвращает текст для модели.
///
/// Ошибка НЕ возвращается наружу: неудачный инструмент — это результат,
/// который модель должна увидеть и обработать, а не повод уронить задачу.
/// Модель, получившая «инструмент отказал», обычно пробует иначе; задача,
/// упавшая на первом отказе, не пробует ничего.
///
/// Принимает ТОЛЬКО [`ApprovedToolCall`], и это единственная точка
/// исполнения. Сырой `ToolCall` сюда не подходит по типу, а собрать
/// `ApprovedToolCall` вне `tool_gate` невозможно — поля приватны. Именно так
/// гарантия держится типом, а не памятью того, кто добавит следующий путь.
async fn execute_tool(
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    client: Option<&McpClient>,
    workspace: &crate::workspace_tools::WorkspaceTools,
    approved: &crate::tool_gate::ApprovedToolCall,
    step: usize,
) -> String {
    let call = approved.call();
    note(
        task_store,
        task_id,
        format!(
            "шаг {step}: вызываю '{}' (разрешено: {}) с аргументами {}",
            call.name,
            approved.approved_by().as_str(),
            call.arguments
        ),
    )
    .await;

    // Аргументы разбираются здесь, а не в `model_backend`: кривой JSON от
    // модели — штатное явление, и он должен вернуться ей текстом ошибки, а не
    // уронить разбор всего ответа провайдера.
    let arguments = if call.arguments.trim().is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_str(&call.arguments) {
            Ok(value) => value,
            Err(err) => {
                let message = format!(
                    "ОШИБКА ИНСТРУМЕНТА: аргументы не разобрались как JSON ({err}); \
                     пришло: {}",
                    call.arguments
                );
                note(task_store, task_id, message.clone()).await;
                return message;
            }
        }
    };

    let started = std::time::Instant::now();
    let tool_server;
    let call_result = if crate::workspace_tools::is_builtin(&call.name) {
        tool_server = "kaic-workspace";
        workspace.execute(&call.name, &arguments)
    } else if let Some(client) = client {
        tool_server = client.server_name();
        client.call_tool(&call.name, arguments).await
    } else {
        tool_server = "unavailable";
        Err(anyhow::anyhow!(
            "инструмент '{}' не является встроенным, а MCP-сервер недоступен",
            call.name
        ))
    };
    let (result, outcome) = match call_result {
        Ok(text) => {
            let failed = text.starts_with("ОШИБКА ИНСТРУМЕНТА");
            let outcome = if failed {
                crate::tool_gate::Outcome::Failed
            } else {
                crate::tool_gate::Outcome::Executed
            };
            (text, outcome)
        }
        Err(err) => (
            format!("ОШИБКА ИНСТРУМЕНТА: {err:#}"),
            crate::tool_gate::Outcome::Failed,
        ),
    };

    // Журнал пишется ДО записи в контекст: контекст можно перечитать и
    // истолковать, а журнал отвечает на вопрос «что система реально
    // сделала», и потерять его строку хуже, чем потерять заметку.
    if let Err(err) = crate::tool_gate::append_audit(
        crate::tool_gate::AUDIT_LOG_PATH,
        &task_id.to_string(),
        tool_server,
        call,
        Some(approved.approved_by()),
        outcome,
        &result,
    ) {
        tracing::error!("не удалось записать вызов в аудит: {err:#}");
    }

    // Результат сохраняется ролью `tool` вместе с идентификатором вызова —
    // это то, ради чего в схеме Task Store появилась колонка.
    if let Err(err) = task_store
        .append_context(
            task_id,
            ContextEntry {
                role: TOOL_ROLE.to_string(),
                content: result.clone(),
                at: Utc::now(),
                tool_call_id: Some(call.id.clone()),
            },
        )
        .await
    {
        tracing::error!("не удалось сохранить результат инструмента {task_id}: {err:#}");
    }

    tracing::info!(
        "задача {task_id}, шаг {step}: инструмент '{}' отработал за {} мс",
        call.name,
        started.elapsed().as_millis()
    );

    result
}

/// Проверяет явный выбор модели на HTTP boundary, до создания задачи.
fn validate_model_override(
    registry: &ResourceRegistry,
    model: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(model) = model else {
        return Ok(None);
    };
    let model = model.trim();
    if model.is_empty() {
        return Err("model должен быть непустым именем модели".to_string());
    }
    if registry.get(model).is_none() {
        return Err(format!("модель '{model}' отсутствует в Resource Registry"));
    }
    Ok(Some(model.to_string()))
}

/// Фоновый пайплайн: Scheduler подбирает и вызывает модель, результат
/// (или ошибка) дописывается в контекст задачи, статус обновляется.
/// Используется и при создании задачи, и при её продолжении после
/// решения пользователя — в обоих случаях на вход идёт полная накопленная
/// история сообщений, а не только последняя реплика.
/// Совместимый вызов pipeline для continuation и внутренних callers без override.
async fn run_task_pipeline(
    task_store: Arc<TaskStore>,
    scheduler: Arc<Scheduler>,
    control: TaskControl,
    task_id: Uuid,
    category: String,
    messages: Vec<Message>,
    allow_manual: bool,
) {
    run_task_pipeline_with_model(
        task_store,
        scheduler,
        control,
        task_id,
        category,
        None,
        messages,
        allow_manual,
    )
    .await;
}

async fn run_task_pipeline_with_model(
    task_store: Arc<TaskStore>,
    scheduler: Arc<Scheduler>,
    control: TaskControl,
    task_id: Uuid,
    category: String,
    model_override: Option<String>,
    messages: Vec<Message>,
    allow_manual: bool,
) {
    // Задача отмечается идущей ДО первого обращения к модели и снимается на
    // любом исходе — иначе `cancel` не нашёл бы кого останавливать, а
    // `continue` считал бы завершённую задачу вечно занятой.
    let Some(mut cancel_rx) = control.try_register(task_id).await else {
        tracing::warn!("задача {task_id}: второй pipeline не запущен — task ID уже занят");
        return;
    };
    run_task_pipeline_inner(
        &task_store,
        &scheduler,
        &control,
        &mut cancel_rx,
        task_id,
        category,
        model_override,
        messages,
        allow_manual,
    )
    .await;
    control.unregister(task_id).await;
}

/// Сколько раз подряд задача может подхватить реплику человека, пришедшую
/// во время работы.
///
/// Предел нужен по той же причине, что и `MAX_TOOL_ITERATIONS`: человек,
/// который шлёт реплики быстрее, чем модель отвечает, не должен получить
/// незавершимую задачу. Три — это «ответил, уточнили, ответил, уточнили,
/// ответил»; дальше разговор ведётся новой задачей.
const MAX_HUMAN_FOLLOWUPS: usize = 3;

#[allow(clippy::too_many_arguments)]
async fn run_task_pipeline_inner(
    task_store: &Arc<TaskStore>,
    scheduler: &Arc<Scheduler>,
    control: &TaskControl,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    task_id: Uuid,
    category: String,
    model_override: Option<String>,
    messages: Vec<Message>,
    allow_manual: bool,
) {
    let _ = control;
    // Первое и пока единственное ветвление по категории. До этого все
    // категории шли одним путём (текст → модель → текст), и Video ничем не
    // отличалась от Programming, хотя существовала в Router'е.
    //
    // Video использует отдельный медиа-пайплайн. Его единственный model-step —
    // извлечение поискового термина; explicit terminal override передаётся туда,
    // а в auto-режиме сохраняется штатная extraction-модель.
    if category == Category::Video.as_str() {
        tokio::select! {
            biased;
            _ = wait_for_cancel(cancel_rx) => {}
            _ = run_video_pipeline(
                task_store.clone(),
                scheduler.clone(),
                task_id,
                &messages,
                model_override.as_deref(),
            ) => {}
        }
        return;
    }

    let mut messages = messages;
    for followup in 0..=MAX_HUMAN_FOLLOWUPS {
        // Сколько реплик ЧЕЛОВЕКА в контексте до обращения к модели.
        //
        // Считаются только записи роли `user`, а не длина контекста целиком, и это
        // не придирка: пайплайн пишет в контекст сам — `note()` кладёт список
        // инструментов, `record_tool_request()` кладёт запрос вызова, отказ кладёт
        // сообщение роли `tool`. По длине «контекст вырос» выполнялось на любой
        // задаче с инструментами без всякого участия человека, и цикл
        // перезапускался до предела, затирая настоящий ответ пустым. Найдено
        // живым прогоном, а не модульным тестом.
        let user_turns_before = task_store
            .get(task_id)
            .await
            .ok()
            .flatten()
            .map(|t| t.context.iter().filter(|e| e.role == "user").count())
            .unwrap_or(0);

        let started = std::time::Instant::now();

        // Гонка «работа против отмены». Именно здесь чинится долг: раньше
        // `run_agent_loop` просто ожидался до конца, и слово `Cancelled` в
        // хранилище никого не касалось. Проигравшее будущее ДРОПАЕТСЯ, а вместе
        // с ним — и запрос к модели, и `Session`, держащая `GenerationGuard`.
        // Поэтому после отмены слот модели освобождается сразу, а не после
        // естественного конца генерации.
        let loop_result = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel_rx) => {
                tracing::info!(
                    "задача {task_id} остановлена человеком через {:.1} с — генерация прервана",
                    started.elapsed().as_secs_f64()
                );
                return;
            }
            outcome = run_agent_loop(
                task_store,
                scheduler,
                task_id,
                &category,
                model_override.as_deref(),
                allow_manual,
                messages.clone(),
            ) => outcome,
        };

        match loop_result {
            // Задача встала на паузу ради решения человека. Статус уже
            // `WaitingForHuman`, и трогать его нельзя: это не завершение.
            Ok(LoopOutcome::AwaitingHuman) => {
                tracing::info!(
                    "задача {task_id} ждёт решения человека по вызову инструмента ({:.1} с работы)",
                    started.elapsed().as_secs_f64()
                );
                return;
            }
            Ok(LoopOutcome::Answer(response)) => {
                // Длина ответа, а не ответ: размер полезен для диагностики,
                // содержимое в лог не идёт.
                tracing::info!(
                    "задача {task_id} завершена: done за {:.1} с, ответ {} символов",
                    started.elapsed().as_secs_f64(),
                    response.chars().count()
                );
                // Статус решается ПЕРВЫМ, до записи ответа: только он говорит,
                // является ли пришедший текст ответом задачи или результатом
                // работы, от которой человек успел отказаться. Прежде порядок
                // был обратным, и ответ ложился ролью `assistant` независимо ни
                // от чего — то есть отменённая задача получала полноценный
                // ответ, который Telegram-мост потом и отправлял.
                let user_turns_after = task_store
                    .get(task_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|t| t.context.iter().filter(|e| e.role == "user").count())
                    .unwrap_or(user_turns_before);

                // Пришла ли реплика человека, пока шла генерация? Если да —
                // задача НЕ закрывается: ответ ложится в контекст, реплика
                // подхватывается, и следующий шаг делает ТА ЖЕ задача, тем же
                // пайплайном, последовательно. Второй генерации не заводится —
                // ровно это и было дефектом: `running_tasks` уходил с 1 на 2.
                if user_turns_after > user_turns_before && followup < MAX_HUMAN_FOLLOWUPS {
                    let appended = task_store
                        .append_context(
                            task_id,
                            ContextEntry {
                                role: "assistant".to_string(),
                                content: response,
                                at: Utc::now(),
                                tool_call_id: None,
                            },
                        )
                        .await;
                    if let Err(e) = appended {
                        tracing::error!("не удалось сохранить ответ задачи {task_id}: {e:#}");
                        return;
                    }
                    let Ok(Some(refreshed)) = task_store.get(task_id).await else {
                        return;
                    };
                    tracing::info!(
                    "задача {task_id}: реплика человека пришла во время работы — продолжаю ТУ ЖЕ задачу, шаг {}",
                    followup + 2
                );
                    messages = context_to_messages(&refreshed.context);
                    continue;
                }

                record_outcome(task_store, task_id, TaskStatus::Done, response).await;
                return;
            }
            Err(err) => {
                tracing::warn!(
                    "задача {task_id} завершена: failed за {:.1} с — {err:#}",
                    started.elapsed().as_secs_f64()
                );
                // Причина пишется всегда — она объясняет, чем занималась система,
                // даже если задачу уже отменили. А вот статус Failed ставится
                // только если задача его ещё ждёт: отменённая задача не должна
                // становиться упавшей, человек её не запускал заново.
                let append_result = task_store
                    .append_context(
                        task_id,
                        ContextEntry {
                            role: "system".to_string(),
                            content: format!("Ошибка: {err:#}"),
                            at: Utc::now(),
                            tool_call_id: None,
                        },
                    )
                    .await;
                if let Err(e) = append_result {
                    tracing::error!("не удалось сохранить ошибку для задачи {task_id}: {e:#}");
                }
                if !claim_finish(task_store, task_id, TaskStatus::Failed).await {
                    tracing::warn!(
                        "задача {task_id}: отказ пришёл после того, как статус сменил человек — \
                     решение человека сохранено"
                    );
                }
                return;
            }
        }
    }
}

/// Ждёт сигнала отмены и только его.
///
/// Если отправитель исчез, это НЕ отмена: функция зависает навсегда, и в
/// `select!` побеждает работа. Иначе снятие задачи с учёта выглядело бы
/// как отмена и обрывало бы уже завершающуюся генерацию.
async fn wait_for_cancel(rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await
}

async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TaskResponse>, ApiError> {
    match state.task_store.get(id).await? {
        Some(task) => Ok(Json(TaskResponse::from(task))),
        None => Err(ApiError::NotFound),
    }
}

/// Тело запроса на продолжение задачи. `message` — необязательная реплика
/// пользователя ("Продолжай", "Переделай", конкретная правка), которая
/// дописывается в накопленный контекст задачи перед тем, как агент
/// возобновит работу. `allow_manual` работает так же, как при создании
/// задачи — по умолчанию `false`, не наследуется автоматически от
/// исходного запроса.
#[derive(Deserialize)]
struct ContinueRequest {
    message: Option<String>,
    allow_manual: Option<bool>,
}

async fn continue_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<ContinueRequest>,
) -> Result<StatusCode, ApiError> {
    if state.task_store.get(id).await?.is_none() {
        return Err(ApiError::NotFound);
    }

    if let Some(message) = body.message {
        state
            .task_store
            .append_context(
                id,
                ContextEntry {
                    role: "user".to_string(),
                    content: message,
                    at: Utc::now(),
                    tool_call_id: None,
                },
            )
            .await?;
    }

    state
        .task_store
        .set_pending_telegram_message(id, None)
        .await?;

    // Задача уже идёт? Тогда реплика ДОПИСАНА в контекст выше, и этого
    // достаточно: пайплайн подхватит её сам на ближайшей границе шага и
    // продолжит ТУ ЖЕ задачу. Второй пайплайн не заводится.
    //
    // Раньше здесь безусловно делался `tokio::spawn`, и по одной задаче
    // начинали считать две генерации сразу — измерено АРХИВ-105:
    // `running_tasks` 1 -> 2, `busy_models.generations` 1 -> 2.
    if state.control.is_running(id).await {
        tracing::info!(
            "задача {id} уже идёт — реплика дописана в контекст, вторая генерация не заводится"
        );
        return Ok(StatusCode::ACCEPTED);
    }

    state.task_store.set_status(id, TaskStatus::Running).await?;

    // Перечитываем задачу — контекст уже включает только что добавленную
    // реплику пользователя, и именно эта полная история идёт в пайплайн,
    // а не только последнее сообщение.
    let refreshed = state.task_store.get(id).await?.ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("задача исчезла во время продолжения"))
    })?;

    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
        state.control.clone(),
        id,
        refreshed.category,
        context_to_messages(&refreshed.context),
        body.allow_manual.unwrap_or(false),
    ));

    Ok(StatusCode::OK)
}

async fn pause_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if state.task_store.get(id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    state.task_store.set_status(id, TaskStatus::Paused).await?;
    Ok(StatusCode::OK)
}

/// Решение человека по конкретному вызову инструмента.
#[derive(Deserialize)]
struct ToolDecisionRequest {
    /// `true` — исполнить, `false` — отказать. Поле обязательное намеренно:
    /// «да» и «нет» — разные решения, и умолчания у них быть не может.
    approve: bool,
}

/// Отдельный маршрут, а не признак в `continue`, и это не вкусовщина.
///
/// `continue` означает «продолжай работу» и ничего не говорит о КОНКРЕТНОМ
/// вызове: клиент, отправивший его, не подтверждает ничего — он просто
/// возобновляет задачу. Если навесить подтверждение на него, то любой
/// «продолжить» из интерфейса стал бы молчаливым «да» на висящий вызов
/// произвольного кода. Разрешение обязано быть отдельным, явным действием,
/// адресованным именно этому вызову.
///
/// Идемпотентности здесь нет и не нужно: решать нечего, если ожидающего
/// вызова нет — тогда 404, а не молчаливое «ок».
async fn tool_decision(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<ToolDecisionRequest>,
) -> Result<StatusCode, ApiError> {
    let task = state.task_store.get(id).await?.ok_or(ApiError::NotFound)?;

    let Some(call) = pending_tool_request(&task.context) else {
        return Err(ApiError::BadRequest(
            "у задачи нет вызова инструмента, ожидающего решения".to_string(),
        ));
    };

    if body.approve {
        // Единственный путь, которым человек создаёт `ApprovedToolCall`.
        // Исполнение идёт не здесь, а в возобновлённом пайплайне: здесь
        // некому держать `GenerationGuard`, а исполнять инструмент вне
        // сессии значило бы дать вытеснить модель под задачей.
        if let Err(err) = state
            .task_store
            .append_context(
                id,
                ContextEntry {
                    role: TOOL_APPROVAL_ROLE.to_string(),
                    content: format!("человек подтвердил вызов '{}'", call.name),
                    at: Utc::now(),
                    tool_call_id: Some(call.id.clone()),
                },
            )
            .await
        {
            tracing::error!("не удалось записать подтверждение по задаче {id}: {err:#}");
        }
        tracing::info!("задача {id}: человек подтвердил вызов '{}'", call.name);
    } else {
        // Отказ становится результатом вызова — сообщением роли `tool`.
        // Модель обязана узнать об отказе: иначе она либо повторит тот же
        // вызов, либо соврёт, что выполнила. Пусть лучше попробует иначе или
        // честно скажет, что не может.
        let refusal = format!(
            "ОТКАЗАНО ЧЕЛОВЕКОМ: вызов '{}' не был исполнен. \
             Не повторяй его; предложи другой способ или объясни, \
             почему задача невыполнима без него.",
            call.name
        );
        if let Err(err) = state
            .task_store
            .append_context(
                id,
                ContextEntry {
                    role: TOOL_ROLE.to_string(),
                    content: refusal,
                    at: Utc::now(),
                    tool_call_id: Some(call.id.clone()),
                },
            )
            .await
        {
            tracing::error!("не удалось записать отказ по задаче {id}: {err:#}");
        }

        if let Err(err) = crate::tool_gate::append_audit(
            crate::tool_gate::AUDIT_LOG_PATH,
            &id.to_string(),
            "-",
            &call,
            None,
            crate::tool_gate::Outcome::DeniedByHuman,
            "отказ человека",
        ) {
            tracing::error!("не удалось записать отказ в аудит: {err:#}");
        }
        tracing::info!("задача {id}: человек ОТКАЗАЛ в вызове '{}'", call.name);
    }

    // Задача возобновляется в обоих случаях: при «да» — чтобы исполнить, при
    // «нет» — чтобы модель увидела отказ и ответила по-человечески.
    state.task_store.set_status(id, TaskStatus::Running).await?;
    let refreshed =
        state.task_store.get(id).await?.ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!("задача исчезла во время решения"))
        })?;

    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
        state.control.clone(),
        id,
        refreshed.category.clone(),
        context_to_messages(&refreshed.context),
        false,
    ));

    Ok(StatusCode::OK)
}

async fn cancel_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if state.task_store.get(id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    state
        .task_store
        .set_status(id, TaskStatus::Cancelled)
        .await?;

    // Пометка в хранилище — это ЗАПИСЬ о решении, а не его исполнение.
    // Раньше здесь всё и заканчивалось: слово `Cancelled` не читал никто,
    // и генерация досиживала до конца. Теперь идущему пайплайну посылается
    // сигнал, и он бросает работу на ближайшей точке ожидания.
    let was_running = state.control.cancel(id).await;
    tracing::info!(
        "задача {id} отменена человеком; шла в этот момент: {}",
        if was_running {
            "да, сигнал послан"
        } else {
            "нет"
        }
    );
    Ok(StatusCode::OK)
}

/// Одна модель из Resource Registry вместе с её текущим состоянием загрузки.
/// Отдельный DTO, а не прямая отдача `ResourceEntry` — Registry не должен
/// подстраивать свою структуру под то, что удобно показывать в GUI.
#[derive(Serialize)]
struct ModelStatusDto {
    model: String,
    vram_mb: u32,
    ram_offload_mb: u32,
    always_loaded: bool,
    preferred_device: &'static str,
    loaded: bool,
    last_used: Option<DateTime<Utc>>,
    /// Параметр ЗАГРУЗКИ: смена требует перезагрузки модели.
    /// `None` — не измерено (модель не загружается на этой машине).
    context_length: Option<u32>,
    /// Потолок, заявленный моделью. Известен без загрузки.
    max_context_length: Option<u32>,
    /// Параметр ВЫЗОВА: применяется на каждый запрос, перезагрузка не нужна.
    /// `None` — не задана, действует умолчание провайдера.
    temperature: Option<f32>,
}

async fn list_models(State(state): State<AppState>) -> Json<Vec<ModelStatusDto>> {
    let loaded: HashMap<String, DateTime<Utc>> =
        state.scheduler.snapshot().await.into_iter().collect();

    let models = state
        .resource_registry
        .all()
        .map(|(name, entry)| ModelStatusDto {
            model: name.clone(),
            vram_mb: entry.vram_mb,
            ram_offload_mb: entry.ram_offload_mb,
            always_loaded: entry.always_loaded,
            preferred_device: preferred_device_str(entry.preferred_device),
            loaded: loaded.contains_key(name),
            last_used: loaded.get(name).copied(),
            context_length: entry.context_length,
            max_context_length: entry.max_context_length,
            // Через реестр, а не напрямую из entry: значение могло быть
            // изменено в рантайме и ещё не перечитано с диска.
            temperature: state.resource_registry.temperature_for(name),
        })
        .collect();

    Json(models)
}

/// Тело запроса на смену temperature.
///
/// `null` — осмысленное значение, а не отсутствие: оно означает «убрать
/// настройку», после чего поле перестаёт отправляться провайдеру и
/// действует его умолчание.
#[derive(Deserialize)]
struct SetTemperatureRequest {
    temperature: Option<f32>,
}

/// Смена temperature модели. Перезагрузка не требуется — это параметр
/// вызова (в отличие от context_length, который параметр загрузки).
///
/// Модель не обязана быть загружена: значение живёт в реестре и применится
/// при следующем обращении к ней.
async fn set_model_temperature(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SetTemperatureRequest>,
) -> Result<Json<Vec<ModelStatusDto>>, ApiError> {
    // Валидация именно здесь, а не только в UI: маршрут — единственная
    // точка, которую нельзя обойти (curl, Telegram, любой другой клиент).
    // При отказе реестр не читается и не пишется вообще.
    crate::resource_registry::validate_temperature(body.temperature)
        .map_err(ApiError::BadRequest)?;

    state
        .resource_registry
        .set_temperature(&name, body.temperature)
        .map_err(ApiError::Internal)?;
    tracing::info!(
        "temperature модели '{name}' изменена на {:?}",
        body.temperature
    );
    Ok(list_models(State(state)).await)
}

fn preferred_device_str(device: crate::resource_registry::PreferredDevice) -> &'static str {
    use crate::resource_registry::PreferredDevice;
    match device {
        PreferredDevice::Gpu => "gpu",
        PreferredDevice::Cpu => "cpu",
        PreferredDevice::Hybrid => "hybrid",
    }
}

#[derive(Serialize)]
struct LoadedModelDto {
    model: String,
    last_used: DateTime<Utc>,
}

#[derive(Serialize)]
struct StatusDto {
    /// Фактически выбранная реализация ModelBackend.
    backend: String,
    /// Конфигурация реализации без секретов, если backend её предоставляет.
    backend_config: Option<String>,
    /// Backend runtime health, independent from Scheduler's accounting.
    backend_healthy: bool,
    /// Whether the backend runtime was initialized successfully.
    backend_initialized: bool,
    /// Names of models resident in the backend right now.
    backend_loaded_resident_models: Vec<String>,
    /// Number of resident models (kept explicit for lightweight clients).
    backend_loaded_resident_model_count: usize,
    /// Configured llama.cpp GPU offload layer count, when exposed by backend.
    backend_configured_gpu_layers: Option<u32>,
    /// Safe llama.cpp device/offload summary, when a loaded model exposes it.
    backend_device_offload_summary: Option<String>,
    /// Provider capabilities reported without probing model output.
    capabilities: crate::model_backend::BackendCapabilities,
    /// Embedded llama.cpp cannot interrupt a synchronous native decode.
    cancellation_supported: bool,
    /// Whether the backend enforces a bounded generation timeout.
    timeout_supported: bool,
    /// Бюджет памяти под модели (RAM + VRAM минус резерв под ОС), а не
    /// объём видеопамяти — см. константу в main.rs.
    total_model_memory_mb: u32,
    /// Сумма табличных `vram_mb` по моделям, которые Scheduler считает
    /// загруженными. Учётная величина: у GPU ничего не спрашивается.
    used_model_memory_mb: u32,
    loaded_models: Vec<LoadedModelDto>,
    /// Сколько генераций идёт прямо сейчас и на каких моделях.
    ///
    /// Без этого «система свободна» и «идут две задачи, третья будет
    /// отвергнута» выглядят в панели одинаково.
    running_tasks: usize,
    /// Предел, после которого задачи получают отказ.
    max_concurrent_tasks: usize,
    busy_models: Vec<BusyModelDto>,
    /// `0` is intentional: Scheduler rejects over-capacity work instead of
    /// queueing it, so there is no hidden queue to report.
    queue_depth: usize,
    /// idle, loading, unloading, or generating.
    inference_state: String,
    /// Что Scheduler делает прямо сейчас, или `null` в покое.
    ///
    /// Без этого поля неблокирующий `/status` был бы честным, но непонятным:
    /// во время загрузки он говорил бы «модель не загружена», и отличить это
    /// от «модели нет и не будет» стало бы невозможно.
    active_operation: Option<ActiveOperationDto>,
}

#[derive(Serialize)]
struct BusyModelDto {
    model: String,
    /// Сколько генераций идёт на этой модели. Больше одной возможно:
    /// предел считается по задачам, а не по моделям.
    generations: usize,
}

#[derive(Serialize)]
struct ActiveOperationDto {
    /// «загрузка» или «выгрузка».
    kind: String,
    model: String,
    /// Категория задачи или ярлык модели — ради чего идёт операция.
    reason: String,
    started_at: DateTime<Utc>,
    /// Сколько уже длится. Считается на сервере, чтобы клиенту не пришлось
    /// сверять часы.
    elapsed_ms: i64,
}

async fn status(State(state): State<AppState>) -> Json<StatusDto> {
    let backend = state
        .scheduler
        .backend_status()
        .await
        .unwrap_or_else(|error| {
            tracing::warn!("не удалось получить runtime status backend-а: {error:#}");
            crate::model_backend::BackendStatus {
                healthy: false,
                initialized: false,
                loaded_resident_models: Vec::new(),
                configured_gpu_layers: None,
                device_offload_summary: None,
                capabilities: crate::model_backend::BackendCapabilities {
                    tool_calling: crate::model_backend::BackendCapability::Unsupported,
                },
                cancellation_supported: false,
                timeout_supported: false,
            }
        });
    let loaded = state.scheduler.snapshot().await;

    let used_model_memory_mb: u32 = loaded
        .iter()
        .filter_map(|(name, _)| state.resource_registry.get(name))
        .map(|r| r.vram_mb)
        .sum();

    let loaded_models = loaded
        .into_iter()
        .map(|(model, last_used)| LoadedModelDto { model, last_used })
        .collect();

    let active_operation = state
        .scheduler
        .active_operation()
        .map(|op| ActiveOperationDto {
            kind: op.kind.to_string(),
            model: op.model,
            reason: op.reason,
            started_at: op.started_at,
            elapsed_ms: (Utc::now() - op.started_at).num_milliseconds(),
        });

    let busy_models: Vec<BusyModelDto> = state
        .scheduler
        .busy_models()
        .into_iter()
        .map(|(model, generations)| BusyModelDto { model, generations })
        .collect();

    let running_tasks = state.scheduler.running_generations();
    let inference_state = match active_operation.as_ref().map(|op| op.kind.as_str()) {
        Some("загрузка") => "loading",
        Some("выгрузка") => "unloading",
        _ if running_tasks > 0 => "generating",
        _ => "idle",
    };

    Json(StatusDto {
        backend: state.scheduler.backend_name().to_string(),
        backend_config: state.scheduler.backend_config(),
        backend_healthy: backend.healthy,
        backend_initialized: backend.initialized,
        backend_loaded_resident_model_count: backend.loaded_resident_models.len(),
        backend_loaded_resident_models: backend.loaded_resident_models,
        backend_configured_gpu_layers: backend.configured_gpu_layers,
        backend_device_offload_summary: backend.device_offload_summary,
        capabilities: backend.capabilities,
        cancellation_supported: backend.cancellation_supported,
        timeout_supported: backend.timeout_supported,
        total_model_memory_mb: state.scheduler.total_model_memory_mb(),
        used_model_memory_mb,
        loaded_models,
        running_tasks: state.scheduler.running_generations(),
        max_concurrent_tasks: crate::scheduler::MAX_CONCURRENT_TASKS,
        busy_models,
        queue_depth: 0,
        inference_state: inference_state.to_string(),
        active_operation,
    })
}

// --- Ошибки API ---

/// Единая обёртка ошибок для обработчиков: либо "не найдено" (404),
/// либо любая внутренняя ошибка (500) с текстом причины.
enum ApiError {
    NotFound,
    /// Запрос отвергнут по содержимому — вина клиента, не сервера.
    /// Текст уходит пользователю как есть, поэтому обязан называть поле
    /// и допустимые значения.
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "задача не найдена").into_response(),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            ApiError::Internal(err) => {
                tracing::error!("Control Center API: {err:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provenance::{LicensedProvenance, MediaKind, Provenance};

    #[test]
    fn every_agent_run_gets_local_execution_instructions_first() {
        let mut messages = vec![Message::new(Role::User, "создай файл")];

        add_default_local_agent_instructions(&mut messages);

        assert_eq!(messages[0].role, Role::System);
        assert!(messages[0].content.contains("workspace tools"));
        assert!(messages[0]
            .content
            .contains("После создания нового файла перечитай точный целевой файл"));
        assert!(messages[0].content.contains("сохрани начальный символ `/`"));
        assert_eq!(messages[1].role, Role::User);
    }

    #[test]
    fn create_task_model_override_is_optional_but_must_be_registered() {
        let registry = ResourceRegistry::from_file("config/resource_registry.yaml")
            .expect("resource registry fixture");
        let known = registry.all().next().expect("at least one model").0.clone();

        assert_eq!(validate_model_override(&registry, None).unwrap(), None);
        assert_eq!(
            validate_model_override(&registry, Some(&known)).unwrap(),
            Some(known)
        );
        assert!(validate_model_override(&registry, Some(" ")).is_err());
        assert!(validate_model_override(&registry, Some("not-in-registry")).is_err());
    }

    fn entry(asset_id: &str, license: &str, attribution: &str) -> ManifestEntry {
        ManifestEntry {
            asset_id: asset_id.to_string(),
            kind: MediaKind::Image,
            provenance: Provenance::Licensed(LicensedProvenance {
                source: "openverse".to_string(),
                asset_url: "https://example.org/a.jpg".to_string(),
                landing_url: "https://example.org/p".to_string(),
                license: license.to_string(),
                license_version: "2.0".to_string(),
                license_url: "https://creativecommons.org/licenses/by/2.0/".to_string(),
                license_snapshot: "снимок".to_string(),
                creator: Some("Автор".to_string()),
                creator_url: None,
                attribution: attribution.to_string(),
                retrieved_at: Utc::now(),
            }),
        }
    }

    // --- Отмена задачи ---------------------------------------------------

    /// Backend, который «думает» заданное время и возвращает текст.
    ///
    /// Настоящая модель здесь не нужна и не разрешена: предмет проверки —
    /// что делает система со статусом, когда генерация завершается ПОСЛЕ
    /// отмены. Содержимое ответа к этому отношения не имеет, а время
    /// генерации должно быть миллисекундами, а не минутами.
    struct SlowFakeBackend {
        generate_ms: u64,
    }

    #[async_trait::async_trait]
    impl crate::model_backend::ModelBackend for SlowFakeBackend {
        async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unload(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: crate::model_backend::GenerateRequest,
        ) -> anyhow::Result<crate::model_backend::GenerateResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(self.generate_ms)).await;
            Ok(crate::model_backend::GenerateResponse::Text(
                "ответ модели".to_string(),
            ))
        }
    }

    fn pipeline_fixture(generate_ms: u64) -> (AppState, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaic-cancel-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let resources = dir.join("resource_registry.yaml");
        std::fs::write(&resources, "Fake:\n  vram_mb: 1000\n  load_seconds: 1\n").unwrap();
        let capabilities = dir.join("capability_registry.yaml");
        std::fs::write(&capabilities, "Simple:\n  primary: Fake\n").unwrap();

        let resource_registry = Arc::new(ResourceRegistry::from_file(&resources).unwrap());
        let scheduler = Arc::new(Scheduler::new(
            Arc::new(
                crate::capability_registry::CapabilityRegistry::from_file(&capabilities).unwrap(),
            ),
            resource_registry.clone(),
            Arc::new(SlowFakeBackend { generate_ms }),
            24495,
        ));
        let task_store = Arc::new(TaskStore::new(":memory:").unwrap());

        (
            AppState {
                task_store,
                scheduler,
                resource_registry,
                control: TaskControl::default(),
            },
            dir,
        )
    }

    struct TextFakeBackend {
        generate_ms: u64,
        text: String,
    }

    #[async_trait::async_trait]
    impl crate::model_backend::ModelBackend for TextFakeBackend {
        async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unload(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: crate::model_backend::GenerateRequest,
        ) -> anyhow::Result<crate::model_backend::GenerateResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(self.generate_ms)).await;
            Ok(crate::model_backend::GenerateResponse::Text(
                self.text.clone(),
            ))
        }
    }

    fn text_pipeline_fixture(text: &str, generate_ms: u64) -> (AppState, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaic-text-output-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("resource_registry.yaml"),
            "Fake:\n  vram_mb: 1000\n  load_seconds: 1\nChosen:\n  vram_mb: 1000\n  load_seconds: 1\nFable9B:\n  vram_mb: 1000\n  load_seconds: 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("capability_registry.yaml"),
            "Simple:\n  primary: Fake\n",
        )
        .unwrap();

        let resource_registry =
            Arc::new(ResourceRegistry::from_file(dir.join("resource_registry.yaml")).unwrap());
        let scheduler = Arc::new(Scheduler::new(
            Arc::new(
                crate::capability_registry::CapabilityRegistry::from_file(
                    dir.join("capability_registry.yaml"),
                )
                .unwrap(),
            ),
            resource_registry.clone(),
            Arc::new(TextFakeBackend {
                generate_ms,
                text: text.to_string(),
            }),
            24495,
        ));
        (
            AppState {
                task_store: Arc::new(TaskStore::new(":memory:").unwrap()),
                scheduler,
                resource_registry,
                control: TaskControl::default(),
            },
            dir,
        )
    }

    async fn status_of(state: &AppState, id: Uuid) -> TaskStatus {
        state
            .task_store
            .get(id)
            .await
            .expect("задача читается")
            .expect("задача существует")
            .status
    }

    #[tokio::test]
    async fn video_artifact_and_answer_publish_atomically_only_for_running_task() {
        let (state, dir) = text_pipeline_fixture("ответ", 1);
        let completed = state.task_store.create("Video").await.unwrap();
        record_artifact_outcome(
            &state.task_store,
            completed.id,
            "/tmp/render.mp4".to_string(),
            "готово".to_string(),
        )
        .await;
        let completed = state.task_store.get(completed.id).await.unwrap().unwrap();
        assert_eq!(completed.status, TaskStatus::Done);
        assert!(completed
            .context
            .iter()
            .any(|entry| entry.role == ARTIFACT_ROLE));
        assert!(completed
            .context
            .iter()
            .any(|entry| entry.role == "assistant"));

        let cancelled = state.task_store.create("Video").await.unwrap();
        state
            .task_store
            .set_status(cancelled.id, TaskStatus::Cancelled)
            .await
            .unwrap();
        record_artifact_outcome(
            &state.task_store,
            cancelled.id,
            "/tmp/must-not-publish.mp4".to_string(),
            "поздний результат".to_string(),
        )
        .await;
        let cancelled = state.task_store.get(cancelled.id).await.unwrap().unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert!(!cancelled
            .context
            .iter()
            .any(|entry| entry.role == ARTIFACT_ROLE));
        assert!(!cancelled
            .context
            .iter()
            .any(|entry| entry.role == "assistant"));
        assert!(cancelled.context.iter().any(|entry| entry.role == "system"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn status_includes_backend_runtime_metrics() {
        let (state, dir) = text_pipeline_fixture("ответ", 1);
        let payload = serde_json::to_value(status(State(state.clone())).await.0)
            .expect("status response must serialize");

        assert_eq!(payload["backend_healthy"], serde_json::json!(true));
        assert_eq!(payload["backend_initialized"], serde_json::json!(true));
        assert_eq!(
            payload["backend_loaded_resident_models"],
            serde_json::json!([])
        );
        assert_eq!(
            payload["backend_loaded_resident_model_count"],
            serde_json::json!(0)
        );
        assert_eq!(
            payload["backend_configured_gpu_layers"],
            serde_json::Value::Null
        );
        assert_eq!(
            payload["backend_device_offload_summary"],
            serde_json::Value::Null
        );
        assert_eq!(payload["cancellation_supported"], serde_json::json!(false));
        assert_eq!(payload["timeout_supported"], serde_json::json!(false));
        assert_eq!(payload["queue_depth"], serde_json::json!(0));
        assert_eq!(payload["inference_state"], serde_json::json!("idle"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_and_whitespace_model_text_fail_instead_of_becoming_answers() {
        for text in ["", " \n\t "] {
            let (state, dir) = text_pipeline_fixture(text, 1);
            let task = state.task_store.create("Simple").await.unwrap();

            run_task_pipeline(
                state.task_store.clone(),
                state.scheduler.clone(),
                state.control.clone(),
                task.id,
                "Simple".to_string(),
                Vec::new(),
                false,
            )
            .await;

            let loaded = state.task_store.get(task.id).await.unwrap().unwrap();
            assert_eq!(
                loaded.status,
                TaskStatus::Failed,
                "model output {text:?} was accepted as Done"
            );
            assert!(
                loaded.context.iter().all(|entry| entry.role != "assistant"),
                "empty model output became an assistant answer"
            );
            assert!(
                loaded.context.iter().any(|entry| {
                    entry.role == "system" && entry.content.contains("модель вернула пустой текст")
                }),
                "failure reason does not explain the empty model output"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[tokio::test]
    async fn non_empty_model_text_still_becomes_the_answer() {
        let (state, dir) = text_pipeline_fixture("  ответ модели  ", 1);
        let task = state.task_store.create("Simple").await.unwrap();

        run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        )
        .await;

        let loaded = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Done);
        assert!(loaded.context.iter().any(|entry| {
            entry.role == "assistant" && entry.content == "  ответ модели  "
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_turn_uses_the_real_task_pipeline_and_returns_its_answer() {
        let (state, dir) = text_pipeline_fixture("ответ терминальному пользователю", 1);

        let turn = run_terminal_turn(&state, None, "привет из терминала", false)
            .await
            .expect("terminal turn must complete through the task pipeline");

        assert_eq!(turn.status, TaskStatus::Done);
        assert_eq!(
            turn.reply.as_deref(),
            Some("ответ терминальному пользователю")
        );
        let stored = state
            .task_store
            .get(turn.task_id)
            .await
            .unwrap()
            .expect("terminal task must be persisted");
        assert!(stored.context.iter().any(|entry| {
            entry.role == "user" && entry.content == "привет из терминала"
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_model_override_reaches_the_real_agent_session() {
        let (state, dir) = text_pipeline_fixture("ответ выбранной модели", 1);

        let outcome = run_terminal_turn_interruptible_with_model(
            &state,
            None,
            "используй выбранную модель",
            Some("Chosen"),
            false,
            std::future::pending(),
        )
        .await
        .unwrap();
        let TerminalTurnOutcome::Completed(turn) = outcome else {
            panic!("terminal turn was unexpectedly interrupted");
        };
        let stored = state.task_store.get(turn.task_id).await.unwrap().unwrap();

        assert_eq!(execution_metadata(&stored.context).unwrap().model, "Chosen");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_model_override_also_reaches_the_video_model_step() {
        let (state, dir) = text_pipeline_fixture("зимний лес", 1);
        let task = state.task_store.create("Video").await.unwrap();

        let term = extract_search_term(
            &state.scheduler,
            &state.task_store,
            task.id,
            "сделай ролик о зимнем лесе",
            Some("Chosen"),
        )
        .await
        .unwrap();
        let loaded: Vec<_> = state
            .scheduler
            .snapshot()
            .await
            .into_iter()
            .map(|(model, _)| model)
            .collect();

        assert_eq!(term, "зимний лес");
        assert!(loaded.contains(&"Chosen".to_string()));
        let stored = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(execution_metadata(&stored.context).unwrap().model, "Chosen");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn auto_video_model_step_keeps_the_extraction_model_and_records_provenance() {
        let (state, dir) = text_pipeline_fixture("зимний лес", 1);
        let task = state.task_store.create("Video").await.unwrap();

        let term = extract_search_term(
            &state.scheduler,
            &state.task_store,
            task.id,
            "сделай ролик о зимнем лесе",
            None,
        )
        .await
        .unwrap();
        let stored = state.task_store.get(task.id).await.unwrap().unwrap();

        assert_eq!(term, "зимний лес");
        assert_eq!(
            execution_metadata(&stored.context).unwrap().model,
            media_query_extract::EXTRACTION_MODEL_LABEL
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ctrl_c_cancels_a_video_terminal_turn_without_waiting_for_the_pipeline() {
        let (state, dir) = text_pipeline_fixture("зимний лес", 5_000);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            run_terminal_turn_interruptible_with_model(
                &state,
                None,
                "сделай видео о зимнем лесе",
                Some("Chosen"),
                false,
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Ok(())
                },
            ),
        )
        .await
        .expect("Video cancellation must not wait for the media pipeline")
        .unwrap();
        let TerminalTurnOutcome::Interrupted { task_id } = outcome else {
            panic!("Video turn was not interrupted");
        };

        assert_eq!(status_of(&state, task_id).await, TaskStatus::Cancelled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unavailable_explicit_video_model_fails_instead_of_silently_falling_back() {
        let (state, dir) = text_pipeline_fixture("не используется", 1);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            run_terminal_turn_interruptible_with_model(
                &state,
                None,
                "сделай видео о зимнем лесе",
                Some("Missing"),
                false,
                std::future::pending(),
            ),
        )
        .await
        .expect("explicit model failure must finish before media network work")
        .unwrap();
        let TerminalTurnOutcome::Completed(turn) = outcome else {
            panic!("turn was unexpectedly interrupted");
        };

        assert_eq!(turn.status, TaskStatus::Failed);
        assert!(turn
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("Missing"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn task_control_registration_cannot_replace_an_active_pipeline() {
        let control = TaskControl::default();
        let task_id = Uuid::new_v4();

        let mut first = control
            .try_register(task_id)
            .await
            .expect("first pipeline must acquire the task");
        assert!(control.try_register(task_id).await.is_none());

        assert!(control.cancel(task_id).await);
        first.changed().await.unwrap();
        assert!(*first.borrow());
    }

    #[tokio::test]
    async fn cancelled_task_cannot_be_restored_to_waiting_for_human() {
        let store = Arc::new(TaskStore::new(":memory:").unwrap());
        let task = store.create("Simple").await.unwrap();
        store
            .set_status(task.id, TaskStatus::Cancelled)
            .await
            .unwrap();
        let call = crate::model_backend::ToolCall {
            id: "call_cancelled".to_string(),
            name: "write_file".to_string(),
            arguments: "{\"path\":\"x\"}".to_string(),
        };

        let result = park_for_approval(&store, task.id, "filesystem", &call).await;

        assert!(result.is_err());
        assert_eq!(
            store.get(task.id).await.unwrap().unwrap().status,
            TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn interrupt_error_releases_registration_and_fails_the_task() {
        let (state, dir) = text_pipeline_fixture("ответ не должен сохраниться", 5_000);

        let error = match run_terminal_turn_interruptible(
            &state,
            None,
            "вопрос",
            false,
            std::future::ready(Err(std::io::Error::other("signal handler failed"))),
        )
        .await
        {
            Ok(_) => panic!("broken signal future must be reported"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("signal handler failed"));

        let tasks = state.task_store.list().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert!(!state.control.is_running(tasks[0].id).await);
        assert!(tasks[0]
            .context
            .iter()
            .all(|entry| entry.role != "assistant"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ctrl_c_cancels_an_in_flight_terminal_turn() {
        let (state, dir) = text_pipeline_fixture("слишком поздний ответ", 5_000);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            run_terminal_turn_interruptible(&state, None, "долгий вопрос", false, async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            }),
        )
        .await
        .expect("Ctrl+C must not wait for model completion")
        .expect("interrupt handling must succeed");

        let task_id = match outcome {
            TerminalTurnOutcome::Interrupted { task_id } => task_id,
            TerminalTurnOutcome::Completed(_) => panic!("slow turn completed before interrupt"),
        };
        let stored = state.task_store.get(task_id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Cancelled);
        assert!(!state.control.is_running(task_id).await);
        assert!(stored.context.iter().all(|entry| entry.role != "assistant"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_followup_continues_the_same_persisted_conversation() {
        let (state, dir) = text_pipeline_fixture("ответ", 1);
        let first = run_terminal_turn(&state, None, "первый вопрос", false)
            .await
            .unwrap();

        let second = run_terminal_turn(&state, Some(first.task_id), "уточнение", false)
            .await
            .expect("follow-up must continue the same terminal task");

        assert_eq!(second.task_id, first.task_id);
        assert_eq!(second.status, TaskStatus::Done);
        let stored = state.task_store.get(second.task_id).await.unwrap().unwrap();
        let user_messages = stored
            .context
            .iter()
            .filter(|entry| entry.role == "user")
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(user_messages, vec!["первый вопрос", "уточнение"]);
        assert_eq!(
            stored
                .context
                .iter()
                .filter(|entry| entry.role == "assistant")
                .count(),
            2
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_turn_returns_the_pipeline_failure_reason() {
        let (state, dir) = text_pipeline_fixture("  ", 1);

        let turn = run_terminal_turn(&state, None, "вопрос", false)
            .await
            .expect("pipeline failures are terminal results, not transport failures");

        assert_eq!(turn.status, TaskStatus::Failed);
        assert!(
            turn.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("модель вернула пустой текст")),
            "terminal result must expose the stored failure reason"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminal_followup_cannot_bypass_a_pending_tool_approval() {
        let (state, dir) = text_pipeline_fixture("ответ", 1);
        let task = state.task_store.create("Simple").await.unwrap();
        state
            .task_store
            .set_status(task.id, TaskStatus::WaitingForHuman)
            .await
            .unwrap();

        let error = match run_terminal_turn(&state, Some(task.id), "продолжай", false).await
        {
            Ok(_) => panic!("waiting task must not accept a normal follow-up"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("ожидает решения по инструменту"));
        let stored = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::WaitingForHuman);
        assert!(stored.context.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn pause_wins_when_empty_model_text_arrives_after_the_human_decision() {
        let (state, dir) = text_pipeline_fixture(" \n ", 250);
        let task = state.task_store.create("Simple").await.unwrap();
        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let paused = pause_task(State(state.clone()), Path(task.id)).await;
        assert!(paused.is_ok(), "pause handler rejected an existing task");
        pipeline.await.unwrap();

        let loaded = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Paused);
        assert!(loaded.context.iter().all(|entry| entry.role != "assistant"));
        assert!(loaded
            .context
            .iter()
            .any(|entry| entry.content.contains("модель вернула пустой текст")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_wins_while_an_empty_model_response_is_in_flight() {
        let (state, dir) = text_pipeline_fixture("", 250);
        let task = state.task_store.create("Simple").await.unwrap();
        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let cancelled = cancel_task(State(state.clone()), Path(task.id)).await;
        assert!(
            cancelled.is_ok(),
            "cancel handler rejected an existing task"
        );
        pipeline.await.unwrap();

        let loaded = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Cancelled);
        assert!(loaded.context.iter().all(|entry| entry.role != "assistant"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Считает обращения к модели. Нужен там, где предмет проверки — СКОЛЬКО
    /// раз система пошла к модели, а не что она принесла.
    struct CountingSlowBackend {
        generate_ms: u64,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        /// Сколько генераций идёт ПРЯМО СЕЙЧАС.
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        /// Максимум, которого одновременность достигала за прогон. Это и
        /// есть измеренная величина дефекта: АРХИВ-105 видел `running_tasks`
        /// 1 -> 2 на одной задаче.
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::model_backend::ModelBackend for CountingSlowBackend {
        async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unload(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: crate::model_backend::GenerateRequest,
        ) -> anyhow::Result<crate::model_backend::GenerateResponse> {
            use std::sync::atomic::Ordering::SeqCst;
            self.calls.fetch_add(1, SeqCst);
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(self.generate_ms)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            Ok(crate::model_backend::GenerateResponse::Text(
                "ответ модели".to_string(),
            ))
        }
    }

    /// Что видит фикстура: сколько раз ходили к модели и какой была
    /// наибольшая одновременность.
    struct Counters {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn counting_fixture(generate_ms: u64) -> (AppState, Counters, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaic-count-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let resources = dir.join("resource_registry.yaml");
        std::fs::write(
            &resources,
            "Fake:
  vram_mb: 1000
  load_seconds: 1
",
        )
        .unwrap();
        let capabilities = dir.join("capability_registry.yaml");
        std::fs::write(
            &capabilities,
            "Simple:
  primary: Fake
",
        )
        .unwrap();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resource_registry = Arc::new(ResourceRegistry::from_file(&resources).unwrap());
        let scheduler = Arc::new(Scheduler::new(
            Arc::new(
                crate::capability_registry::CapabilityRegistry::from_file(&capabilities).unwrap(),
            ),
            resource_registry.clone(),
            Arc::new(CountingSlowBackend {
                generate_ms,
                calls: calls.clone(),
                in_flight: in_flight.clone(),
                peak: peak.clone(),
            }),
            24495,
        ));
        let task_store = Arc::new(TaskStore::new(":memory:").unwrap());
        (
            AppState {
                task_store,
                scheduler,
                resource_registry,
                control: TaskControl::default(),
            },
            Counters { calls, peak },
            dir,
        )
    }

    /// ДОЛГ 2, ЧАСТЬ 1. Отмена обязана ОСТАНАВЛИВАТЬ работу, а не только
    /// помечать задачу отменённой.
    ///
    /// Измерено АРХИВ-105 на живой системе: `POST /cancel` отвечал 200
    /// мгновенно, а работа шла ещё 36 секунд — 46.141 с против 46.266 с при
    /// контроле, то есть разница ноль. Здесь то же самое в миллисекундах.
    ///
    /// Что именно упало бы, если правку убрать: пайплайн досиживает всю
    /// генерацию, и от момента отмены до его завершения проходит почти
    /// столько же, сколько оставалось генерации.
    #[tokio::test]
    async fn cancelling_stops_the_generation_instead_of_waiting_for_it() {
        let (state, dir) = pipeline_fixture(800);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let cancelled_at = std::time::Instant::now();
        if cancel_task(State(state.clone()), Path(task.id))
            .await
            .is_err()
        {
            panic!("обработчик отмены отказал");
        }

        pipeline.await.expect("пайплайн не паниковал");
        let after_cancel = cancelled_at.elapsed();

        assert!(
            after_cancel < std::time::Duration::from_millis(300),
            "после отмены пайплайн жил ещё {:?} — то есть досидел генерацию,              а не прекратил её. Осталось генерации было ~650 мс",
            after_cancel
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// РЕГРЕССИЯ, НАЙДЕННАЯ ЖИВЫМ ПРОГОНОМ (не модульным тестом).
    ///
    /// Подхват реплики человека я в прошлом заходе определял как «контекст
    /// вырос за время генерации». Это неверно: пайплайн пишет в контекст САМ —
    /// `note()` кладёт список инструментов, `record_tool_request()` кладёт
    /// запрос вызова. То есть на любой задаче с инструментами условие
    /// выполнялось без всякой реплики человека, цикл перезапускался до предела
    /// `MAX_HUMAN_FOLLOWUPS`, и последний пустой ответ ЗАТИРАЛ настоящий.
    ///
    /// Живой прогон это и показал: задача ответила по делу на 7-й записи, а
    /// закрылась пустым ответом на 13-й. Модульные тесты прошлого захода этого
    /// не видели, потому что без `config/mcp.yaml` пайплайн ничего сам не
    /// пишет.
    ///
    /// Различать надо не рост контекста, а появление записей роли `user`.
    #[tokio::test]
    async fn context_grown_by_the_pipeline_itself_is_not_a_human_reply() {
        let (state, counters, dir) = counting_fixture(600);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        // Пайплайн так и пишет о себе сам: роль `system`, не `user`.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        state
            .task_store
            .append_context(
                task.id,
                ContextEntry {
                    role: "system".to_string(),
                    content: "инструменты от 'BlenderMCP': get_scene_info, …".to_string(),
                    at: Utc::now(),
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();

        pipeline.await.expect("пайплайн не паниковал");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(
            counters.calls.load(SeqCst),
            1,
            "рост контекста записью самого пайплайна принят за реплику человека:              цикл перезапустился {} раз вместо одного",
            counters.calls.load(SeqCst)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ДОЛГ 2, ЧАСТЬ 2. Реплика в ИДУЩУЮ задачу не должна заводить вторую
    /// генерацию.
    ///
    /// Измерено АРХИВ-105: `POST /continue` отвечал 200, а `running_tasks`
    /// уходил с 1 на 2 и `busy_models.generations` с 1 на 2 — то есть модель
    /// считала одну и ту же задачу дважды.
    ///
    /// Что именно упало бы без правки: счётчик обращений к модели равен 2.
    #[tokio::test]
    async fn continuing_a_running_task_does_not_start_a_second_generation() {
        let (state, counters, dir) = counting_fixture(600);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = continue_task(
            State(state.clone()),
            Path(task.id),
            Json(ContinueRequest {
                message: Some("добавь про мосты".to_string()),
                allow_manual: None,
            }),
        )
        .await;

        pipeline.await.expect("пайплайн не паниковал");
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;

        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(
            counters.peak.load(SeqCst),
            1,
            "по одной задаче одновременно шло {} генераций: реплика завела ВТОРУЮ              параллельную работу вместо того, чтобы войти в текущую задачу.              Это и есть измеренное АРХИВ-105 `running_tasks` 1 -> 2",
            counters.peak.load(SeqCst)
        );
        assert_eq!(
            counters.calls.load(SeqCst),
            2,
            "реплика обязана быть учтена: ожидался ровно один ДОПОЛНИТЕЛЬНЫЙ шаг              той же задачи, последовательно после первого"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Главный дефект: человек отменил, система подтвердила отмену, а
    /// завершившаяся генерация молча вернула задачу в Done.
    ///
    /// Отмена идёт через САМ обработчик `cancel_task`, а не мимо него —
    /// иначе проверялся бы не тот путь, которым пользуется человек.
    #[tokio::test]
    async fn a_cancelled_task_stays_cancelled_after_the_generation_finishes() {
        let (state, dir) = pipeline_fixture(400);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        // Отменяем, пока генерация идёт.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        match cancel_task(State(state.clone()), Path(task.id)).await {
            Ok(code) => assert_eq!(code, StatusCode::OK, "отмена обязана вернуть 200"),
            Err(_) => panic!("обработчик отмены отказал"),
        }
        assert_eq!(
            status_of(&state, task.id).await,
            TaskStatus::Cancelled,
            "отмена не записалась — проверять дальше нечего"
        );

        pipeline.await.expect("пайплайн не паниковал");

        assert_eq!(
            status_of(&state, task.id).await,
            TaskStatus::Cancelled,
            "завершившаяся генерация переписала отмену"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ЭТОТ ТЕСТ ЗАМЕНИЛ СОБОЙ `work_finished_after_a_cancel_is_kept_but_not_as_the_answer`.
    ///
    /// Прежний тест закреплял поведение мира, в котором отмена не
    /// останавливала работу: генерация всё равно досчитывалась, и вопрос
    /// был лишь в том, куда положить её результат — он требовал, чтобы
    /// результат остался следом роли `system`, но не стал ответом.
    ///
    /// После правки долга 2 такого результата не существует: отмена бросает
    /// генерацию. Требование «сохрани досчитанное» стало недостижимым не
    /// потому, что его нарушили, а потому, что досчитывать больше нечего.
    /// Оставить прежний тест зелёным можно было бы только вернув дефект.
    ///
    /// Проверяемое здесь свойство строго сильнее прежнего: у отменённой
    /// задачи нет ни ответа, ни текста модели вообще.
    #[tokio::test]
    async fn a_cancelled_task_has_neither_an_answer_nor_any_model_text() {
        let (state, dir) = pipeline_fixture(800);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = cancel_task(State(state.clone()), Path(task.id)).await;
        pipeline.await.expect("пайплайн не паниковал");

        assert_eq!(
            status_of(&state, task.id).await,
            TaskStatus::Cancelled,
            "отменённая задача обязана остаться отменённой"
        );

        let context = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;

        assert!(
            context.iter().all(|e| e.role != "assistant"),
            "у отменённой задачи появился ответ — Telegram-мост отправит его человеку"
        );
        assert!(
            context.iter().all(|e| !e.content.contains("ответ модели")),
            "текст модели попал в контекст отменённой задачи: генерация не была прервана,              а лишь переименована"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Терминальность сделана НЕ глобальной, и это намеренно: команда
    /// человека обязана применяться всегда. Если бы `set_status` запрещал
    /// уход с `Cancelled`, продолжение отменённой задачи молча ничего бы не
    /// делало — то есть починка одного молчаливого отказа создала бы другой.
    #[tokio::test]
    async fn a_human_command_can_still_move_a_cancelled_task() {
        let (state, dir) = pipeline_fixture(1);
        let task = state.task_store.create("Simple").await.unwrap();

        let _ = cancel_task(State(state.clone()), Path(task.id)).await;
        assert_eq!(status_of(&state, task.id).await, TaskStatus::Cancelled);

        // Тот же безусловный путь, которым пользуются cancel/pause/continue.
        state
            .task_store
            .set_status(task.id, TaskStatus::Running)
            .await
            .expect("команда человека применяется безусловно");
        assert_eq!(status_of(&state, task.id).await, TaskStatus::Running);

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Цикл вызова инструментов ---------------------------------------

    /// Backend, который ВСЕГДА просит вызвать инструмент и никогда не
    /// отвечает текстом. Ровно то поведение, ради которого существует предел
    /// итераций: зацикливание модели — наблюдаемое явление, а не гипотеза.
    struct AlwaysToolCallsBackend {
        seen: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::model_backend::ModelBackend for AlwaysToolCallsBackend {
        async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn load(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn unload(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn generate(
            &self,
            _model: &str,
            _request: GenerateRequest,
        ) -> anyhow::Result<GenerateResponse> {
            let n = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(GenerateResponse::ToolCalls(vec![
                crate::model_backend::ToolCall {
                    id: format!("call_{n}"),
                    name: "get_scene_info".to_string(),
                    arguments: "{}".to_string(),
                },
            ]))
        }
    }

    fn looping_fixture() -> (
        AppState,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::path::PathBuf,
    ) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kaic-loop-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("resource_registry.yaml"),
            "Fake:\n  vram_mb: 1000\n  load_seconds: 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("capability_registry.yaml"),
            "Simple:\n  primary: Fake\n",
        )
        .unwrap();

        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resource_registry =
            Arc::new(ResourceRegistry::from_file(dir.join("resource_registry.yaml")).unwrap());
        let scheduler = Arc::new(Scheduler::new(
            Arc::new(
                crate::capability_registry::CapabilityRegistry::from_file(
                    dir.join("capability_registry.yaml"),
                )
                .unwrap(),
            ),
            resource_registry.clone(),
            Arc::new(AlwaysToolCallsBackend { seen: seen.clone() }),
            24495,
        ));
        (
            AppState {
                task_store: Arc::new(TaskStore::new(":memory:").unwrap()),
                scheduler,
                resource_registry,
                control: TaskControl::default(),
            },
            seen,
            dir,
        )
    }

    /// Модель, зациклившаяся на вызовах инструмента, обязана упереться в
    /// предел и уронить задачу с ВНЯТНОЙ причиной — а не крутиться, пока
    /// кто-нибудь не заметит.
    #[tokio::test]
    async fn a_looping_model_hits_the_step_limit_and_fails_out_loud() {
        let (state, seen, dir) = looping_fixture();
        let task = state.task_store.create("Simple").await.unwrap();

        // Встроенные инструменты доступны и без MCP, но backend намеренно
        // запрашивает имя, которого ему не выдавали. Цикл обязан отклонить
        // такой вызов сразу, до ToolGate и любого исполнителя.
        run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            vec![Message::new(Role::User, "сделай что-нибудь")],
            false,
        )
        .await;

        let loaded = state.task_store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Failed, "задача обязана упасть");

        let reason = loaded
            .context
            .iter()
            .find(|e| e.content.starts_with("Ошибка:"))
            .expect("причина обязана быть в контексте задачи");
        assert!(
            reason.content.contains("невыданный инструмент"),
            "причина не называет, что произошло: {}",
            reason.content
        );
        // Ровно один запрос: без инструментов крутиться незачем.
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Отказ человека обязан дойти до модели сообщением роли `tool`.
    ///
    /// Иначе она либо повторит тот же вызов, либо соврёт, что выполнила его.
    #[tokio::test]
    async fn a_refusal_reaches_the_model_as_a_tool_result() {
        let (state, _seen, dir) = looping_fixture();
        let task = state.task_store.create("Simple").await.unwrap();

        // Задача в состоянии «ждём решения по вызову»: есть запрос и нет
        // ответа на него.
        state
            .task_store
            .append_context(
                task.id,
                ContextEntry {
                    role: TOOL_REQUEST_ROLE.to_string(),
                    content: serde_json::json!({
                        "name": "execute_blender_code",
                        "arguments": "{\"code\":\"import os\"}"
                    })
                    .to_string(),
                    at: Utc::now(),
                    tool_call_id: Some("call_9".to_string()),
                },
            )
            .await
            .unwrap();
        state
            .task_store
            .set_status(task.id, TaskStatus::WaitingForHuman)
            .await
            .unwrap();

        let found = pending_tool_request(
            &state
                .task_store
                .get(task.id)
                .await
                .unwrap()
                .unwrap()
                .context,
        )
        .expect("ожидающий вызов найден");
        assert_eq!(found.name, "execute_blender_code");

        // Человек отказывает.
        let code = tool_decision(
            State(state.clone()),
            Path(task.id),
            Json(ToolDecisionRequest { approve: false }),
        )
        .await;
        assert!(code.is_ok(), "маршрут решения обязан принять отказ");

        let context = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;
        let refusal = context
            .iter()
            .find(|e| e.role == TOOL_ROLE && e.tool_call_id.as_deref() == Some("call_9"))
            .expect("отказ обязан лечь ролью tool с тем же tool_call_id");
        assert!(
            refusal.content.contains("ОТКАЗАНО ЧЕЛОВЕКОМ"),
            "модель не поймёт, что это отказ: {}",
            refusal.content
        );
        // После ответа вызов перестаёт быть ожидающим — иначе повторное
        // решение исполнило бы его ещё раз.
        assert!(pending_tool_request(&context).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Подтверждённый вызов помечается так, что возобновлённый пайплайн
    /// исполнит его — и ровно один раз.
    #[tokio::test]
    async fn an_approval_is_recorded_and_consumed_once() {
        let (state, _seen, dir) = looping_fixture();
        let task = state.task_store.create("Simple").await.unwrap();
        state
            .task_store
            .append_context(
                task.id,
                ContextEntry {
                    role: TOOL_REQUEST_ROLE.to_string(),
                    content: serde_json::json!({"name":"get_scene_info","arguments":"{}"})
                        .to_string(),
                    at: Utc::now(),
                    tool_call_id: Some("call_5".to_string()),
                },
            )
            .await
            .unwrap();

        let ctx = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;
        // До решения человека — не подтверждён.
        assert!(approved_pending_call(&ctx).is_none());

        state
            .task_store
            .append_context(
                task.id,
                ContextEntry {
                    role: TOOL_APPROVAL_ROLE.to_string(),
                    content: "человек подтвердил".to_string(),
                    at: Utc::now(),
                    tool_call_id: Some("call_5".to_string()),
                },
            )
            .await
            .unwrap();
        let ctx = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;
        assert!(
            approved_pending_call(&ctx).is_some(),
            "подтверждение не увидено"
        );

        // После исполнения (результат роли tool) — больше не ожидает.
        state
            .task_store
            .append_context(
                task.id,
                ContextEntry {
                    role: TOOL_ROLE.to_string(),
                    content: "{}".to_string(),
                    at: Utc::now(),
                    tool_call_id: Some("call_5".to_string()),
                },
            )
            .await
            .unwrap();
        let ctx = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;
        assert!(
            approved_pending_call(&ctx).is_none(),
            "подтверждённый вызов исполнился бы на каждом круге"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Запрос вызова, переживший паузу, восстанавливается в реплику
    /// ассистента с `tool_calls` — без неё провайдер отвергнет историю.
    #[test]
    fn a_parked_tool_request_rebuilds_into_an_assistant_message() {
        let context = vec![
            ContextEntry {
                role: "user".to_string(),
                content: "сделай".to_string(),
                at: Utc::now(),
                tool_call_id: None,
            },
            ContextEntry {
                role: TOOL_REQUEST_ROLE.to_string(),
                content: serde_json::json!({"name":"get_scene_info","arguments":"{}"}).to_string(),
                at: Utc::now(),
                tool_call_id: Some("call_3".to_string()),
            },
            ContextEntry {
                role: TOOL_ROLE.to_string(),
                content: "результат".to_string(),
                at: Utc::now(),
                tool_call_id: Some("call_3".to_string()),
            },
        ];

        let messages = context_to_messages(&context);
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1].tool_calls.len(),
            1,
            "запрос вызова не восстановлен"
        );
        assert_eq!(messages[1].tool_calls[0].id, "call_3");
        assert_eq!(messages[1].tool_calls[0].name, "get_scene_info");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_3"));
    }

    #[test]
    fn execution_metadata_is_parsed_for_api_clients() {
        let context = vec![ContextEntry {
            role: EXECUTION_METADATA_ROLE.to_string(),
            content: "backend=embedded; model=Fable9B".to_string(),
            at: Utc::now(),
            tool_call_id: None,
        }];

        assert_eq!(
            execution_metadata(&context),
            Some(ExecutionMetadata {
                backend: "embedded".to_string(),
                model: "Fable9B".to_string(),
            })
        );
    }

    #[test]
    fn malformed_or_incomplete_execution_metadata_is_omitted() {
        for content in [
            "backend=embedded",
            "model=Fable9B",
            "backend=; model=Fable9B",
            "not metadata",
        ] {
            let context = vec![ContextEntry {
                role: EXECUTION_METADATA_ROLE.to_string(),
                content: content.to_string(),
                at: Utc::now(),
                tool_call_id: None,
            }];
            assert_eq!(execution_metadata(&context), None, "content: {content}");
        }
    }

    #[test]
    fn task_response_serializes_optional_execution_metadata_without_changing_task_fields() {
        let task = Task {
            id: Uuid::nil(),
            category: "Simple".to_string(),
            status: TaskStatus::Done,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            context: vec![],
            pending_telegram_message_id: None,
        };
        let response = TaskResponse::from(task.clone());
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["id"], serde_json::json!(Uuid::nil().to_string()));
        assert!(json.get("execution").is_none());

        let with_metadata = TaskResponse::from(Task {
            context: vec![ContextEntry {
                role: EXECUTION_METADATA_ROLE.to_string(),
                content: "backend=external; model=remote-model".to_string(),
                at: Utc::now(),
                tool_call_id: None,
            }],
            ..task
        });
        assert_eq!(
            serde_json::to_value(with_metadata).unwrap()["execution"],
            serde_json::json!({
                "backend": "external",
                "model": "remote-model",
            })
        );
    }

    #[test]
    fn execution_metadata_is_not_sent_to_the_model() {
        let context = vec![ContextEntry {
            role: EXECUTION_METADATA_ROLE.to_string(),
            content: "backend=embedded; model=Fable9B".to_string(),
            at: Utc::now(),
            tool_call_id: None,
        }];

        assert!(context_to_messages(&context).is_empty());
    }

    #[test]
    fn the_step_limit_is_a_real_bound_not_a_formality() {
        // Замер 2026-08-13 подтверждал двухшаговые цепочки; предел взят
        // вчетверо больше измеренного. Ноль или единица обесценили бы цикл,
        // а сотня не отличалась бы от бесконечности.
        assert!(MAX_TOOL_ITERATIONS >= 4, "предел меньше измеренных цепочек");
        assert!(MAX_TOOL_ITERATIONS <= 16, "предел уже не ограничивает");
    }

    /// Парная проверка: без неё тест выше зеленел бы и от того, что пайплайн
    /// вообще не доходит до записи статуса — например если бы подбор модели
    /// отказал и до генерации дело не дошло.
    #[tokio::test]
    async fn a_task_nobody_cancelled_still_reaches_done() {
        let (state, dir) = pipeline_fixture(400);
        let task = state.task_store.create("Simple").await.unwrap();

        run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            state.control.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        )
        .await;

        assert_eq!(
            status_of(&state, task.id).await,
            TaskStatus::Done,
            "неотменённая задача обязана дойти до Done"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn video_query_is_the_user_text_verbatim() {
        let messages = vec![
            Message::new(Role::System, "служебное".to_string()),
            Message::new(Role::User, "  горный ручей  ".to_string()),
        ];

        // Дословно, только с обрезкой пробелов: никакого разбора запроса.
        assert_eq!(media_query(&messages), Some("горный ручей".to_string()));
    }

    #[test]
    fn video_task_without_user_text_has_no_query() {
        let messages = vec![Message::new(Role::System, "только служебное".to_string())];
        assert_eq!(media_query(&messages), None);

        let blank = vec![Message::new(Role::User, "   ".to_string())];
        assert_eq!(media_query(&blank), None);
    }

    #[test]
    fn gate_rejection_of_every_asset_yields_error_not_panic() {
        // ND запрещает производные произведения; монтаж — производное.
        // В потоке задачи это должно давать штатную ошибку (→ Failed),
        // а не панику и не пустой успешный план.
        let entries = vec![
            entry("openverse:1", "by-nd", "credit"),
            entry("openverse:2", "by-nc-nd", "credit"),
        ];

        let result = assemble_render_plan(entries);

        let reason = result.expect_err("ND-ассеты не могут дать план");
        assert!(
            reason.contains("гейт не допустил ни одного"),
            "причина: {reason}"
        );
        assert!(
            reason.contains("ND"),
            "причина обязана называть ND: {reason}"
        );
    }

    #[test]
    fn empty_source_result_is_an_error() {
        let reason = assemble_render_plan(Vec::new()).expect_err("пустой ответ — ошибка");
        assert!(reason.contains("ни одного ассета"), "причина: {reason}");
    }

    #[test]
    fn partial_rejection_keeps_good_assets_and_reports_the_rest() {
        // Смешанный результат поиска — типичный случай: часть ассетов годная,
        // часть нет. Задача должна выполниться на годных, а отказы —
        // попасть в отчёт, а не потеряться.
        let entries = vec![
            entry(
                "openverse:good",
                "by",
                "\"X\" by Автор is licensed under CC BY 2.0.",
            ),
            entry("openverse:nd", "by-nd", "credit"),
            entry("openverse:pd", "cc0", ""),
        ];

        let (plan, rejections) = assemble_render_plan(entries).expect("план собирается");

        assert_eq!(plan.asset_ids().len(), 2, "годные ассеты на таймлайне");
        assert_eq!(rejections.len(), 1, "отказ не проглочен");
        assert!(rejections[0].contains("ND"));
        assert!(
            plan.credits_text().contains("Автор"),
            "CC-BY-материал обязан быть в титрах"
        );
    }

    #[test]
    fn cc_by_without_attribution_is_rejected_in_flow() {
        // Гейт не выпустит CC-BY без строки автора — титры собрать нечем.
        let entries = vec![entry("openverse:1", "by", "")];

        let reason = assemble_render_plan(entries).expect_err("нечем делать титры");
        assert!(reason.contains("атрибуции"), "причина: {reason}");
    }
}
