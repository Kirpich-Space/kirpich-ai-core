//! Титры и план рендера.
//!
//! Для CC-BY титры — не украшение, а условие лицензии: формально корректно
//! полученный ассет без указания автора всё равно использован с нарушением.
//! Поэтому титры здесь — обязательный шаг сборки: план рендера невозможно
//! получить, если хоть один ассет, требующий атрибуции, не попал в титры.

use super::gate::Timeline;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// Таймлайн пуст — рендерить нечего.
    EmptyTimeline,
    /// Ассет требует атрибуции, но строки титров у него нет.
    /// В норме недостижимо (гейт не выпустил бы такой ассет) — проверка
    /// оставлена как страховка на случай будущих правок гейта.
    CreditMissing { asset_id: String },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyTimeline => write!(f, "таймлайн пуст"),
            Self::CreditMissing { asset_id } => {
                write!(f, "ассет {asset_id} требует атрибуции, но титров для него нет")
            }
        }
    }
}

/// Готовый к рендеру план.
///
/// Поля приватны: получить план можно только через [`plan_render`], то есть
/// пройдя проверку титров.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderPlan {
    asset_ids: Vec<String>,
    credits_text: String,
}

impl RenderPlan {
    pub fn asset_ids(&self) -> &[String] {
        &self.asset_ids
    }

    /// Текст титров. Пустым не бывает, если хоть один ассет требует атрибуции.
    pub fn credits_text(&self) -> &str {
        &self.credits_text
    }

    /// Аргументы ffmpeg. Собираются как данные — исполнение внешнего процесса
    /// в v1 не входит: рендерить пока нечего, а непроверенный вызов ffmpeg
    /// выглядел бы работающим, не будучи им.
    ///
    /// Вызывающей стороны за пределами тестов пока нет: она появится в задаче
    /// про исполнение рендера.
    #[allow(dead_code)]
    pub fn ffmpeg_args(&self, output: &str) -> Vec<String> {
        let mut args = Vec::new();
        for id in &self.asset_ids {
            args.push("-i".to_string());
            args.push(id.clone());
        }
        // Титры вжигаются отдельным шагом — не как подсказка плеера, а как
        // часть картинки: файл может уехать куда угодно без метаданных.
        args.push("-vf".to_string());
        args.push(format!("drawtext=text='{}'", escape_drawtext(&self.credits_text)));
        args.push(output.to_string());
        args
    }
}

/// Экранирует текст для фильтра drawtext: кавычки и двоеточия в нём
/// синтаксически значимы.
///
/// Используется только из `ffmpeg_args` — оживёт вместе с ним.
#[allow(dead_code)]
fn escape_drawtext(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace('\n', " · ")
}

