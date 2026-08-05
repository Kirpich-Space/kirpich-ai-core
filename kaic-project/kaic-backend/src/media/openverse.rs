//! Единственный источник ассетов в v1 — Openverse.
//!
//! Выбран потому, что отдаёт лицензию **структурированным полем**, а не
//! текстом на странице: `license`, `license_version`, `license_url` плюс
//! готовая каноническая строка `attribution`. Условия проверены реальным
//! запросом к `https://api.openverse.org/v1/images/`, не по памяти:
//!
//! ```text
//! license          = by
//! license_version  = 2.0
//! license_url      = https://creativecommons.org/licenses/by/2.0/
//! creator          = Kiwi Tom
//! attribution      = "mountain" by Kiwi Tom is licensed under CC BY 2.0. ...
//! ```
//!
//! Словарь лицензий тоже проверен запросами: `cc0`, `pdm`, `by`, `by-sa`,
//! `by-nd`, `by-nc`, `by-nc-nd` существуют, а несуществующее значение API
//! отвергает (`License 'sampling' does not exist.`) — то есть фильтр по
//! лицензии на стороне источника надёжен и опечатка не пройдёт молча.
//!
//! Аудио-эндпоинт Openverse (`/v1/audio/`) здесь НЕ используется: музыка
//! исключена из v1.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;

use super::provenance::{LicensedProvenance, ManifestEntry, MediaKind, Provenance};

const SOURCE_ID: &str = "openverse";
const API_IMAGES: &str = "https://api.openverse.org/v1/images/";

/// Openverse просит осмысленный User-Agent от клиентов.
const USER_AGENT: &str = "KAIC-media-pipeline/0.1";

// --- Форма ответа API --------------------------------------------------------
// Описаны только поля, которые действительно нужны манифесту. Остальные
// (`tags`, `mature`, `related_url`, ...) намеренно не объявлены.

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult {
    pub id: String,
    /// Название ассета у источника. В манифест не идёт (права от него не
    /// зависят), но разбирается — пригодится для человекочитаемых отчётов.
    #[allow(dead_code)]
    pub title: Option<String>,
    pub url: String,
    pub foreign_landing_url: String,
    pub license: String,
    pub license_version: Option<String>,
    pub license_url: Option<String>,
    pub creator: Option<String>,
    pub creator_url: Option<String>,
    pub attribution: Option<String>,
}

/// Превращает результат поиска в запись манифеста.
///
/// Чистая функция: ни сети, ни времени сборки — только данные ответа.
/// Гейт вызывается позже и отдельно; здесь ничего не «одобряется».
pub fn to_manifest_entry(result: &SearchResult) -> ManifestEntry {
    let license_url = result
        .license_url
        .clone()
        .unwrap_or_else(|| format!("https://creativecommons.org/licenses/{}/", result.license));

    // Снимок условий на момент получения: канонической строки атрибуции
    // и ссылки на текст лицензии достаточно, чтобы потом восстановить,
    // на каких условиях ассет был взят.
    let license_snapshot = format!(
        "{} | {} | версия {}",
        result.attribution.clone().unwrap_or_default(),
        license_url,
        result.license_version.clone().unwrap_or_default()
    );

    ManifestEntry {
        asset_id: format!("{SOURCE_ID}:{}", result.id),
        kind: MediaKind::Image,
        provenance: Provenance::Licensed(LicensedProvenance {
            source: SOURCE_ID.to_string(),
            asset_url: result.url.clone(),
            landing_url: result.foreign_landing_url.clone(),
            license: result.license.clone(),
            license_version: result.license_version.clone().unwrap_or_default(),
            license_url,
            license_snapshot,
            creator: result.creator.clone(),
            creator_url: result.creator_url.clone(),
            attribution: result.attribution.clone().unwrap_or_default(),
            retrieved_at: Utc::now(),
        }),
    }
}

/// Разбирает тело ответа поиска. Выделено отдельно, чтобы тестировать на
/// настоящем сохранённом ответе без обращения к сети.
pub fn parse_search_response(body: &str) -> Result<SearchResponse> {
    serde_json::from_str(body).context("не удалось разобрать ответ Openverse")
}

