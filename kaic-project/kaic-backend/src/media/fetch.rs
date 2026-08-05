//! Загрузка ассетов и верификация их соответствия манифесту.
//!
//! Шаг между «план собран» и «ffmpeg что-то делает». Смысл не в том, чтобы
//! получить файл, а в том, чтобы получить **тот** файл, который описан в
//! манифесте: сработавший URL сам по себе ничего не доказывает.
//!
//! Набор проверок и почему именно он:
//!
//! 1. **HTTP-статус** — без него проверять нечего.
//! 2. **Потолок размера, считаемый по факту прочитанных байт.** Тело читается
//!    кусками со счётчиком, а не целиком: `Content-Length` может отсутствовать
//!    или врать, и доверять ему как единственной защите нельзя. Без потолка
//!    один URL способен выесть память процесса.
//! 3. **Непустое тело** — нулевой файл формально скачался, но бесполезен.
//! 4. **Сигнатура формата (магические байты).** Главная проверка. Манифест
//!    утверждает `MediaKind::Image`; заголовок `Content-Type` этого не
//!    доказывает — он может отсутствовать, врать или описывать HTML-страницу
//!    с ошибкой, отданную с кодом 200. Содержимое проверяется по байтам.
//! 5. **Совпадение с `kind` из манифеста.** Расхождение между заявленным и
//!    фактическим — именно то несоответствие, ради которого шаг существует.
//!
//! Чего здесь НЕТ: сверки контрольной суммы с источником. Openverse не отдаёт
//! ни хэша, ни размера, поэтому сравнивать не с чем — вычисленный на месте
//! хэш подтверждал бы сам себя.

use std::path::{Path, PathBuf};

use super::provenance::{ManifestEntry, MediaKind, Provenance};

/// Кем мы представляемся при скачивании файлов.
///
/// Не косметика: Wikimedia отвечает `403 Forbidden` на запрос без
/// `User-Agent` — «Please set a user-agent and respect our robot policy».
/// `reqwest` по умолчанию этот заголовок не отправляет вовсе, поэтому
/// ассеты с Wikimedia не скачивались, тогда как Flickr то же самое прощал.
/// В `openverse.rs` UA выставлялся для запросов к API — здесь была
/// асимметрия, а не осознанное решение.
///
/// Контакт в строке — требование той же политики: администратор сервера
/// должен понимать, кто к нему ходит, и иметь возможность связаться.
const DOWNLOAD_USER_AGENT: &str =
    "KAIC-media-pipeline/0.1 (personal local agent; +https://github.com/Kirpich-Space/kirpich-ai-core)";

/// Потолок на один ассет. 32 MiB с запасом покрывают фотографию любого
/// разумного разрешения и при этом ограничивают цену одного битого URL.
pub const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;

/// Минимум, ниже которого файл заведомо не изображение.
const MIN_ASSET_BYTES: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Gif,
    WebP,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }
}

/// Определяет формат по сигнатуре файла.
///
/// Проверяются именно байты, а не расширение URL и не `Content-Type`:
/// и то и другое задаётся сервером и может не соответствовать содержимому.
pub fn detect_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    // RIFF....WEBP — четыре байта размера между сигнатурами.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedAsset {
    pub asset_id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub format: ImageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// В манифесте нет URL: материал пользователя лежит локально и не качается.
    NotDownloadable { asset_id: String },
    Http { asset_id: String, detail: String },
    Empty { asset_id: String },
    TooLarge { asset_id: String, limit: u64 },
    /// Скачалось что-то, что не является изображением ни одного известного
    /// формата — например HTML-страница ошибки, отданная с кодом 200.
    NotAnImage { asset_id: String, head: String },
    /// Манифест заявляет один тип медиа, а скачано другое.
    KindMismatch {
        asset_id: String,
        declared: MediaKind,
    },
    Io { asset_id: String, detail: String },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDownloadable { asset_id } => {
                write!(f, "{asset_id}: нечего скачивать (материал пользователя)")
            }
            Self::Http { asset_id, detail } => write!(f, "{asset_id}: ошибка загрузки — {detail}"),
            Self::Empty { asset_id } => write!(f, "{asset_id}: пустой файл"),
            Self::TooLarge { asset_id, limit } => {
                write!(f, "{asset_id}: файл превышает лимит {limit} байт")
            }
            Self::NotAnImage { asset_id, head } => write!(
                f,
                "{asset_id}: содержимое не является изображением (первые байты: {head})"
            ),
            Self::KindMismatch { asset_id, declared } => write!(
                f,
                "{asset_id}: манифест заявляет {declared:?}, а скачано изображение"
            ),
            Self::Io { asset_id, detail } => write!(f, "{asset_id}: не удалось сохранить — {detail}"),
        }
    }
}

