//! Медиа-пайплайн v1 под `Category::Video`.
//!
//! Устройство — три звена, и порядок между ними принципиален:
//!
//! ```text
//! источник → провенанс-манифест → ГЕЙТ → таймлайн → титры → план рендера
//! ```
//!
//! Гейт стоит **на входе**, а не ревизией собранного проекта: к моменту
//! готового рендера отзывать ассет уже поздно. Свойство «ассет без записи в
//! манифесте на таймлайн не попадёт» обеспечено типами, а не дисциплиной —
//! [`gate::AdmittedAsset`] нельзя создать вне `gate.rs`, а [`gate::Timeline`]
//! принимает только его. То же и с титрами: [`credits::RenderPlan`] выдаётся
//! только функцией, которая их проверила.
//!
//! Что решено и зафиксировано (см. PROJECT_STATE.md, раздел решений трека):
//! - ffmpeg без экспорта в NLE;
//! - CC-BY допустим, титры для него обязательны;
//! - музыка исключена из v1 полностью;
//! - в v1 один источник — Openverse.
//!
//! Модуль подключён к `run_task_pipeline` через `Category::Video`: точка
//! ветвления и сборка плана живут в `control_center_api.rs`, чтобы пайплайн
//! не знал ни про HTTP, ни про Task Store.

pub mod credits;
pub mod fetch;
pub mod gate;
pub mod openverse;
pub mod provenance;
pub mod query;
pub mod render;

#[cfg(test)]
mod tests {
    use super::credits::plan_render;
    use super::gate::{admit, GateRejection, Timeline};
    use super::openverse;
    use super::provenance::{Manifest, ManifestEntry, MediaKind, Provenance, UserProvenance};
    use chrono::Utc;

    /// Сквозной путь: реальный ответ источника → манифест → гейт → таймлайн →
    /// титры → план. Проверяется, что звенья стыкуются, а не только работают
    /// по отдельности.
    #[test]
    fn end_to_end_from_real_source_response_to_render_plan() {
        let body = include_str!("../../tests/openverse_by_sample.json");
        let parsed = openverse::parse_search_response(body).expect("ответ разбирается");

        let mut manifest = Manifest::new();
        let mut ids = Vec::new();
        for result in &parsed.results {
            let entry = openverse::to_manifest_entry(result);
            ids.push(entry.asset_id.clone());
            manifest.insert(entry);
        }

        let mut timeline = Timeline::new();
        for id in &ids {
            timeline.push(admit(&manifest, id).expect("ассет допущен"));
        }

        let plan = plan_render(&timeline).expect("план собирается");

        assert_eq!(plan.asset_ids().len(), ids.len());
        assert!(
            !plan.credits_text().is_empty(),
            "проект из CC-BY-материала обязан иметь непустые титры"
        );
    }

    /// Смешанный проект: материал пользователя рядом с лицензионным.
    #[test]
    fn user_material_and_licensed_material_coexist() {
        let body = include_str!("../../tests/openverse_by_sample.json");
        let parsed = openverse::parse_search_response(body).expect("разбирается");

        let mut manifest = Manifest::new();
        let licensed = openverse::to_manifest_entry(&parsed.results[0]);
        let licensed_id = licensed.asset_id.clone();
        manifest.insert(licensed);
        manifest.insert(ManifestEntry {
            asset_id: "user:my-photo".to_string(),
            kind: MediaKind::Image,
            provenance: Provenance::UserOwned(UserProvenance {
                original_path: "C:/photos/my.jpg".to_string(),
                rights_confirmation: "снято мной, права мои".to_string(),
                confirmed_at: Utc::now(),
            }),
        });

        let mut timeline = Timeline::new();
        timeline.push(admit(&manifest, "user:my-photo").expect("свой материал допущен"));
        timeline.push(admit(&manifest, &licensed_id).expect("лицензионный допущен"));

        let plan = plan_render(&timeline).expect("план собирается");

        assert_eq!(plan.asset_ids().len(), 2);
        // Свой материал в титрах не нуждается, лицензионный — обязан быть.
        assert_eq!(
            plan.credits_text().lines().count(),
            1,
            "в титрах ровно один CC-BY-материал"
        );
    }

    /// Ассет, которого нет в манифесте, не попадает на таймлайн ни при каких
    /// обстоятельствах — это и есть жёсткий гейт.
    #[test]
    fn unmanifested_asset_never_reaches_the_timeline() {
        let manifest = Manifest::new();

        let rejection = admit(&manifest, "скачано-мимо-пайплайна.jpg")
            .expect_err("ассет без манифеста обязан быть отвергнут");

        assert!(matches!(rejection, GateRejection::NoManifestEntry { .. }));

        // Положить его в Timeline невозможно: push() принимает только
        // AdmittedAsset, а его вне gate.rs не сконструировать.
        let timeline = Timeline::new();
        assert!(timeline.is_empty());
    }
}
