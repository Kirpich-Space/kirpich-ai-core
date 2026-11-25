//! Точка входа KAIC.
//!
//! Только сборка компонентов — никакой логики здесь не живёт.
//! Router нигде не создаётся как объект: у него нет состояния,
//! `Router::classify()` — обычная ассоциированная функция, вызывается
//! напрямую из control_center_api.rs.

mod capability_registry;
mod control_auth;
mod control_center_api;
#[cfg(feature = "embedded-backend")]
mod embedded_backend;
mod http_client;
mod llama_server;
mod mcp;
mod media;
mod model_backend;
mod model_directory;
mod resource_registry;
mod router;
mod scheduler;
mod task_store;
// Старый bridge сохранён для регрессий; пользовательский вход теперь только CLI.
#[cfg(test)]
mod telegram_bridge;
mod tool_gate;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;

use capability_registry::CapabilityRegistry;
use control_center_api::{serve, AppState};
use model_backend::{LmStudioBackend, ModelBackend};
use resource_registry::ResourceRegistry;
use scheduler::Scheduler;
use task_store::TaskStore;

/// Память, доступная моделям на этой машине, в мегабайтах.
///
/// ЭТО НЕ ВИДЕОПАМЯТЬ. Раньше здесь стояла ёмкость GPU (8000 под RTX 4060),
/// и Scheduler сравнивал её с `vram_mb` моделей — а `vram_mb` означает полный
/// footprint при НУЛЕВОМ оффлоаде. Сравнивались несопоставимые величины, и
/// никакое значение константы этого не чинило: подъём числа оставил бы имя
/// врущим. Поэтому сменилась величина, а не только число.
///
/// Формула (замеры 2026-08-07 на этой машине):
///
///     (RAM_всего - РЕЗЕРВ) + VRAM = (32691 - 16384) + 8188 = 24495
///
/// RAM_всего 32691 MiB — `Win32_OperatingSystem.TotalVisibleMemorySize`.
/// VRAM 8188 MiB — `nvidia-smi memory.total`. Обе величины — ёмкость железа,
/// а не снимок свободного места: снимок нестабилен, и на этом мы уже
/// обжигались с `vram_mb`.
///
/// РЕЗЕРВ 16384 MiB (16 GiB) — под ОС и средства разработки. Обоснован
/// наблюдением, а не догадкой: в момент замера занято 21456 MiB, из них
/// 5640 MiB держит LM Studio под резидентную модель, то есть базовое
/// потребление без моделей — 15816 MiB (ОС, Defender, WSL, Cursor, Chrome,
/// агент). Округлено вверх до 16 GiB — ровно одна из двух планок памяти.
/// Округление намеренное: резерв обязан быть числом политики, которое не
/// меняется от того, что показал очередной снимок.
///
/// Оффлоад на 8 ГБ видеопамяти — нормальный режим работы этой машины, а не
/// деградация. После смены величины он перестал ломать расчёт.
///
/// ЧЕГО ЭТО ЧИСЛО НЕ МОДЕЛИРУЕТ: скорость. Модель, помещающаяся в бюджет
/// целиком через оперативную память, пройдёт проверку и будет отвечать
/// медленно. Различия "быстро в видеопамяти / медленно через оффлоад"
/// в системе нет вовсе — до этой правки его не было тоже, просто теперь
/// это видно. Вводить его — отдельная задача.
const TOTAL_MODEL_MEMORY_MB: u32 = 24495;

