//! Жёсткий гейт на входе в пайплайн.
//!
//! Ключевое свойство: ассет без записи в манифесте на таймлайн попасть **не
//! может** — и это гарантия типов, а не дисциплина вызывающего.
//! [`AdmittedAsset`] имеет приватные поля и приватный конструктор, поэтому
//! создать его вне этого модуля невозможно; [`Timeline`] принимает только его.
//! Забыть проверку нельзя: нечего будет положить.
//!
//! Проверка происходит на входе, а не ревизией собранного проекта — к моменту
//! готового рендера отзывать ассет уже поздно.

use super::provenance::{Manifest, MediaKind, Provenance};

/// Лицензии, разрешённые к монтажу в v1.
///
/// - `cc0`, `pdm` — public domain, обязательств нет;
/// - `by` — CC-BY, разрешён решением архитектора, требует титров.
///
/// Список расширяется только решением архитектора. Почему остальные не здесь —
/// см. [`rejection_reason`].
const ALLOWED_LICENSES: &[&str] = &["cc0", "pdm", "by"];

/// Требует ли лицензия указания автора.
///
/// Public domain не требует; всё остальное из разрешённых — требует.
pub fn requires_attribution(license: &str) -> bool {
    !matches!(license, "cc0" | "pdm")
}

/// Почему лицензия не допущена. Причина сохраняется в отказе, чтобы решение
/// можно было обсудить, а не гадать, отчего ассет «просто не прошёл».
fn rejection_reason(license: &str) -> &'static str {
    match license {
        // Монтаж — это производное произведение по определению, а ND его
        // прямо запрещает. Тут нечего решать: такой ассет непригоден.
        "by-nd" | "by-nc-nd" => "лицензия ND запрещает производные произведения, а монтаж — производное",
        // NC ограничивает коммерческое использование. Пайплайн не знает,
        // коммерческий ли результат, и решать это за пользователя не вправе.
        "by-nc" | "by-nc-sa" => "лицензия NC ограничивает коммерческое использование; для v1 не разрешена",
        // ShareAlike навязывает свою лицензию всему результату монтажа —
        // это решение о лицензировании проекта, не техническая деталь.
        "by-sa" => "ShareAlike распространяет свою лицензию на весь результат; для v1 не разрешена",
        _ => "лицензия не входит в список разрешённых для v1",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRejection {
    /// Ассета нет в манифесте — основной случай, ради которого гейт есть.
    NoManifestEntry { asset_id: String },
    /// Музыка исключена из v1 полностью.
    AudioNotAllowed { asset_id: String },
    LicenseNotAllowed {
        asset_id: String,
        license: String,
        reason: &'static str,
    },
    /// Лицензия требует атрибуции, но источник не дал строку автора.
    /// Собрать титры будет нечем, значит использовать ассет нельзя.
    AttributionMissing { asset_id: String },
    /// Материал пользователя без явного подтверждения прав.
    RightsNotConfirmed { asset_id: String },
}

impl std::fmt::Display for GateRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoManifestEntry { asset_id } => {
                write!(f, "ассет {asset_id} отсутствует в провенанс-манифесте")
            }
            Self::AudioNotAllowed { asset_id } => {
                write!(f, "ассет {asset_id} — аудио; музыка исключена из v1")
            }
            Self::LicenseNotAllowed {
                asset_id,
                license,
                reason,
            } => write!(f, "ассет {asset_id}: лицензия '{license}' не допущена — {reason}"),
            Self::AttributionMissing { asset_id } => {
                write!(f, "ассет {asset_id}: лицензия требует атрибуции, но автор не указан")
            }
            Self::RightsNotConfirmed { asset_id } => {
                write!(f, "ассет {asset_id}: права пользователя не подтверждены")
            }
        }
    }
}

/// Ассет, прошедший гейт.
///
/// Поля приватны, конструктор приватный — вне этого модуля значение создать
/// нельзя. Именно это делает «без манифеста на таймлайн не попадёт»
/// свойством пайплайна, а не пожеланием.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedAsset {
    asset_id: String,
    kind: MediaKind,
    /// Строка титров, если лицензия их требует.
    credit: Option<String>,
    license: String,
}