/// Собирает титры из таймлайна.
///
/// Порядок — порядок ассетов на таймлайне, дубликаты строк убираются:
/// один и тот же автор не должен упоминаться дважды.
pub fn build_credits(timeline: &Timeline) -> Result<String, BuildError> {
    if timeline.is_empty() {
        return Err(BuildError::EmptyTimeline);
    }

    let mut lines: Vec<String> = Vec::new();
    for asset in timeline.assets() {
        if let Some(credit) = asset.credit() {
            if credit.trim().is_empty() {
                return Err(BuildError::CreditMissing {
                    asset_id: asset.asset_id().to_string(),
                });
            }
            let line = credit.trim().to_string();
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }

    Ok(lines.join("\n"))
}

/// Единственный путь к плану рендера: титры проверяются здесь и обойти
/// проверку нельзя — конструктор [`RenderPlan`] приватный.
pub fn plan_render(timeline: &Timeline) -> Result<RenderPlan, BuildError> {
    let credits_text = build_credits(timeline)?;

    // Каждый ассет, требующий атрибуции, обязан быть представлен в титрах.
    for asset in timeline.assets() {
        if let Some(credit) = asset.credit() {
            if !credits_text.contains(credit.trim()) {
                return Err(BuildError::CreditMissing {
                    asset_id: asset.asset_id().to_string(),
                });
            }
        }
    }

    Ok(RenderPlan {
        asset_ids: timeline
            .assets()
            .iter()
            .map(|a| a.asset_id().to_string())
            .collect(),
        credits_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::gate::{admit, Timeline};
    use crate::media::provenance::{
        LicensedProvenance, Manifest, ManifestEntry, MediaKind, Provenance,
    };
    use chrono::Utc;

    fn timeline_with(entries: &[(&str, &str, &str)]) -> Timeline {
        let mut manifest = Manifest::new();
        for (id, license, attribution) in entries {
            manifest.insert(ManifestEntry {
                asset_id: id.to_string(),
                kind: MediaKind::Image,
                provenance: Provenance::Licensed(LicensedProvenance {
                    source: "openverse".to_string(),
                    asset_url: "https://example.org/a.jpg".to_string(),
                    landing_url: "https://example.org/p".to_string(),
                    license: license.to_string(),
                    license_version: "2.0".to_string(),
                    license_url: "https://creativecommons.org/licenses/by/2.0/".to_string(),
                    license_snapshot: "снимок".to_string(),
                    creator: Some("Автор".to_string()),
                    creator_url: None,
                    attribution: attribution.to_string(),
                    retrieved_at: Utc::now(),
                }),
            });
        }

        let mut timeline = Timeline::new();
        for (id, _, _) in entries {
            timeline.push(admit(&manifest, id).expect("ассет допущен гейтом"));
        }
        timeline
    }

    #[test]
    fn empty_timeline_cannot_be_rendered() {
        let timeline = Timeline::new();

        assert_eq!(plan_render(&timeline), Err(BuildError::EmptyTimeline));
    }

    #[test]
    fn cc_by_asset_forces_credits_into_the_plan() {
        let credit = "\"Mountains\" by Kiwi Tom is licensed under CC BY 2.0.";
        let timeline = timeline_with(&[("a1", "by", credit)]);

        let plan = plan_render(&timeline).expect("план собирается");

        assert!(
            plan.credits_text().contains("Kiwi Tom"),
            "титры обязаны называть автора CC-BY-материала"
        );
        assert!(plan.ffmpeg_args("out.mp4").iter().any(|a| a.contains("drawtext")));
    }

    #[test]
    fn public_domain_only_project_has_empty_credits_and_still_renders() {
        // CC0/PDM атрибуции не требуют — пустые титры здесь законны,
        // и сборка из-за них падать не должна.
        let timeline = timeline_with(&[("a1", "cc0", "")]);

        let plan = plan_render(&timeline).expect("план собирается");

        assert_eq!(plan.credits_text(), "");
    }

    #[test]
    fn duplicate_authors_are_not_repeated() {
        let credit = "\"X\" by Один Автор is licensed under CC BY 2.0.";
        let timeline = timeline_with(&[("a1", "by", credit), ("a2", "by", credit)]);

        let plan = plan_render(&timeline).expect("план собирается");

        assert_eq!(
            plan.credits_text().lines().count(),
            1,
            "один автор — одна строка титров"
        );
        assert_eq!(plan.asset_ids().len(), 2, "но оба ассета остаются на таймлайне");
    }

    #[test]
    fn mixed_project_credits_only_the_licenses_that_require_it() {
        let credit = "\"Y\" by Второй is licensed under CC BY 2.0.";
        let timeline = timeline_with(&[("pd", "pdm", ""), ("by", "by", credit)]);

        let plan = plan_render(&timeline).expect("план собирается");

        assert_eq!(plan.credits_text(), credit);
        assert_eq!(plan.asset_ids().len(), 2);
    }

    #[test]
    fn drawtext_special_characters_are_escaped() {
        // Двоеточие и апостроф ломают синтаксис фильтра drawtext.
        let credit = "\"A: B\" by O'Brien is licensed under CC BY 2.0.";
        let timeline = timeline_with(&[("a1", "by", credit)]);

        let plan = plan_render(&timeline).expect("план собирается");
        let args = plan.ffmpeg_args("out.mp4");
        let vf = args.iter().find(|a| a.contains("drawtext")).expect("есть drawtext");

        assert!(vf.contains("\\:"), "двоеточие должно быть экранировано");
        assert!(vf.contains("\\'"), "апостроф должен быть экранирован");
    }
}
