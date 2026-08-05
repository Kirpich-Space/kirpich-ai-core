//! Точка входа KAIC.
//!
//! Только сборка компонентов — никакой логики здесь не живёт.
//! Router нигде не создаётся как объект: у него нет состояния,
//! `Router::classify()` — обычная ассоциированная функция, вызывается
//! напрямую из control_center_api.rs.

mod capability_registry;
mod control_center_api;
#[cfg(feature = "embedded-backend")]
mod embedded_backend;
mod media;
mod model_backend;
mod resource_registry;
mod router;
mod scheduler;
mod task_store;
mod telegram_bridge;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;

use capability_registry::CapabilityRegistry;
use control_center_api::{serve, AppState};
use model_backend::LmStudioBackend;
use resource_registry::ResourceRegistry;
use scheduler::Scheduler;
use task_store::TaskStore;

/// Объём VRAM на текущей машине (RTX 4060). Меняется здесь одной строкой,
/// если железо сменится — Scheduler больше нигде это число не хранит.
const TOTAL_VRAM_MB: u32 = 8000;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let capability_registry = Arc::new(CapabilityRegistry::from_file(
        "config/capability_registry.yaml",
    )?);
    let resource_registry = Arc::new(ResourceRegistry::from_file(
        "config/resource_registry.yaml",
    )?);

    let backend = Arc::new(LmStudioBackend::new("http://localhost:1234", None));

    let scheduler = Arc::new(Scheduler::new(
        capability_registry,
        resource_registry.clone(),
        backend,
        TOTAL_VRAM_MB,
    ));
    // Сначала убрать чужое, потом грузить своё. Прошлый процесс мог
    // завершиться как угодно — от Ctrl+C до краха — и оставить в LM Studio
    // загруженные модели, о которых мы ничего не знаем.
    scheduler.unload_stale_instances().await?;
    scheduler.preload_always_loaded().await?;

    let task_store = Arc::new(TaskStore::new("kaic.db")?);

    // Копия для чистки после serve(): сам scheduler уезжает в AppState.
    let scheduler_for_cleanup = scheduler.clone();

    let state = AppState {
        task_store,
        scheduler,
        resource_registry,
    };

    // Telegram Bridge — такой же HTTP-клиент Control Center API, как Electron.
    // Поднимается до serve(), чтобы к моменту первого сообщения API уже слушал.
    // Без TELOXIDE_TOKEN просто не запускается: backend обязан работать и без
    // Telegram.
    let telegram_config = telegram_bridge::TelegramConfig::load_or_default("config/telegram.yaml")?;
    telegram_bridge::spawn_if_configured(telegram_config).await;

    let addr: SocketAddr = "127.0.0.1:4545".parse()?;
    let result = serve(state, addr).await;

    // Дополнение к стартовой чистке, не замена ей: срабатывает только при
    // корректном завершении (Ctrl+C через graceful_shutdown). При kill или
    // краше этот код не выполнится — и это нормально, следствие уберёт
    // чистка при следующем старте.
    if let Err(err) = scheduler_for_cleanup.unload_stale_instances().await {
        tracing::warn!("не удалось выгрузить модели при завершении: {err:#}");
    }

    result
}