impl AdmittedAsset {
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }
    /// Часть публичного контракта допущенного ассета: понадобится, когда
    /// рендер начнёт различать изображения и видео.
    #[allow(dead_code)]
    pub fn kind(&self) -> MediaKind {
        self.kind
    }
    /// `Some` ⇒ ассет обязан быть упомянут в титрах.
    pub fn credit(&self) -> Option<&str> {
        self.credit.as_deref()
    }
    /// Лицензия, под которой ассет допущен. Нужна отчётности о проекте;
    /// вызывающей стороны за пределами тестов пока нет.
    #[allow(dead_code)]
    pub fn license(&self) -> &str {
        &self.license
    }
}

/// Единственный вход в пайплайн.
pub fn admit(manifest: &Manifest, asset_id: &str) -> Result<AdmittedAsset, GateRejection> {
    let Some(entry) = manifest.get(asset_id) else {
        return Err(GateRejection::NoManifestEntry {
            asset_id: asset_id.to_string(),
        });
    };

    if entry.kind == MediaKind::Audio {
        return Err(GateRejection::AudioNotAllowed {
            asset_id: asset_id.to_string(),
        });
    }

    match &entry.provenance {
        Provenance::Licensed(licensed) => {
            if !ALLOWED_LICENSES.contains(&licensed.license.as_str()) {
                return Err(GateRejection::LicenseNotAllowed {
                    asset_id: asset_id.to_string(),
                    license: licensed.license.clone(),
                    reason: rejection_reason(&licensed.license),
                });
            }

            let credit = if requires_attribution(&licensed.license) {
                if licensed.attribution.trim().is_empty() {
                    return Err(GateRejection::AttributionMissing {
                        asset_id: asset_id.to_string(),
                    });
                }
                Some(licensed.attribution.clone())
            } else {
                None
            };

            Ok(AdmittedAsset {
                asset_id: asset_id.to_string(),
                kind: entry.kind,
                credit,
                license: licensed.license.clone(),
            })
        }
        Provenance::UserOwned(user) => {
            if !user.is_confirmed() {
                return Err(GateRejection::RightsNotConfirmed {
                    asset_id: asset_id.to_string(),
                });
            }
            Ok(AdmittedAsset {
                asset_id: asset_id.to_string(),
                kind: entry.kind,
                credit: None,
                license: "user-owned".to_string(),
            })
        }
    }
}

/// Таймлайн монтажа. Наполняется только прошедшими гейт ассетами.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    assets: Vec<AdmittedAsset>,
}

