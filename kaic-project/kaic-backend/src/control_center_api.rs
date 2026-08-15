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
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::media::credits::{plan_render, RenderPlan};
use crate::media::fetch as media_fetch;
use crate::media::gate::{admit, AdmittedAsset, Timeline};
use crate::media::openverse;
use crate::media::provenance::{Manifest, ManifestEntry};
use crate::media::query as media_query_extract;
use crate::media::render as media_render;
use crate::mcp::{McpClient, McpServerConfig};
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
}

/// Собирает маршруты Control Center API.
pub fn build_router(state: AppState) -> axum::Router {
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
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Запускает Control Center API на указанном адресе.
/// Вызывается один раз из `main.rs`.
pub async fn serve(state: AppState, addr: SocketAddr) -> anyhow::Result<()> {
    let app = build_router(state);
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

async fn list_tasks(State(state): State<AppState>) -> Result<Json<Vec<Task>>, ApiError> {
    Ok(Json(state.task_store.list().await?))
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
}

async fn create_task(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<Json<Task>, ApiError> {
    let category = match &body.category {
        Some(c) => c.clone(),
        None => {
            let metadata = TaskMetadata::default();
            TaskRouter::classify(&body.text, &metadata).as_str().to_string()
        }
    };
    let allow_manual = body.allow_manual.unwrap_or(false);

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

    let refreshed = state
        .task_store
        .get(task.id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("задача исчезла сразу после создания")))?;

    // Текст задачи в лог НЕ идёт: он приходит в том числе из Telegram и
    // может содержать личные данные. В логе только идентификатор и категория.
    tracing::info!(
        "задача {} создана: категория '{}'{}",
        task.id,
        category,
        if body.category.is_some() { "" } else { " (определена Router'ом)" }
    );

    // Задача уже сохранена в статусе Running — Electron получает её сразу
    // и дальше следит за изменениями через GET /tasks/{id}. Сама работа
    // (подбор модели + генерация) идёт в фоне, а не блокирует этот запрос:
    // для L4/L5 моделей она может занять от секунд до минут, и Task Store
    // как раз для этого и существует.
    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
        task.id,
        category,
        context_to_messages(&refreshed.context),
        allow_manual,
    ));

    Ok(Json(refreshed))
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
fn admit_all(entries: Vec<ManifestEntry>) -> Result<(Manifest, Vec<AdmittedAsset>, Vec<String>), String> {
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

    let query = extract_search_term(&scheduler, &task_store, task_id, &raw_text).await;

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
                fail_task(&task_store, task_id, format!("источник недоступен: {err:#}")).await;
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

            // Путь к результату отдельной записью, машиночитаемо: сводка
            // выше предназначена человеку, и выкусывать из неё путь регуляркой
            // означало бы завязать клиента на формат текста.
            if let Err(err) = task_store
                .append_context(
                    task_id,
                    ContextEntry {
                        role: ARTIFACT_ROLE.to_string(),
                        content: rendered.path.to_string_lossy().to_string(),
                        at: Utc::now(),
                        tool_call_id: None,
                    },
                )
                .await
            {
                tracing::error!("не удалось сохранить путь результата {task_id}: {err:#}");
            }

            // Тот же порядок, что и в текстовом пайплайне: сначала заявка на
            // статус, потом роль записи. Видео-задачу отменяют ровно так же,
            // как текстовую, и результат отменённой не должен выглядеть
            // ответом.
            record_outcome(&task_store, task_id, TaskStatus::Done, summary).await;
        }
        // Отказ гейта на всех ассетах — это Failed с внятной причиной, а не
        // паника: задача завершается штатно, причина видна в контексте.
        Err(reason) => fail_task(&task_store, task_id, reason).await,
    }
}