/// Ищет изображения. Фильтр по лицензии передаётся источнику, чтобы заведомо
/// негодные ассеты не приезжали вовсе — но это оптимизация, а не защита:
/// решение всё равно принимает гейт.
pub async fn search_images(
    client: &reqwest::Client,
    query: &str,
    licenses: &[&str],
    page_size: u8,
) -> Result<Vec<ManifestEntry>> {
    let response = client
        .get(API_IMAGES)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json")
        .query(&[
            ("q", query),
            ("license", &licenses.join(",") as &str),
            ("page_size", &page_size.to_string() as &str),
        ])
        .send()
        .await
        .context("Openverse недоступен")?;

    // Тело читается ДО проверки статуса: при ошибке именно оно объясняет
    // причину («License 'x' does not exist», лимит запросов и т.п.), а один
    // код 400 не говорит ничего.
    let status = response.status();
    let url = response.url().clone();
    let body = response
        .text()
        .await
        .context("не удалось прочитать ответ Openverse")?;

    if !status.is_success() {
        anyhow::bail!("Openverse ответил {status} на {url}: {body}");
    }

    let parsed = parse_search_response(&body)?;
    Ok(parsed.results.iter().map(to_manifest_entry).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::gate::admit;
    use crate::media::provenance::Manifest;

    /// Настоящий ответ Openverse, сохранённый реальным запросом
    /// (`?q=mountain&license=by&page_size=1`). Тест офлайновый: сети нет,
    /// но форма данных — не выдуманная.
    const REAL_RESPONSE: &str = include_str!("../../tests/openverse_by_sample.json");

    #[test]
    fn parses_real_openverse_response() {
        let parsed = parse_search_response(REAL_RESPONSE).expect("реальный ответ разбирается");

        assert!(!parsed.results.is_empty(), "в фикстуре есть результат");
        let first = &parsed.results[0];
        assert_eq!(first.license, "by");
        assert!(first.attribution.is_some(), "Openverse даёт готовую атрибуцию");
        assert!(first.url.starts_with("http"));
        assert!(first.foreign_landing_url.starts_with("http"));
    }

    #[test]
    fn real_response_becomes_manifest_entry_with_full_provenance() {
        let parsed = parse_search_response(REAL_RESPONSE).expect("разбирается");
        let entry = to_manifest_entry(&parsed.results[0]);

        assert!(entry.asset_id.starts_with("openverse:"));
        assert_eq!(entry.kind, MediaKind::Image);

        let Provenance::Licensed(p) = &entry.provenance else {
            panic!("ассет из источника обязан быть Licensed");
        };
        // Все пять обязательных элементов провенанса присутствуют.
        assert_eq!(p.source, "openverse");
        assert!(!p.asset_url.is_empty(), "URL");
        assert_eq!(p.license, "by", "идентификатор лицензии");
        assert!(p.creator.is_some(), "автор");
        assert!(!p.license_snapshot.is_empty(), "снимок текста лицензии");
        assert!(p.retrieved_at <= Utc::now(), "время получения");
    }

    #[test]
    fn entry_from_real_response_passes_the_gate_and_carries_credit() {
        let parsed = parse_search_response(REAL_RESPONSE).expect("разбирается");
        let entry = to_manifest_entry(&parsed.results[0]);
        let asset_id = entry.asset_id.clone();

        let mut manifest = Manifest::new();
        manifest.insert(entry);

        let admitted = admit(&manifest, &asset_id).expect("CC-BY из Openverse допускается");
        assert!(
            admitted.credit().is_some(),
            "CC-BY обязан принести строку титров"
        );
    }

    #[test]
    fn missing_optional_fields_do_not_panic() {
        // У Openverse часть полей опциональна (в живом ответе видели пустые
        // category/filesize/filetype). Отсутствие creator/attribution не
        // должно ронять разбор — ассет просто не пройдёт гейт.
        let minimal = r#"{"results":[{
            "id":"x1",
            "url":"https://example.org/a.jpg",
            "foreign_landing_url":"https://example.org/p",
            "license":"by"
        }]}"#;

        let parsed = parse_search_response(minimal).expect("минимальный ответ разбирается");
        let entry = to_manifest_entry(&parsed.results[0]);

        let asset_id = entry.asset_id.clone();
        let mut manifest = Manifest::new();
        manifest.insert(entry);

        // Атрибуции нет ⇒ титры собрать нечем ⇒ гейт обязан отказать.
        assert!(
            admit(&manifest, &asset_id).is_err(),
            "CC-BY без строки автора использовать нельзя"
        );
    }
}
