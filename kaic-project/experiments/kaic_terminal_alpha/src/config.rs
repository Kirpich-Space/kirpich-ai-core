//! Простой конфиг kaic.toml.
//!
//! Сознательно НЕ используем serde+toml — формат плоский, ключей мало,
//! полноценный TOML-парсер тут избыточен ("50 строк вместо 500").
//! Понимаем `key = value` построчно, `#` — комментарий, значения-строки
//! можно (не обязательно) обернуть в кавычки.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Config {
    pub model_path: String,
    pub n_gpu_layers: u32,
    pub n_ctx: u32,
    pub temperature: f32,
    pub max_tokens: usize,
    pub log_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model_path: "model.gguf".to_string(),
            n_gpu_layers: 0,
            n_ctx: 4096,
            temperature: 0.8,
            max_tokens: 256,
            log_path: "logs/kaic.log".to_string(),
        }
    }
}

const DEFAULT_TOML: &str = "\
# Конфигурация KAIC Terminal Alpha
model_path = \"model.gguf\"
n_gpu_layers = 0
n_ctx = 4096
temperature = 0.8
max_tokens = 256
log_path = \"logs/kaic.log\"
";

impl Config {
    /// Если файла нет — создаёт его с значениями по умолчанию (удобно для
    /// первого запуска) и возвращает Config::default().
    pub fn load(path: &str) -> Result<Config> {
        if !Path::new(path).exists() {
            let mut f = fs::File::create(path)?;
            f.write_all(DEFAULT_TOML.as_bytes())?;
            return Ok(Config::default());
        }

        let text = fs::read_to_string(path)?;
        let mut cfg = Config::default();

        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"');

            match key {
                "model_path" => cfg.model_path = value.to_string(),
                "n_gpu_layers" => cfg.n_gpu_layers = value.parse().unwrap_or(cfg.n_gpu_layers),
                "n_ctx" => cfg.n_ctx = value.parse().unwrap_or(cfg.n_ctx),
                "temperature" => cfg.temperature = value.parse().unwrap_or(cfg.temperature),
                "max_tokens" => cfg.max_tokens = value.parse().unwrap_or(cfg.max_tokens),
                "log_path" => cfg.log_path = value.to_string(),
                _ => {
                    // неизвестный ключ — молча игнорируем, это альфа
                }
            }
        }

        Ok(cfg)
    }
}
