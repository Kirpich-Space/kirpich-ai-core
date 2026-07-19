//! Resource Registry — таблица ресурсных характеристик моделей.
//!
//! В отличие от Capability Registry (который знает, "что модель умеет"),
//! этот реестр знает, "сколько модель стоит": сколько VRAM занимает,
//! сколько RAM требуется при CPU-offload оставшихся слоёв, сколько
//! секунд занимает холодная загрузка, и должна ли модель быть
//! всегда резидентной в памяти.
//!
//! Использует его исключительно Scheduler — для решения, можно ли
//! сейчас загрузить модель, что выгрузить при нехватке VRAM (LRU),
//! и какие модели выгружать нельзя никогда (`always_loaded`).
//!
//! Router и Capability Registry об этой таблице не знают.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Где физически размещается модель при загрузке.
///
/// Это диагностическое поле на будущее: сейчас все модели грузятся на GPU
/// (с частичным CPU-offload, см. `ram_offload_mb`), но если появится модель,
/// которую выгоднее держать целиком в RAM — Scheduler'у не придётся меняться,
/// он просто прочитает другое значение этого поля.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferredDevice {
    Gpu,
    Cpu,
    Hybrid,
}

impl Default for PreferredDevice {
    fn default() -> Self {
        PreferredDevice::Gpu
    }
}

/// Ресурсные характеристики одной модели.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceEntry {
    /// Реальное потребление VRAM (в мегабайтах) при выбранной конфигурации
    /// загрузки — то есть именно то число, на которое Scheduler ориентируется
    /// при решении "поместится ли модель в свободную память сейчас".
    /// Не имеет отношения к тому, сколько слоёв ушло на CPU — это учтено
    /// заранее в самом числе.
    pub vram_mb: u32,

    /// Сколько системной RAM (в мегабайтах) дополнительно занято за счёт
    /// CPU-offload вынесенных слоёв. Scheduler в принятии решений это число
    /// не использует — это диагностическая информация для GUI, логов
    /// и мониторинга памяти.
    #[serde(default)]
    pub ram_offload_mb: u32,

    /// Ожидаемое время холодной загрузки модели в секундах.
    /// Используется Scheduler'ом, чтобы понимать цену переключения моделей.
    pub load_seconds: u32,

    /// Если true — модель не подлежит автоматической выгрузке (LRU).
    /// Держится в памяти постоянно (например Nemotron и Fable9B).
    #[serde(default)]
    pub always_loaded: bool,

    /// Где физически размещается модель. По умолчанию — GPU.
    #[serde(default)]
    pub preferred_device: PreferredDevice,

    /// Путь к файлу модели (например GGUF), относительно рабочей директории.
    /// Нужен только для backend'ов, которые сами загружают файл модели
    /// (см. будущий EmbeddedBackend) — LMStudioBackend обращается к модели
    /// по имени через LM Studio и это поле не использует, поэтому оно
    /// опционально и для существующих записей реестра не требуется.
    #[serde(default)]
    pub path: Option<String>,
}

/// Реестр ресурсных характеристик всех известных моделей.
///
/// Загружается один раз из YAML-файла при старте системы.
#[derive(Debug, Clone)]
pub struct ResourceRegistry {
    entries: HashMap<String, ResourceEntry>,
}

impl ResourceRegistry {
    /// Загружает реестр из YAML-файла.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        let entries: HashMap<String, ResourceEntry> = serde_yaml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать YAML в {}", path.display()))?;
        Ok(Self { entries })
    }

    /// Перечисляет все известные модели вместе с их характеристиками.
    /// Используется Control Center API для отображения списка моделей.
    pub fn all(&self) -> impl Iterator<Item = (&String, &ResourceEntry)> {
        self.entries.iter()
    }

    /// Возвращает ресурсные характеристики модели по имени, если она известна реестру.
    pub fn get(&self, model_name: &str) -> Option<&ResourceEntry> {
        self.entries.get(model_name)
    }

    /// Суммарный объём VRAM (в мегабайтах), который обязаны занимать
    /// always_loaded-модели одновременно. Scheduler использует это как
    /// нижнюю границу постоянно занятой памяти при расчёте свободного места.
    pub fn always_loaded_vram_mb(&self) -> u32 {
        self.entries
            .values()
            .filter(|e| e.always_loaded)
            .map(|e| e.vram_mb)
            .sum()
    }

    /// Имена моделей, помеченных как always_loaded —
    /// Scheduler не имеет права их выгружать по LRU.
    pub fn always_loaded_models(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, e)| e.always_loaded)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_registry() -> ResourceRegistry {
        let yaml = r#"
Nemotron:
  vram_mb: 3000
  load_seconds: 2
  always_loaded: true

Fable9B:
  vram_mb: 6000
  load_seconds: 5
  always_loaded: true

Qwen40B:
  vram_mb: 6000
  ram_offload_mb: 18000
  load_seconds: 90
"#;
        let entries: HashMap<String, ResourceEntry> = serde_yaml::from_str(yaml).unwrap();
        ResourceRegistry { entries }
    }

    #[test]
    fn get_returns_known_model() {
        let registry = sample_registry();
        let entry = registry.get("Fable9B").unwrap();
        assert_eq!(entry.vram_mb, 6000);
        assert!(entry.always_loaded);
    }

    #[test]
    fn get_returns_none_for_unknown_model() {
        let registry = sample_registry();
        assert!(registry.get("Unknown").is_none());
    }

    #[test]
    fn always_loaded_vram_sums_only_flagged_models() {
        let registry = sample_registry();
        // Nemotron (3000) + Fable9B (6000), Qwen40B не always_loaded
        assert_eq!(registry.always_loaded_vram_mb(), 9000);
    }

    #[test]
    fn ram_offload_defaults_to_zero() {
        let registry = sample_registry();
        assert_eq!(registry.get("Nemotron").unwrap().ram_offload_mb, 0);
    }

    #[test]
    fn preferred_device_defaults_to_gpu() {
        let registry = sample_registry();
        assert_eq!(
            registry.get("Nemotron").unwrap().preferred_device,
            PreferredDevice::Gpu
        );
    }

    #[test]
    fn path_defaults_to_none() {
        let registry = sample_registry();
        assert_eq!(registry.get("Nemotron").unwrap().path, None);
    }
}
