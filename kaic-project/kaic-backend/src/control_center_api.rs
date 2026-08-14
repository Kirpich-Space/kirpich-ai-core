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
use crate::model_backend::{GenerateRequest, Message, Role};
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
                    },
                )
                .await
            {
                tracing::error!("не удалось сохранить путь результата {task_id}: {err:#}");
            }

            if let Err(err) = task_store
                .append_context(
                    task_id,
                    ContextEntry {
                        role: "assistant".to_string(),
                        content: summary,
                        at: Utc::now(),
                    },
                )
                .await
            {
                tracing::error!("не удалось сохранить план для задачи {task_id}: {err:#}");
            }
            if let Err(err) = task_store.set_status(task_id, TaskStatus::Done).await {
                tracing::error!("не удалось обновить статус задачи {task_id}: {err:#}");
            }
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
        Ok(response) => response.content,
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
    if let Err(err) = task_store.set_status(task_id, TaskStatus::Failed).await {
        tracing::error!("не удалось обновить статус задачи {task_id}: {err:#}");
    }
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

    // temperature подставит Scheduler: только он знает, какая модель
    // в итоге возьмёт задачу.
    let request = GenerateRequest { messages, temperature: None };

    let started = std::time::Instant::now();
    match scheduler.run(&category, allow_manual, request).await {
        Ok(response) => {
            // Длина ответа, а не ответ: размер полезен для диагностики,
            // содержимое в лог не идёт.
            tracing::info!(
                "задача {task_id} завершена: done за {:.1} с, ответ {} символов",
                started.elapsed().as_secs_f64(),
                response.content.chars().count()
            );
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
            tracing::warn!(
                "задача {task_id} завершена: failed за {:.1} с — {err:#}",
                started.elapsed().as_secs_f64()
            );
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

    #[test]
    fn video_query_is_the_user_text_verbatim() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "служебное".to_string(),
            },
            Message {
                role: Role::User,
                content: "  горный ручей  ".to_string(),
            },
        ];

        // Дословно, только с обрезкой пробелов: никакого разбора запроса.
        assert_eq!(media_query(&messages), Some("горный ручей".to_string()));
    }

    #[test]
    fn video_task_without_user_text_has_no_query() {
        let messages = vec![Message {
            role: Role::System,
            content: "только служебное".to_string(),
        }];
        assert_eq!(media_query(&messages), None);

        let blank = vec![Message {
            role: Role::User,
            content: "   ".to_string(),
        }];
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
