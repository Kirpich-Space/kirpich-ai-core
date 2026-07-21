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

use crate::model_backend::{GenerateRequest, Message, Role};
use crate::resource_registry::ResourceRegistry;
// Алиас: axum тоже экспортирует тип `Router`, поэтому наш классификатор
// задач импортируется под другим именем — иначе имена конфликтуют.
use crate::router::{Router as TaskRouter, TaskMetadata};
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
        .route("/models", get(list_models))
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
            },
        )
        .await?;

    let refreshed = state
        .task_store
        .get(task.id)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("задача исчезла сразу после создания")))?;

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

/// Переводит накопленный контекст задачи в историю сообщений для модели.
/// Роли, не относящиеся напрямую к диалогу (например "system" — заметки
/// об ошибках), сопоставляются с `Role::System`.
fn context_to_messages(context: &[ContextEntry]) -> Vec<Message> {
    context
        .iter()
        .map(|entry| Message {
            role: match entry.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::System,
            },
            content: entry.content.clone(),
        })
        .collect()
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
    let request = GenerateRequest { messages };

    match scheduler.run(&category, allow_manual, request).await {
        Ok(response) => {
            let append_result = task_store
                .append_context(
                    task_id,
                    ContextEntry {
                        role: "assistant".to_string(),
                        content: response.content,
                        at: Utc::now(),
                    },
                )
                .await;
            if let Err(err) = append_result {
                tracing::error!("не удалось сохранить ответ модели для задачи {task_id}: {err:#}");
            }
            if let Err(err) = task_store.set_status(task_id, TaskStatus::Done).await {
                tracing::error!("не удалось обновить статус задачи {task_id}: {err:#}");
            }
        }
        Err(err) => {
            tracing::warn!("задача {task_id} завершилась ошибкой: {err:#}");
            let append_result = task_store
                .append_context(
                    task_id,
                    ContextEntry {
                        role: "system".to_string(),
                        content: format!("Ошибка: {err:#}"),
                        at: Utc::now(),
                    },
                )
                .await;
            if let Err(e) = append_result {
                tracing::error!("не удалось сохранить ошибку для задачи {task_id}: {e:#}");
            }
            if let Err(e) = task_store.set_status(task_id, TaskStatus::Failed).await {
                tracing::error!("не удалось обновить статус задачи {task_id}: {e:#}");
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
        })
        .collect();

    Json(models)
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
    total_vram_mb: u32,
    used_vram_mb: u32,
    loaded_models: Vec<LoadedModelDto>,
}

async fn status(State(state): State<AppState>) -> Json<StatusDto> {
    let loaded = state.scheduler.snapshot().await;

    let used_vram_mb: u32 = loaded
        .iter()
        .filter_map(|(name, _)| state.resource_registry.get(name))
        .map(|r| r.vram_mb)
        .sum();

    let loaded_models = loaded
        .into_iter()
        .map(|(model, last_used)| LoadedModelDto { model, last_used })
        .collect();

    Json(StatusDto {
        total_vram_mb: state.scheduler.total_vram_mb(),
        used_vram_mb,
        loaded_models,
    })
}

// --- Ошибки API ---

/// Единая обёртка ошибок для обработчиков: либо "не найдено" (404),
/// либо любая внутренняя ошибка (500) с текстом причины.
enum ApiError {
    NotFound,
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
            ApiError::Internal(err) => {
                tracing::error!("Control Center API: {err:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
            }
        }
    }
}
