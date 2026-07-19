//! Capability Registry — таблица соответствия категорий задач и моделей.
//!
//! Это единственное место в системе, которое знает о существовании
//! конкретных моделей (Fable9B, GLM47, Qwen40B и т.д.).
//!
//! Router передаёт сюда категорию задачи ("Programming", "Engineering" и т.п.)
//! и получает список моделей-кандидатов: primary + fallback по порядку.
//!
//! Никакой логики выбора здесь нет — только чтение заранее известных фактов
//! из YAML-файла, который редактируется вручную.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Запись реестра для одной категории задач.
#[derive(Debug, Clone, Deserialize)]
pub struct CapabilityEntry {
    /// Основная модель для этой категории.
    pub primary: String,

    /// Модели, которые пробуются по очереди, если primary недоступна
    /// (не загружена / сервис недоступен). Пустой список — fallback нет.
    #[serde(default)]
    pub fallback: Vec<String>,

    /// Если true — модель никогда не выбирается автоматически.
    /// Используется только по явному запросу пользователя (например Qwen40B).
    #[serde(default)]
    pub manual_only: bool,
}

/// Реестр соответствия "категория задачи → модели".
///
/// Загружается один раз из YAML-файла. Чтобы поменять модель для категории
/// или добавить новую категорию, достаточно отредактировать YAML —
/// код Router и Scheduler менять не нужно.
#[derive(Debug, Clone)]
pub struct CapabilityRegistry {
    entries: HashMap<String, CapabilityEntry>,
}

impl CapabilityRegistry {
    /// Загружает реестр из YAML-файла.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        let entries: HashMap<String, CapabilityEntry> = serde_yaml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать YAML в {}", path.display()))?;
        Ok(Self { entries })
    }

    /// Возвращает запись реестра для указанной категории, если она есть.
    pub fn get(&self, category: &str) -> Option<&CapabilityEntry> {
        self.entries.get(category)
    }

    /// Возвращает упорядоченный список моделей-кандидатов для категории:
    /// сначала primary, затем fallback по порядку.
    ///
    /// Если категория не найдена в реестре — возвращает пустой список,
    /// вызывающий код (Scheduler) должен обработать этот случай сам
    /// (например, откатиться на категорию `Simple`).
    pub fn candidates(&self, category: &str) -> Vec<&str> {
        match self.entries.get(category) {
            Some(entry) => {
                let mut result = vec![entry.primary.as_str()];
                result.extend(entry.fallback.iter().map(String::as_str));
                result
            }
            None => Vec::new(),
        }
    }

    /// Проверяет, помечена ли категория как "только вручную"
    /// (модель не должна запускаться автоматически).
    pub fn is_manual_only(&self, category: &str) -> bool {
        self.entries
            .get(category)
            .map(|e| e.manual_only)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_returns_primary_then_fallback() {
        let yaml = r#"
Programming:
  primary: Fable9B
  fallback:
    - GLM47
    - GPTOSS20
"#;
        let entries: HashMap<String, CapabilityEntry> = serde_yaml::from_str(yaml).unwrap();
        let registry = CapabilityRegistry { entries };

        assert_eq!(
            registry.candidates("Programming"),
            vec!["Fable9B", "GLM47", "GPTOSS20"]
        );
    }

    #[test]
    fn unknown_category_returns_empty() {
        let registry = CapabilityRegistry {
            entries: HashMap::new(),
        };
        assert!(registry.candidates("Unknown").is_empty());
    }

    #[test]
    fn manual_only_defaults_to_false() {
        let yaml = r#"
Programming:
  primary: Fable9B
"#;
        let entries: HashMap<String, CapabilityEntry> = serde_yaml::from_str(yaml).unwrap();
        let registry = CapabilityRegistry { entries };
        assert!(!registry.is_manual_only("Programming"));
    }
}
