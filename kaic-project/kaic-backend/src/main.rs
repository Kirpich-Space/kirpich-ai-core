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
#[cfg(feature = "embedded-backend")]
mod llama_common_adapter;
mod mcp;
mod media;
mod model_backend;
mod resource_registry;
mod router;
mod scheduler;
mod task_store;
mod telegram_bridge;
mod terminal;
mod tool_gate;
mod workspace_tools;

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(feature = "embedded-backend")]
use anyhow::Context;
use anyhow::Result;

#[cfg(feature = "embedded-backend")]
use embedded_backend::EmbeddedBackend;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Serve,
    Terminal,
}

fn parse_run_mode(args: &[&str]) -> anyhow::Result<RunMode> {
    match args {
        [] => Ok(RunMode::Serve),
        ["--terminal" | "terminal"] => Ok(RunMode::Terminal),
        _ => anyhow::bail!("использование: kaic [--terminal|terminal]"),
    }
}

fn should_preload_always_loaded(mode: RunMode) -> bool {
    mode == RunMode::Serve
}

fn should_shutdown_scheduler(mode: RunMode) -> bool {
    mode == RunMode::Serve
}

fn default_log_filter(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Serve => "info",
        RunMode::Terminal => "error",
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let run_mode = parse_run_mode(&arg_refs)?;

    // Server сохраняет подробную диагностику. Интерактивный terminal по
    // умолчанию показывает только ошибки, чтобы служебные Scheduler-логи не
    // ломали интерфейс; RUST_LOG по-прежнему главнее этого выбора.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(default_log_filter(run_mode))
            }),
        )
        .init();

    // Секрет нужен только сетевому Control Center. Локальный terminal mode не
    // открывает порт и не должен требовать фиктивный токен.
    let control_token = match run_mode {
        RunMode::Serve => Some(control_auth::ControlToken::from_env()?),
        RunMode::Terminal => None,
    };

    let capability_registry = Arc::new(CapabilityRegistry::from_file(
        "config/capability_registry.yaml",
    )?);
    let resource_registry = Arc::new(ResourceRegistry::from_file(
        "config/resource_registry.yaml",
    )?);

    let backend_name = env::var("KAIC_BACKEND").unwrap_or_else(|_| "lmstudio".to_string());
    let backend: Arc<dyn ModelBackend> = match backend_name.as_str() {
        "lmstudio" => Arc::new(LmStudioBackend::new("http://localhost:1234", None)),
        "embedded" => {
            #[cfg(feature = "embedded-backend")]
            {
                let model_path = env::var("KAIC_EMBEDDED_MODEL_PATH")
                    .context("KAIC_EMBEDDED_MODEL_PATH обязателен для KAIC_BACKEND=embedded")?;
                let gpu_layers = env::var("KAIC_EMBEDDED_GPU_LAYERS")
                    .unwrap_or_else(|_| "0".to_string())
                    .parse()
                    .context("KAIC_EMBEDDED_GPU_LAYERS должен быть целым числом")?;
                Arc::new(EmbeddedBackend::new(model_path, gpu_layers)?)
            }
            #[cfg(not(feature = "embedded-backend"))]
            {
                anyhow::bail!("KAIC_BACKEND=embedded требует сборку с --features embedded-backend")
            }
        }
        other => {
            anyhow::bail!("неизвестный KAIC_BACKEND: {other} (ожидался lmstudio или embedded)")
        }
    };

    let scheduler = Arc::new(Scheduler::new(
        capability_registry,
        resource_registry.clone(),
        backend,
        TOTAL_MODEL_MEMORY_MB,
    ));
    // Server очищает чужие инстансы и заранее грузит always_loaded модели.
    // Терминал должен открыться до первого обращения к провайдеру: Scheduler
    // выполнит необходимую загрузку по первому пользовательскому запросу.
    if should_preload_always_loaded(run_mode) {
        scheduler.unload_stale_instances().await?;
        scheduler.preload_always_loaded().await?;
    }

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

    let result = match run_mode {
        RunMode::Serve => {
            // Telegram Bridge — такой же HTTP-клиент Control Center API, как Electron.
            // Без TELOXIDE_TOKEN backend продолжает работать без Telegram.
            let telegram_config =
                telegram_bridge::TelegramConfig::load_or_default("config/telegram.yaml")?;
            telegram_bridge::spawn_if_configured(telegram_config).await;

            let addr: SocketAddr = "127.0.0.1:4545".parse()?;
            serve(
                state,
                addr,
                control_token.expect("serve mode always has a control token"),
            )
            .await
        }
        RunMode::Terminal => terminal::run(&state).await,
    };

    // Дополнение к стартовой чистке, не замена ей: срабатывает только при
    // корректном завершении (Ctrl+C через graceful_shutdown). При kill или
    // краше этот код не выполнится — и это нормально, следствие уберёт
    // чистка при следующем старте.
    if should_shutdown_scheduler(run_mode) {
        if let Err(err) = scheduler_for_cleanup.shutdown().await {
            tracing::warn!("не удалось выгрузить модели при завершении: {err:#}");
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_flag_selects_repl_without_changing_the_default_server_mode() {
        assert_eq!(parse_run_mode(&[]).unwrap(), RunMode::Serve);
        assert_eq!(parse_run_mode(&["--terminal"]).unwrap(), RunMode::Terminal);
        assert_eq!(parse_run_mode(&["terminal"]).unwrap(), RunMode::Terminal);
        assert!(parse_run_mode(&["--unknown"]).is_err());
    }

    #[test]
    fn terminal_defaults_to_quiet_logs_while_server_keeps_diagnostics() {
        assert_eq!(default_log_filter(RunMode::Terminal), "error");
        assert_eq!(default_log_filter(RunMode::Serve), "info");
    }

    #[test]
    fn only_server_mode_shuts_down_provider_models() {
        assert!(should_shutdown_scheduler(RunMode::Serve));
        assert!(!should_shutdown_scheduler(RunMode::Terminal));
    }

    #[test]
    fn only_server_mode_preloads_always_loaded_models() {
        assert!(should_preload_always_loaded(RunMode::Serve));
        assert!(!should_preload_always_loaded(RunMode::Terminal));
    }
}