/// Превращает свободный текст задачи в поисковый термин силами модели.
///
/// Шаг необязательный по построению: любая неудача — недоступная модель,
/// пустой или неправдоподобный ответ — возвращает исходный текст дословно,
/// то есть поведение откатывается к прежнему, а не ломается.
async fn extract_search_term(
    scheduler: &Arc<Scheduler>,
    task_store: &Arc<TaskStore>,
    task_id: Uuid,
    raw_text: &str,
) -> String {
    let started = std::time::Instant::now();
    let request = media_query_extract::build_request(raw_text);

    let raw = match scheduler
        .run_with_model(media_query_extract::EXTRACTION_MODEL_LABEL, request)
        .await
    {
        // Инструментов этому шагу не давали, поэтому ответ не-текстом здесь
        // невозможен. Если он всё же придёт — это отказ, а не повод
        // домысливать: шаг по построению деградирующий, берём дословный текст.
        Ok(response) => match response.text() {
            Some(text) => text.to_string(),
            None => {
                note(
                    task_store,
                    task_id,
                    "модель запросила вызов инструмента там, где инструментов нет; \
                     ищу по тексту задачи"
                        .to_string(),
                )
                .await;
                return raw_text.to_string();
            }
        },
        Err(err) => {
            note(
                task_store,
                task_id,
                format!("извлечение термина не выполнено ({err:#}); ищу по тексту задачи"),
            )
            .await;
            return raw_text.to_string();
        }
    };

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
            term
        }
        None => {
            note(
                task_store,
                task_id,
                format!("модель не дала пригодный термин; ищу по тексту задачи «{raw_text}»"),
            )
            .await;
            raw_text.to_string()
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

/// Кладёт результат фоновой работы в задачу — ответом, если задача его ещё
/// ждёт, и помеченной заметкой, если человек успел от неё отказаться.
///
/// Молча выбросить результат было бы новым молчаливым отказом — тем самым
/// классом, который здесь и чинится. Через месяц никто не поймёт, почему GPU
/// был занят полчаса на отменённой задаче, если от этой работы не осталось
/// следа. Поэтому текст сохраняется всегда; меняется только его роль.
///
/// Роль `system`, а не `assistant`, и это не косметика: Telegram-мост берёт
/// ПОСЛЕДНЮЮ запись роли `assistant` и отправляет её человеку как ответ.
/// Результат отменённой задачи, положенный ролью `assistant`, уехал бы в
/// Telegram как полноценный ответ — то есть отмена снова была бы отменена,
/// только другим путём.
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
    allow_manual: bool,
    mut messages: Vec<Message>,
) -> anyhow::Result<LoopOutcome> {
    let config = McpServerConfig::load(MCP_CONFIG_PATH)?;
    // Список разрешённых читается из конфига и только оттуда. Пустой список —
    // подтверждать всё; отсутствие конфига — инструментов нет вовсе.
    let auto_approve = config
        .as_ref()
        .map(|c| c.auto_approve.clone())
        .unwrap_or_default();

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

    let tools = match &mcp {
        Some(client) => {
            let tools = client.list_tools().await?;
            note(
                task_store,
                task_id,
                format!(
                    "инструменты от '{}': {}. Без подтверждения разрешены: {}",
                    client.server_name(),
                    tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "),
                    if auto_approve.is_empty() {
                        "ничего — каждый вызов требует решения человека".to_string()
                    } else {
                        auto_approve.join(", ")
                    }
                ),
            )
            .await;
            tools
        }
        None => Vec::new(),
    };

    // Сессия, а не `run`: guard обязан пережить все шаги цикла, включая
    // паузы на работу инструмента, иначе модель вытеснят между шагами.
    let session = scheduler.begin_session(category, allow_manual).await?;

    let outcome = agent_steps(
        task_store,
        &session,
        task_id,
        &mut messages,
        &tools,
        mcp.as_ref(),
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
    auto_approve: &[String],
    server_name: &str,
) -> anyhow::Result<LoopOutcome> {
    // Возобновление после «да»: подтверждённый вызов исполняется ДО того, как
    // модель спросят снова. Иначе она увидела бы собственный запрос без
    // результата и либо повторила бы его, либо получила бы отвергнутую
    // провайдером историю.
    if let Some(call) = approved_pending_call(&task_store.get(task_id).await?.map(|t| t.context).unwrap_or_default()) {
        let Some(client) = mcp else {
            anyhow::bail!("вызов подтверждён, но MCP-сервер недоступен");
        };
        // Единственный путь человека к `ApprovedToolCall`.
        let approved = crate::tool_gate::approve_by_human(call);
        let result = execute_tool(task_store, task_id, client, &approved, 0).await;
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
            GenerateResponse::Text(text) => return Ok(LoopOutcome::Answer(text)),
            GenerateResponse::ToolCalls(calls) => {
                let Some(client) = mcp else {
                    anyhow::bail!(
                        "модель запросила вызов инструмента, но MCP-сервер недоступен"
                    );
                };

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
                                execute_tool(task_store, task_id, client, &approved, step).await;
                            messages.push(Message::tool_result(
                                approved.call().id.clone(),
                                result,
                            ));
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
    let request = context
        .iter()
        .rev()
        .find(|e| e.role == TOOL_REQUEST_ROLE)?;
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
    let approved = context
        .iter()
        .any(|e| e.role == TOOL_APPROVAL_ROLE && e.tool_call_id.as_deref() == Some(call.id.as_str()));
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
    note(
        task_store,
        task_id,
        crate::tool_gate::describe(server_name, call),
    )
    .await;

    tracing::info!(
        "задача {task_id}: вызов '{}' ждёт решения человека ({} символов аргументов)",
        call.name,
        call.arguments.chars().count()
    );

    task_store
        .set_status(task_id, TaskStatus::WaitingForHuman)
        .await?;
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
    client: &McpClient,
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
    let (result, outcome) = match client.call_tool(&call.name, arguments).await {
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
        client.server_name(),
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

/// Фоновый пайплайн: Scheduler подбирает и вызывает модель, результат
/// (или ошибка) дописывается в контекст задачи, статус обновляется.
/// Используется и при создании задачи, и при её продолжении после
/// решения пользователя — в обоих случаях на вход идёт полная накопленная
/// история сообщений, а не только последняя реплика.
async fn run_task_pipeline(
    task_store: Arc<TaskStore>,
    scheduler: Arc<Scheduler>,
    task_id: Uuid,
    category: String,
    messages: Vec<Message>,
    allow_manual: bool,
) {
    // Первое и пока единственное ветвление по категории. До этого все
    // категории шли одним путём (текст → модель → текст), и Video ничем не
    // отличалась от Programming, хотя существовала в Router'е.
    //
    // Video не обращается к модели вообще: медиа-пайплайн — это поиск в
    // лицензионном источнике, гейт и сборка плана, а не генерация текста.
    if category == Category::Video.as_str() {
        run_video_pipeline(task_store, scheduler, task_id, &messages).await;
        return;
    }

    let started = std::time::Instant::now();
    match run_agent_loop(&task_store, &scheduler, task_id, &category, allow_manual, messages).await {
        // Задача встала на паузу ради решения человека. Статус уже
        // `WaitingForHuman`, и трогать его нельзя: это не завершение.
        Ok(LoopOutcome::AwaitingHuman) => {
            tracing::info!(
                "задача {task_id} ждёт решения человека по вызову инструмента ({:.1} с работы)",
                started.elapsed().as_secs_f64()
            );
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
            record_outcome(&task_store, task_id, TaskStatus::Done, response).await;
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
            if !claim_finish(&task_store, task_id, TaskStatus::Failed).await {
                tracing::warn!(
                    "задача {task_id}: отказ пришёл после того, как статус сменил человек — \
                     решение человека сохранено"
                );
            }
        }
    }
}

async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Task>, ApiError> {
    match state.task_store.get(id).await? {
        Some(task) => Ok(Json(task)),
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

    state.task_store.set_pending_telegram_message(id, None).await?;
    state.task_store.set_status(id, TaskStatus::Running).await?;

    // Перечитываем задачу — контекст уже включает только что добавленную
    // реплику пользователя, и именно эта полная история идёт в пайплайн,
    // а не только последнее сообщение.
    let refreshed = state
        .task_store
        .get(id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("задача исчезла во время продолжения")))?;

    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
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
    let task = state
        .task_store
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;

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
    let refreshed = state
        .task_store
        .get(id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("задача исчезла во время решения")))?;

    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
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
    state.task_store.set_status(id, TaskStatus::Cancelled).await?;
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
    let loaded: HashMap<String, DateTime<Utc>> = state.scheduler.snapshot().await.into_iter().collect();

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
    tracing::info!("temperature модели '{name}' изменена на {:?}", body.temperature);
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

    let active_operation = state.scheduler.active_operation().map(|op| ActiveOperationDto {
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

    Json(StatusDto {
        total_model_memory_mb: state.scheduler.total_model_memory_mb(),
        used_model_memory_mb,
        loaded_models,
        running_tasks: state.scheduler.running_generations(),
        max_concurrent_tasks: crate::scheduler::MAX_CONCURRENT_TASKS,
        busy_models,
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
            Ok(crate::model_backend::GenerateResponse::Text("ответ модели".to_string()))
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

    /// Результат, досчитанный после отмены, обязан остаться в задаче — но не
    /// ответом.
    ///
    /// Молча выбросить его было бы новым молчаливым отказом. Положить его
    /// ролью `assistant` — тоже отказ, только хитрее: Telegram-мост берёт
    /// последнюю запись этой роли и отправляет её человеку, то есть отменённая
    /// задача всё равно вернула бы ответ.
    #[tokio::test]
    async fn work_finished_after_a_cancel_is_kept_but_not_as_the_answer() {
        let (state, dir) = pipeline_fixture(400);
        let task = state.task_store.create("Simple").await.unwrap();

        let pipeline = tokio::spawn(run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
            task.id,
            "Simple".to_string(),
            Vec::new(),
            false,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = cancel_task(State(state.clone()), Path(task.id)).await;
        pipeline.await.expect("пайплайн не паниковал");

        let context = state
            .task_store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .context;

        assert!(
            context.iter().all(|e| e.role != "assistant"),
            "результат отменённой задачи лёг ответом — Telegram отправит его человеку"
        );
        let trace = context
            .iter()
            .find(|e| e.content.contains("ответ модели"))
            .expect("работа, которую сделал GPU, обязана оставить след");
        assert_eq!(trace.role, "system");
        assert!(
            trace.content.contains("отменена"),
            "след не объясняет, почему это не ответ: {}",
            trace.content
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
            let n = self
                .seen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(GenerateResponse::ToolCalls(vec![
                crate::model_backend::ToolCall {
                    id: format!("call_{n}"),
                    name: "get_scene_info".to_string(),
                    arguments: "{}".to_string(),
                },
            ]))
        }
    }

    fn looping_fixture() -> (AppState, std::sync::Arc<std::sync::atomic::AtomicUsize>, std::path::PathBuf) {
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
        std::fs::write(dir.join("capability_registry.yaml"), "Simple:\n  primary: Fake\n").unwrap();

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

        // Инструменты недоступны (конфига MCP нет), поэтому цикл обязан
        // отказать сразу и назвать причину — это тоже внятный отказ, а не
        // молчаливый обрыв.
        run_task_pipeline(
            state.task_store.clone(),
            state.scheduler.clone(),
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
            reason.content.contains("MCP-сервер недоступен"),
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

        let found = pending_tool_request(&state.task_store.get(task.id).await.unwrap().unwrap().context)
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

        let context = state.task_store.get(task.id).await.unwrap().unwrap().context;
        let refusal = context
            .iter()
            .find(|e| e.role == TOOL_ROLE && e.tool_call_id.as_deref() == Some("call_9"))
            .expect("отказ обязан лечь ролью tool с тем же tool_call_id");
        assert!(
            refusal.content.contains("ОТКАЗАНО ЧЕЛОВЕКОМ"),
            "модель не поймёт, что это отказ: {}", refusal.content
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

        let ctx = state.task_store.get(task.id).await.unwrap().unwrap().context;
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
        let ctx = state.task_store.get(task.id).await.unwrap().unwrap().context;
        assert!(approved_pending_call(&ctx).is_some(), "подтверждение не увидено");

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
        let ctx = state.task_store.get(task.id).await.unwrap().unwrap().context;
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
        assert_eq!(messages[1].tool_calls.len(), 1, "запрос вызова не восстановлен");
        assert_eq!(messages[1].tool_calls[0].id, "call_3");
        assert_eq!(messages[1].tool_calls[0].name, "get_scene_info");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_3"));
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
        assert!(reason.contains("гейт не допустил ни одного"), "причина: {reason}");
        assert!(reason.contains("ND"), "причина обязана называть ND: {reason}");
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
            entry("openverse:good", "by", "\"X\" by Автор is licensed under CC BY 2.0."),
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