impl Timeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, asset: AdmittedAsset) {
        self.assets.push(asset);
    }

    pub fn assets(&self) -> &[AdmittedAsset] {
        &self.assets
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provenance::{LicensedProvenance, ManifestEntry, UserProvenance};
    use chrono::Utc;

    fn licensed(license: &str, attribution: &str) -> Provenance {
        Provenance::Licensed(LicensedProvenance {
            source: "openverse".to_string(),
            asset_url: "https://example.org/a.jpg".to_string(),
            landing_url: "https://example.org/page".to_string(),
            license: license.to_string(),
            license_version: "2.0".to_string(),
            license_url: "https://creativecommons.org/licenses/by/2.0/".to_string(),
            license_snapshot: "текст лицензии на момент получения".to_string(),
            creator: Some("Автор".to_string()),
            creator_url: None,
            attribution: attribution.to_string(),
            retrieved_at: Utc::now(),
        })
    }

    fn manifest_with(asset_id: &str, kind: MediaKind, provenance: Provenance) -> Manifest {
        let mut manifest = Manifest::new();
        manifest.insert(ManifestEntry {
            asset_id: asset_id.to_string(),
            kind,
            provenance,
        });
        manifest
    }

    #[test]
    fn asset_without_manifest_entry_is_rejected() {
        let manifest = Manifest::new();

        let result = admit(&manifest, "неизвестный");

        assert_eq!(
            result,
            Err(GateRejection::NoManifestEntry {
                asset_id: "неизвестный".to_string()
            })
        );
    }

    #[test]
    fn audio_is_rejected_even_with_perfect_license() {
        // Музыка исключена из v1 полностью: безупречная лицензия не помогает.
        let manifest = manifest_with("m1", MediaKind::Audio, licensed("cc0", ""));

        let result = admit(&manifest, "m1");

        assert_eq!(
            result,
            Err(GateRejection::AudioNotAllowed {
                asset_id: "m1".to_string()
            })
        );
    }

    #[test]
    fn no_derivatives_licenses_are_rejected() {
        for license in ["by-nd", "by-nc-nd"] {
            let manifest = manifest_with("a", MediaKind::Image, licensed(license, "credit"));

            let result = admit(&manifest, "a");

            match result {
                Err(GateRejection::LicenseNotAllowed { reason, .. }) => {
                    assert!(reason.contains("ND"), "причина должна называть ND: {reason}");
                }
                other => panic!("ND-лицензия {license} обязана быть отвергнута, получено {other:?}"),
            }
        }
    }

    #[test]
    fn nc_and_sa_are_not_authorized_for_v1() {
        for license in ["by-nc", "by-nc-sa", "by-sa"] {
            let manifest = manifest_with("a", MediaKind::Image, licensed(license, "credit"));
            assert!(
                matches!(
                    admit(&manifest, "a"),
                    Err(GateRejection::LicenseNotAllowed { .. })
                ),
                "лицензия {license} не разрешалась архитектором для v1"
            );
        }
    }

    #[test]
    fn cc_by_requires_attribution_string() {
        let manifest = manifest_with("a", MediaKind::Image, licensed("by", "   "));

        assert_eq!(
            admit(&manifest, "a"),
            Err(GateRejection::AttributionMissing {
                asset_id: "a".to_string()
            })
        );
    }

    #[test]
    fn cc_by_passes_and_carries_credit() {
        let manifest = manifest_with(
            "a",
            MediaKind::Image,
            licensed("by", "\"Mountains\" by Kiwi Tom is licensed under CC BY 2.0."),
        );

        let admitted = admit(&manifest, "a").expect("CC-BY допущен");

        assert_eq!(admitted.license(), "by");
        assert!(admitted.credit().is_some(), "CC-BY обязан нести титры");
    }

    #[test]
    fn public_domain_needs_no_credit() {
        for license in ["cc0", "pdm"] {
            let manifest = manifest_with("a", MediaKind::Image, licensed(license, ""));

            let admitted = admit(&manifest, "a").expect("public domain допущен");

            assert!(admitted.credit().is_none(), "{license} не требует атрибуции");
        }
    }

    #[test]
    fn user_material_requires_explicit_rights_confirmation() {
        let unconfirmed = Provenance::UserOwned(UserProvenance {
            original_path: "a.jpg".to_string(),
            rights_confirmation: String::new(),
            confirmed_at: Utc::now(),
        });
        let manifest = manifest_with("u1", MediaKind::Image, unconfirmed);

        assert_eq!(
            admit(&manifest, "u1"),
            Err(GateRejection::RightsNotConfirmed {
                asset_id: "u1".to_string()
            })
        );

        let confirmed = Provenance::UserOwned(UserProvenance {
            original_path: "a.jpg".to_string(),
            rights_confirmation: "снято мной".to_string(),
            confirmed_at: Utc::now(),
        });
        let manifest = manifest_with("u2", MediaKind::Image, confirmed);
        assert!(admit(&manifest, "u2").is_ok());
    }

    #[test]
    fn timeline_accepts_only_admitted_assets() {
        // Тест компиляционного свойства: положить в Timeline что-то, кроме
        // AdmittedAsset, невозможно, а AdmittedAsset вне gate.rs не создать —
        // у него приватные поля. Здесь проверяется, что легальный путь работает.
        let manifest = manifest_with("a", MediaKind::Image, licensed("cc0", ""));
        let admitted = admit(&manifest, "a").expect("допущен");

        let mut timeline = Timeline::new();
        assert!(timeline.is_empty());
        timeline.push(admitted);

        assert_eq!(timeline.assets().len(), 1);
        assert_eq!(timeline.assets()[0].asset_id(), "a");
    }
}
