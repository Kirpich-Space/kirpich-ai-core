use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub model_path: String,

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
                "не найден {} — скопируй kaic.toml.example в корень проекта и укажи model_path",
                config_path.display()
            )
        })?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("не удалось разобрать {}", config_path.display()))?;
        Ok(config)
    }
}
