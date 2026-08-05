//! Smoke test: real `KaicEngine::chat` through Planner/`run_pass`.
//!
//! Модель Planner'а берётся из `KAIC_MODEL_PATH`, а если переменной нет — из
//! `[models].planner` в `kaic.toml` крейта. Переменная остаётся приоритетной:
//! это точка ручного override и подстановки другой модели в CI.
//!
//! Если не удалось ни то, ни другое, тест выходит рано (без паники), чтобы
//! обычный прогон набора оставался зелёным на машине без локальной модели.

use std::path::Path;

use crate::agent::run_pass;
use crate::contract::GateResult;
use crate::core::config::Config;
use crate::core::engine::{ChatParams, KaicEngine};

/// Путь к модели Planner'а: env-override, иначе конфиг.
///
/// Чистая функция от источников, чтобы приоритет можно было проверить без
/// файловой системы и без загрузки модели. `from_config` вызывается только
/// когда override не задан — конфиг не читается зря.
fn pick_model_path(
    env_override: Option<String>,
    from_config: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match env_override {
        Some(path) if !path.trim().is_empty() => Some(path),
        _ => from_config(),
    }
}

/// `[models].planner` из kaic.toml крейта.
///
/// Переиспользует существующий парсинг `Config` — своего разбора TOML здесь
/// нет, чтобы схема не разъехалась с той, что читает REPL.
fn planner_model_from_config() -> Option<String> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    match Config::load(crate_root) {
        Ok(config) => Some(config.models.planner),
        Err(err) => {
            eprintln!("smoke_run_pass_fills_pass_report: конфиг не прочитан ({err:#})");
            None
        }
    }
}

#[test]
fn smoke_run_pass_fills_pass_report() {
    let env_override = std::env::var("KAIC_MODEL_PATH").ok();
    let Some(model_path) = pick_model_path(env_override, planner_model_from_config) else {
        eprintln!(
            "smoke_run_pass_fills_pass_report: skip (нет ни KAIC_MODEL_PATH, ни [models].planner)"
        );
        return;
    };
    if model_path.trim().is_empty() || !Path::new(&model_path).is_file() {
        eprintln!(
            "smoke_run_pass_fills_pass_report: skip (модель не найдена по пути: {model_path})"
        );
        return;
    }

    let engine = KaicEngine::new(&model_path, 0).expect("engine init");
    let params = ChatParams {
        temperature: 0.2,
        max_tokens: 256,
        context: 2048,
        // Грамматику подставляет сам Planner: она — часть его контракта с
        // моделью, а не параметр вызывающего.
        grammar: None,
    };

    let report = run_pass(
        &engine,
        "List one safe inspect-only step for documenting a Rust CLI.",
        &params,
        &model_path,
    )
    .expect("run_pass");

    eprintln!("[smoke] PassReport = {report:#?}");

    assert_eq!(report.agent_identity.id, "planner_v1");
    assert_eq!(report.agent_identity.version, 1);
    assert_eq!(report.agent_identity.model, model_path);
    assert!(
        matches!(
            report.gate_result,
            GateResult::Ready
                | GateResult::NeedClarification {
                    clarification_questions: _
                }
        ),
        "gate must be Ready or NeedClarification"
    );
    match &report.gate_result {
        GateResult::Ready => {
            // Coder not implemented — report must not invent execution.
            let _ = &report.plan;
        }
        GateResult::NeedClarification {
            clarification_questions,
        } => {
            assert!(
                !clarification_questions.is_empty() || report.plan.is_some(),
                "NeedClarification should carry questions and/or a plan"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pick_model_path;

    #[test]
    fn env_override_wins_and_config_is_not_read() {
        let picked = pick_model_path(Some("/from/env.gguf".to_string()), || {
            panic!("конфиг не должен читаться, когда задан KAIC_MODEL_PATH")
        });

        assert_eq!(picked.as_deref(), Some("/from/env.gguf"));
    }

    #[test]
    fn falls_back_to_config_when_env_is_absent() {
        let picked = pick_model_path(None, || Some("/from/config.gguf".to_string()));

        assert_eq!(picked.as_deref(), Some("/from/config.gguf"));
    }

    #[test]
    fn blank_env_is_treated_as_absent() {
        // Пустая переменная — частый артефакт CI; она не должна побеждать
        // конфиг и не должна выглядеть как заданный путь.
        let picked = pick_model_path(Some("   ".to_string()), || {
            Some("/from/config.gguf".to_string())
        });

        assert_eq!(picked.as_deref(), Some("/from/config.gguf"));
    }

    #[test]
    fn missing_everywhere_yields_none() {
        let picked = pick_model_path(None, || None);

        assert!(picked.is_none());
    }
}