#[tokio::main]
async fn main() -> Result<()> {
    // `fmt::init()` при незаданном RUST_LOG пропускает только ERROR — то есть
    // весь info!/warn! приложения молчал, включая стартовую диагностику и
    // предупреждения Scheduler'а о неудачных загрузках. Умолчание должно быть
    // info: логи существуют, чтобы их читали. RUST_LOG по-прежнему главнее.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Offline setup touches only the selected model directory, without providers.
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--setup-models" {
        let catalog = model_directory::ModelDirectory::from_env()?.scan()?;
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }
    anyhow::ensure!(args.is_empty(), "допустимый аргумент: --setup-models");

    // Секрет Control Center читается ПЕРВЫМ делом — до Resource Registry,
    // до выгрузки моделей и до любого касания GPU. Отсутствие секрета это
    // отказ конфигурации, а не отказ во время работы: узнать о нём надо за
    // миллисекунды, а не после минуты загрузки модели.
    let control_token = control_auth::ControlToken::from_env()?;

    // Второй процесс на том же endpoint отказывается до сверки чужих задач.
    let addr: SocketAddr = "127.0.0.1:4545".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let catalog = model_directory::ModelDirectory::from_env()?.scan()?;
    tracing::info!(
        "каталог моделей: {}; GGUF с весами: {}; отклонено: {}",
        catalog.root.display(),
        catalog.models.len(),
        catalog.rejected.len()
    );
    for rejected in &catalog.rejected {
        tracing::warn!(
            "GGUF {} отклонён: {}",
            rejected.path.display(),
            rejected.error
        );
    }

    // Сверка выполняется до любых исполнителей и до обращения к провайдеру.
    let task_store = Arc::new(TaskStore::new("kaic.db")?);
    let recovered = task_store.reconcile_startup().await?;
    tracing::info!("стартовая сверка: восстановлено состояний задач {recovered}");

    let mut capabilities = CapabilityRegistry::from_file(
        std::env::var("KAIC_CAPABILITY_REGISTRY")
            .unwrap_or_else(|_| "config/capability_registry.yaml".into()),
    )?;
    if let Ok(model) = std::env::var("KAIC_PREFERRED_MODEL") {
        capabilities.prefer_model(&model)?;
    }
    let capability_registry = Arc::new(capabilities);
    let resource_registry = Arc::new(
        ResourceRegistry::from_file(
            std::env::var("KAIC_RESOURCE_REGISTRY")
                .unwrap_or_else(|_| "config/resource_registry.yaml".into()),
        )?
        .with_local_catalog(catalog),
    );
    let backend_name =
        std::env::var("KAIC_MODEL_BACKEND").unwrap_or_else(|_| "llama-server".into());
    let token = std::env::var("KAIC_INFERENCE_TOKEN").ok();
    let backend: Arc<dyn ModelBackend> = match backend_name.as_str() {
        "llama-server" => Arc::new(llama_server::LlamaServerBackend::new(
            &std::env::var("KAIC_INFERENCE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
            token,
        )?),
        "lm-studio" => Arc::new(LmStudioBackend::new(
            std::env::var("KAIC_INFERENCE_URL").unwrap_or_else(|_| "http://localhost:1234".into()),
            token,
        )),
        _ => anyhow::bail!("KAIC_MODEL_BACKEND должен быть llama-server или lm-studio"),
    };
    let memory_budget = std::env::var("KAIC_MODEL_MEMORY_MB")
        .map(|value| value.parse::<u32>())
        .unwrap_or(Ok(TOTAL_MODEL_MEMORY_MB))?;
    anyhow::ensure!(
        memory_budget > 0,
        "KAIC_MODEL_MEMORY_MB должен быть больше нуля"
    );
    let external_lifecycle = backend.externally_managed();

    let scheduler = Arc::new(Scheduler::new(
        capability_registry,
        resource_registry.clone(),
        backend,
        memory_budget,
    ));
    // Удаляем только инстансы с подтверждённым владением KAIC.
    // Неизвестные инстансы прошлого процесса не считаются нашими.
    scheduler.unload_stale_instances().await?;
    if external_lifecycle {
        if let Err(error) = scheduler.preload_always_loaded().await {
            // CLI должен позволять диагностировать отсутствующий inference
            // process. Задачи всё равно проверят endpoint перед резервированием.
            tracing::warn!("внешний inference backend не готов: {error:#}; диагностика доступна через CLI /backend");
        }
    } else {
        scheduler.preload_always_loaded().await?;
    }

    // Что система реально может при текущем бюджете. Только сообщение:
    // старт не блокируется, поведение подбора не меняется. Выяснение этих
    // фактов вручную стоило нескольких задач подряд, хотя Scheduler знает
    // их сам с первой секунды.
    scheduler.log_reachability();

    // Копия для чистки после serve(): сам scheduler уезжает в AppState.
    let scheduler_for_cleanup = scheduler.clone();

    let state = AppState {
        task_store,
        scheduler,
        resource_registry,
        control: control_center_api::TaskControl::default(),
    };

    let result = serve(state, listener, control_token).await;

    // Дополнение к стартовой чистке, не замена ей: срабатывает только при
    // корректном завершении (Ctrl+C через graceful_shutdown). При kill или
    // краше этот код не выполнится — и это нормально, следствие уберёт
    // чистка при следующем старте.
    if scheduler_for_cleanup.running_generations() != 0
        || scheduler_for_cleanup.active_operation().is_some()
        || !scheduler_for_cleanup.recovery_warnings().is_empty()
    {
        tracing::warn!(
            "модели не выгружены: исполнители или неопределённые операции ещё требуют проверки"
        );
    } else if let Err(err) = scheduler_for_cleanup.unload_stale_instances().await {
        tracing::warn!("не удалось выгрузить модели при завершении: {err:#}");
    }

    result
}