/// URL ассета из записи манифеста, если он вообще скачивается.
fn downloadable_url(entry: &ManifestEntry) -> Option<&str> {
    match &entry.provenance {
        Provenance::Licensed(p) => Some(p.asset_url.as_str()),
        // Материал пользователя уже лежит локально — качать нечего.
        Provenance::UserOwned(_) => None,
    }
}

/// Скачивает и верифицирует один ассет.
///
/// Ошибка относится к КОНКРЕТНОМУ ассету и не должна ронять задачу целиком —
/// вызывающий решает, хватает ли ему оставшихся.
pub async fn fetch_and_verify(
    client: &reqwest::Client,
    entry: &ManifestEntry,
    target_dir: &Path,
) -> Result<DownloadedAsset, FetchError> {
    let asset_id = entry.asset_id.clone();

    if entry.kind != MediaKind::Image {
        // Гейт не пропускает аудио, а видео-форматы этот шаг пока не проверяет:
        // молча принять непроверяемый тип было бы хуже, чем честно отказать.
        return Err(FetchError::KindMismatch {
            asset_id,
            declared: entry.kind,
        });
    }

    let Some(url) = downloadable_url(entry) else {
        return Err(FetchError::NotDownloadable { asset_id });
    };

    let mut response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, DOWNLOAD_USER_AGENT)
        .send()
        .await
        .map_err(|e| FetchError::Http {
            asset_id: asset_id.clone(),
            detail: e.to_string(),
        })?;

    if !response.status().is_success() {
        return Err(FetchError::Http {
            asset_id,
            detail: format!("HTTP {}", response.status()),
        });
    }

    // Чтение кусками со счётчиком: обрываем, как только превышен потолок,
    // не дожидаясь конца тела.
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = response.chunk().await.map_err(|e| FetchError::Http {
            asset_id: asset_id.clone(),
            detail: e.to_string(),
        })?;
        let Some(chunk) = chunk else { break };
        if body.len() as u64 + chunk.len() as u64 > MAX_ASSET_BYTES {
            return Err(FetchError::TooLarge {
                asset_id,
                limit: MAX_ASSET_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }

    if (body.len() as u64) < MIN_ASSET_BYTES {
        return Err(FetchError::Empty { asset_id });
    }

    let Some(format) = detect_image_format(&body) else {
        return Err(FetchError::NotAnImage {
            asset_id,
            head: head_hex(&body),
        });
    };

    std::fs::create_dir_all(target_dir).map_err(|e| FetchError::Io {
        asset_id: asset_id.clone(),
        detail: e.to_string(),
    })?;
    let path = target_dir.join(format!("{}.{}", safe_file_stem(&asset_id), format.extension()));
    std::fs::write(&path, &body).map_err(|e| FetchError::Io {
        asset_id: asset_id.clone(),
        detail: e.to_string(),
    })?;

    Ok(DownloadedAsset {
        asset_id,
        path,
        bytes: body.len() as u64,
        format,
    })
}

/// Первые байты в hex — для диагностики, когда пришло не изображение.
fn head_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Идентификатор ассета в имя файла: он приходит из внешнего источника и
/// содержит двоеточие, а на Windows это разделитель альтернативного потока.
fn safe_file_stem(asset_id: &str) -> String {
    asset_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provenance::{LicensedProvenance, UserProvenance};
    use chrono::Utc;

    fn entry(kind: MediaKind, provenance: Provenance) -> ManifestEntry {
        ManifestEntry {
            asset_id: "openverse:abc-123".to_string(),
            kind,
            provenance,
        }
    }

    fn licensed() -> Provenance {
        Provenance::Licensed(LicensedProvenance {
            source: "openverse".to_string(),
            asset_url: "https://example.org/a.jpg".to_string(),
            landing_url: "https://example.org/p".to_string(),
            license: "by".to_string(),
            license_version: "2.0".to_string(),
            license_url: "https://creativecommons.org/licenses/by/2.0/".to_string(),
            license_snapshot: "снимок".to_string(),
            creator: Some("Автор".to_string()),
            creator_url: None,
            attribution: "credit".to_string(),
            retrieved_at: Utc::now(),
        })
    }

    #[test]
    fn detects_real_image_signatures() {
        assert_eq!(detect_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageFormat::Jpeg));
        assert_eq!(
            detect_image_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some(ImageFormat::Png)
        );
        assert_eq!(detect_image_format(b"GIF89a...."), Some(ImageFormat::Gif));

        let mut webp = Vec::from(*b"RIFF");
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(detect_image_format(&webp), Some(ImageFormat::WebP));
    }

    #[test]
    fn html_error_page_is_not_an_image() {
        // Реальный сценарий: сервер отдал страницу ошибки с кодом 200.
        // Content-Type мог бы соврать, сигнатура — нет.
        assert_eq!(detect_image_format(b"<!DOCTYPE html><html>"), None);
        assert_eq!(detect_image_format(b""), None);
        // Обрезанный до одного байта JPEG тоже не считается форматом.
        assert_eq!(detect_image_format(&[0xFF]), None);
    }

    #[tokio::test]
    async fn user_material_is_not_downloadable() {
        let entry = entry(
            MediaKind::Image,
            Provenance::UserOwned(UserProvenance {
                original_path: "C:/photos/a.jpg".to_string(),
                rights_confirmation: "мои".to_string(),
                confirmed_at: Utc::now(),
            }),
        );

        let result = fetch_and_verify(
            &reqwest::Client::new(),
            &entry,
            &std::env::temp_dir().join("kaic-fetch-test"),
        )
        .await;

        assert!(matches!(result, Err(FetchError::NotDownloadable { .. })));
    }

    #[tokio::test]
    async fn non_image_kind_is_refused_before_any_network_call() {
        // Аудио гейт не пропускает; сюда оно попасть не должно, но если
        // попадёт — отказ обязан быть явным, а не молчаливым приёмом.
        let entry = entry(MediaKind::Audio, licensed());

        let result = fetch_and_verify(
            &reqwest::Client::new(),
            &entry,
            &std::env::temp_dir().join("kaic-fetch-test"),
        )
        .await;

        assert!(matches!(
            result,
            Err(FetchError::KindMismatch {
                declared: MediaKind::Audio,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn unreachable_host_is_an_error_not_a_panic() {
        let mut e = entry(MediaKind::Image, licensed());
        e.provenance = Provenance::Licensed(LicensedProvenance {
            asset_url: "http://127.0.0.1:1/nonexistent.jpg".to_string(),
            ..match licensed() {
                Provenance::Licensed(p) => p,
                _ => unreachable!(),
            }
        });

        let result = fetch_and_verify(
            &reqwest::Client::new(),
            &e,
            &std::env::temp_dir().join("kaic-fetch-test"),
        )
        .await;

        assert!(matches!(result, Err(FetchError::Http { .. })));
    }

    #[test]
    fn asset_id_becomes_a_safe_file_name() {
        // Двоеточие из идентификатора источника недопустимо в имени файла
        // на Windows — это разделитель альтернативного потока данных.
        let stem = safe_file_stem("openverse:f9812900-83f8-49a6");
        assert!(!stem.contains(':'));
        assert!(stem.starts_with("openverse_"));
    }

    #[test]
    fn head_hex_is_readable_for_diagnostics() {
        assert_eq!(head_hex(b"<!DOC"), "3c 21 44 4f 43");
    }
}
