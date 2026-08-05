//! Провенанс-манифест: происхождение и права на каждый ассет.
//!
//! Манифест — не «отметка, что лицензия ок». На каждый ассет он несёт автора,
//! источник, идентификатор и версию лицензии, ссылку на её текст и снимок
//! этого текста на момент получения. Без этих полей нельзя ни собрать титры
//! для CC-BY, ни объяснить постфактум, откуда взялся кадр.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Тип медиа, допустимый в пайплайне.
///
/// Аудио здесь отсутствует НАМЕРЕННО: музыка исключена из v1 целиком,
/// включая экспериментальные опции. Отдельный вариант `Audio` существует
/// только чтобы гейт мог его явно отвергнуть, если ассет придёт извне —
/// молчаливое отсутствие варианта означало бы, что аудио просто
/// не распознаётся, а не запрещено.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    /// Не допускается в v1. Существует для явного отказа, не для использования.
    Audio,
}

/// Происхождение ассета из лицензионного источника.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicensedProvenance {
    /// Идентификатор источника, например "openverse".
    pub source: String,
    /// Прямой URL файла.
    pub asset_url: String,
    /// Страница ассета у первичного правообладателя (foreign_landing_url).
    pub landing_url: String,
    /// Идентификатор лицензии как его отдаёт API: "by", "cc0", "by-nd", ...
    pub license: String,
    pub license_version: String,
    pub license_url: String,
    /// Снимок текста лицензии/условий на момент получения. Нужен потому, что
    /// страница лицензии может измениться, а обязательства фиксируются на
    /// момент использования.
    pub license_snapshot: String,
    pub creator: Option<String>,
    pub creator_url: Option<String>,
    /// Готовая строка атрибуции от источника. Для CC-BY именно она попадёт
    /// в титры — самодельная формулировка тут хуже, чем каноническая.
    pub attribution: String,
    pub retrieved_at: DateTime<Utc>,
}

/// Происхождение ассета, предоставленного пользователем.
///
/// Агент не может проверить права на чужой файл и не делает вид, что может:
/// фиксируется ровно то, что пользователь утвердил, и когда.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProvenance {
    pub original_path: String,
    /// Формулировка подтверждения прав, данная пользователем.
    /// Пустая строка означает «не подтверждено» — гейт такой ассет отвергнет.
    pub rights_confirmation: String,
    pub confirmed_at: DateTime<Utc>,
}

impl UserProvenance {
    pub fn is_confirmed(&self) -> bool {
        !self.rights_confirmation.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Provenance {
    /// Получено из лицензионного источника.
    Licensed(LicensedProvenance),
    /// Материал пользователя, права подтверждены им лично.
    UserOwned(UserProvenance),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub asset_id: String,
    pub kind: MediaKind,
    pub provenance: Provenance,
}

/// Манифест проекта: единственный источник правды о происхождении ассетов.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    entries: HashMap<String, ManifestEntry>,
}

impl Manifest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, entry: ManifestEntry) {
        self.entries.insert(entry.asset_id.clone(), entry);
    }

    pub fn get(&self, asset_id: &str) -> Option<&ManifestEntry> {
        self.entries.get(asset_id)
    }

    /// Размер манифеста. Пока используется только тестами — понадобится
    /// отчётности о проекте.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfirmed_user_rights_are_detected() {
        let base = UserProvenance {
            original_path: "C:/photos/a.jpg".to_string(),
            rights_confirmation: String::new(),
            confirmed_at: Utc::now(),
        };
        assert!(!base.is_confirmed(), "пустое подтверждение — не подтверждение");

        let blank = UserProvenance {
            rights_confirmation: "   ".to_string(),
            ..base.clone()
        };
        assert!(!blank.is_confirmed(), "пробелы — не подтверждение");

        let ok = UserProvenance {
            rights_confirmation: "снято мной, права мои".to_string(),
            ..base
        };
        assert!(ok.is_confirmed());
    }

    #[test]
    fn manifest_lookup_is_by_asset_id() {
        let mut manifest = Manifest::new();
        assert!(manifest.is_empty());

        manifest.insert(ManifestEntry {
            asset_id: "a1".to_string(),
            kind: MediaKind::Image,
            provenance: Provenance::UserOwned(UserProvenance {
                original_path: "a.jpg".to_string(),
                rights_confirmation: "мои".to_string(),
                confirmed_at: Utc::now(),
            }),
        });

        assert_eq!(manifest.len(), 1);
        assert!(manifest.get("a1").is_some());
        assert!(manifest.get("не-существует").is_none());
    }
}
