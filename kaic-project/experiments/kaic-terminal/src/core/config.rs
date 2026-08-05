use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Модели по ролям. Раздельные ключи, а не один общий путь: роли предъявляют
/// к модели разные требования, и это установлено экспериментально, а не
/// из соображений вкуса.
///
/// `planner` обязан быть instruction-tuned non-reasoning моделью: reasoning-
/// модель уходит в рассуждение и не выдаёт формат, который ждёт гейт, — ни по
/// текстовой инструкции, ни под GBNF-грамматикой. Диагностика этого стоила
/// ~124 минут реального инференса, повторять её не нужно.
///
/// `repl` — интерактивный диалог с человеком, где reasoning-поведение не
/// мешает, а задержка ответа важнее: там уместна модель поменьше.
#[derive(Debug, Deserialize, Clone)]
pub struct Models {
    pub planner: String,
    pub repl: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub models: Models,

    #[serde(default = "default_temperature")]
    pub temperature: f32,

    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,

    #[serde(default = "default_context")]
    pub context: u32,

    #[serde(default)]
    pub n_gpu_layers: u32,
}

fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> usize {
    1024
}

fn default_context() -> u32 {
    4096
}

impl Config {
    /// Ищет kaic.toml строго в корне проекта — никакого поиска по
    /// родительским папкам или домашней директории. Если файла нет,
    /// явно говорим, что делать, а не тихо подставляем дефолтную модель.
    pub fn load(project_root: &Path) -> Result<Self> {
        let config_path = project_root.join("kaic.toml");
        let text = std::fs::read_to_string(&config_path).with_context(|| {
            format!(
                "не найден {} — скопируй kaic.toml.example в корень проекта и укажи [models]",
                config_path.display()
            )
        })?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("не удалось разобрать {}", config_path.display()))?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// kaic.toml.example обязан оставаться разбираемым в текущий Config.
    /// Пример — единственная инструкция, которую видит человек, поднимающий
    /// проект; разъехавшись со схемой, он молча ломает первый же запуск.
    #[test]
    fn example_config_matches_current_schema() {
        let example = include_str!("../../kaic.toml.example");

        let config: Config = toml::from_str(example).expect("kaic.toml.example разбирается");

        assert!(!config.models.planner.is_empty());
        assert!(!config.models.repl.is_empty());
    }

    #[test]
    fn roles_get_separate_model_paths() {
        let text = r#"
[models]
planner = "/models/planner.gguf"
repl = "/models/repl.gguf"
"#;

        let config: Config = toml::from_str(text).expect("конфиг разбирается");

        assert_eq!(config.models.planner, "/models/planner.gguf");
        assert_eq!(config.models.repl, "/models/repl.gguf");
        // Остальные поля необязательны и берут значения по умолчанию.
        assert_eq!(config.temperature, default_temperature());
        assert_eq!(config.max_tokens, default_max_tokens());
        assert_eq!(config.context, default_context());
        assert_eq!(config.n_gpu_layers, 0);
    }

    #[test]
    fn one_shared_model_path_is_rejected() {
        // Старая схема (общий model_path) не должна проходить молча:
        // иначе Planner тихо унаследовал бы модель REPL.
        let text = r#"model_path = "/models/one.gguf""#;

        assert!(toml::from_str::<Config>(text).is_err());
    }
}
