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
mod mcp;
mod media;
mod model_backend;
mod resource_registry;
mod router;
mod scheduler;
mod task_store;
mod telegram_bridge;
mod tool_gate;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;

use capability_registry::CapabilityRegistry;
use control_center_api::{serve, AppState};
use model_backend::LmStudioBackend;
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

    // Секрет Control Center читается ПЕРВЫМ делом — до Resource Registry,
    // до выгрузки моделей и до любого касания GPU. Отсутствие секрета это
    // отказ конфигурации, а не отказ во время работы: узнать о нём надо за
    // миллисекунды, а не после минуты загрузки модели.
    let control_token = control_auth::ControlToken::from_env()?;

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
        TOTAL_MODEL_MEMORY_MB,
    ));
    // Сначала убрать чужое, потом грузить своё. Прошлый процесс мог
    // завершиться как угодно — от Ctrl+C до краха — и оставить в LM Studio
    // загруженные модели, о которых мы ничего не знаем.
    scheduler.unload_stale_instances().await?;
    scheduler.preload_always_loaded().await?;

    // Что система реально может при текущем бюджете. Только сообщение:
    // старт не блокируется, поведение подбора не меняется. Выяснение этих
    // фактов вручную стоило нескольких задач подряд, хотя Scheduler знает
    // их сам с первой секунды.
    scheduler.log_reachability();

    let task_store = Arc::new(TaskStore::new("kaic.db")?);

    // Копия для чистки после serve(): сам scheduler уезжает в AppState.
    let scheduler_for_cleanup = scheduler.clone();

    let state = AppState {
        task_store,
        scheduler,
        resource_registry,
        control: control_center_api::TaskControl::default(),
    };

    // Telegram Bridge — такой же HTTP-клиент Control Center API, как Electron.
    // Поднимается до serve(), чтобы к моменту первого сообщения API уже слушал.
    // Без TELOXIDE_TOKEN просто не запускается: backend обязан работать и без
    // Telegram.
    let telegram_config = telegram_bridge::TelegramConfig::load_or_default("config/telegram.yaml")?;
    telegram_bridge::spawn_if_configured(telegram_config).await;

    let addr: SocketAddr = "127.0.0.1:4545".parse()?;
    let result = serve(state, addr, control_token).await;

    // Дополнение к стартовой чистке, не замена ей: срабатывает только при
    // корректном завершении (Ctrl+C через graceful_shutdown). При kill или
    // краше этот код не выполнится — и это нормально, следствие уберёт
    // чистка при следующем старте.
    if let Err(err) = scheduler_for_cleanup.unload_stale_instances().await {
        tracing::warn!("не удалось выгрузить модели при завершении: {err:#}");
    }

    result
}
