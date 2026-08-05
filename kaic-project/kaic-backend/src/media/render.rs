//! Запуск ffmpeg: план + локальные файлы → видеофайл на диске.
//!
//! # Аргументы передаются массивом, а не строкой
//!
//! `tokio::process::Command` получает каждый аргумент отдельным вызовом
//! `.arg()`, и процесс запускается напрямую, без интерпретатора команд.
//! Это принципиально: текст титров приходит из внешнего источника
//! (строка `attribution` от Openverse) и содержит кавычки, апострофы,
//! тире и двоеточия. Экранирование под синтаксис фильтра `drawtext` —
//! отдельный слой и от shell не защищает: собери мы команду в одну строку,
//! экранированный под drawtext текст всё равно попал бы в оболочку.
//! Здесь оболочки нет вовсе, поэтому пробивать нечего.
//!
//! # Текст титров вообще не попадает в аргументы
//!
//! Используется `drawtext=textfile=...`, а не `text=...`: титры пишутся в
//! UTF-8 файл рядом с результатом, а ffmpeg читает их оттуда. Это снимает
//! многоуровневое экранирование содержимого (кавычки, переводы строк,
//! двоеточия) целиком — экранировать остаётся только путь к файлу.
//!
//! # Почему scale+pad, а не просто последовательность кадров
//!
//! Установлено замером: изображения из источника имеют разные размеры
//! (1023×682, 1024×576, 1024×768...). Демультиплексор последовательности
//! требует одинаковых, а libx264 отказывается кодировать нечётную ширину
//! («width not divisible by 2 (1023x682)»). Поэтому каждый вход отдельно
//! приводится к общему кадру, и только потом склейка.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::fetch::DownloadedAsset;

/// Имя исполняемого файла. Ищется в PATH — свою копию мы не ставим.
pub const FFMPEG_BIN: &str = "ffmpeg";

/// Разрешение итогового кадра. Чётные стороны обязательны для libx264.
const FRAME_WIDTH: u32 = 1280;
const FRAME_HEIGHT: u32 = 720;

/// Сколько секунд показывается один кадр.
const SECONDS_PER_IMAGE: u32 = 3;

/// Частота кадров результата.
const OUTPUT_FPS: u32 = 30;

/// Потолок времени на один рендер.
///
/// Значение не выбрано «на глаз»: замер на реальном материале этой задачи
/// (5 изображений → 15 секунд видео 1280×720) дал **1.8 секунды**. 300
/// секунд — примерно 165-кратный запас, покрывающий и заметно более длинный
/// ролик, и загруженную машину, и при этом не дающий зависшему процессу
/// держать задачу бесконечно.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(300);

/// Шрифт для титров.
///
/// Задаётся явно, потому что без `fontfile` drawtext на этой машине не
/// стартует вовсе — fontconfig в сборке ffmpeg отсутствует.
const FONT_PATH: &str = "C:/Windows/Fonts/arial.ttf";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// ffmpeg не найден в PATH. Мы его не устанавливаем — сообщаем.
    FfmpegUnavailable,
    NoInputs,
    Spawn(String),
    /// ffmpeg отработал, но вернул ненулевой код.
    Failed { code: Option<i32>, stderr_tail: String },
    TimedOut { seconds: u64 },
    Io(String),
    /// Процесс завершился успешно, но результат непригоден.
    EmptyOutput,
    /// В файле нет сигнатуры ISO-контейнера — успешный код возврата ещё не
    /// означает, что получилось видео.
    NotAnMp4 { head: String },
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FfmpegUnavailable => write!(
                f,
                "ffmpeg не найден в PATH — установите его и повторите (сами мы его не ставим)"
            ),
            Self::NoInputs => write!(f, "нечего рендерить: нет ни одного файла"),
            Self::Spawn(detail) => write!(f, "не удалось запустить ffmpeg: {detail}"),
            Self::Failed { code, stderr_tail } => match code {
                Some(code) => write!(f, "ffmpeg завершился с кодом {code}: {stderr_tail}"),
                None => write!(f, "ffmpeg прерван сигналом: {stderr_tail}"),
            },
            Self::TimedOut { seconds } => write!(f, "ffmpeg не уложился в {seconds} с и был снят"),
            Self::Io(detail) => write!(f, "ошибка файловой системы: {detail}"),
            Self::EmptyOutput => write!(f, "ffmpeg отчитался об успехе, но файл пуст"),
            Self::NotAnMp4 { head } => {
                write!(f, "результат не является MP4 (первые байты: {head})")
            }
        }
    }
}

/// Проверяет, доступен ли ffmpeg. Вызывается до сборки аргументов, чтобы
/// отличить «нет инструмента» от «инструмент отказался работать».
pub async fn ffmpeg_available() -> bool {
    tokio::process::Command::new(FFMPEG_BIN)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Экранирует путь для синтаксиса фильтра: двоеточие в нём разделяет опции.
fn escape_filter_path(path: &str) -> String {
    path.replace('\\', "/").replace(':', "\\:")
}

/// Собирает граф фильтров: каждый вход приводится к общему кадру, затем
/// склейка, затем титры поверх.
fn build_filter_complex(input_count: usize, credits_file: &Path) -> String {
    let mut chains = Vec::with_capacity(input_count);
    let mut labels = String::new();
    for i in 0..input_count {
        chains.push(format!(
            "[{i}:v]scale={FRAME_WIDTH}:{FRAME_HEIGHT}:force_original_aspect_ratio=decrease,\
             pad={FRAME_WIDTH}:{FRAME_HEIGHT}:(ow-iw)/2:(oh-ih)/2,setsar=1[v{i}]"
        ));
        labels.push_str(&format!("[v{i}]"));
    }

    format!(
        "{};{}concat=n={}:v=1:a=0[vc];[vc]drawtext=fontfile='{}':textfile='{}':\
         x=20:y=h-th-20:fontsize=20:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=10[vout]",
        chains.join(";"),
        labels,
        input_count,
        escape_filter_path(FONT_PATH),
        escape_filter_path(&credits_file.to_string_lossy())
    )
}

/// Полный набор аргументов ffmpeg. Возвращается вектором, чтобы каждый
/// элемент ушёл процессу отдельным argv — склейка в строку невозможна
/// по построению.
fn build_args(files: &[DownloadedAsset], filter: &str, output: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".to_string()];
    for file in files {
        args.push("-loop".to_string());
        args.push("1".to_string());
        args.push("-t".to_string());
        args.push(SECONDS_PER_IMAGE.to_string());
        args.push("-i".to_string());
        args.push(file.path.to_string_lossy().to_string());
    }
    args.push("-filter_complex".to_string());
    args.push(filter.to_string());
    args.push("-map".to_string());
    args.push("[vout]".to_string());
    args.push("-c:v".to_string());
    args.push("libx264".to_string());
    args.push("-pix_fmt".to_string());
    args.push("yuv420p".to_string());
    args.push("-r".to_string());
    args.push(OUTPUT_FPS.to_string());
    args.push(output.to_string_lossy().to_string());
    args
}

/// Признак ISO-контейнера: `ftyp` на смещении 4.
fn looks_like_mp4(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[4..8] == b"ftyp"
}

/// Рендерит слайдшоу из скачанных ассетов с вжатыми титрами.
pub async fn render_slideshow(
    files: &[DownloadedAsset],
    credits: &str,
    out_dir: &Path,
) -> Result<RenderOutcome, RenderError> {
    if files.is_empty() {
        return Err(RenderError::NoInputs);
    }
    if !ffmpeg_available().await {
        return Err(RenderError::FfmpegUnavailable);
    }

    std::fs::create_dir_all(out_dir).map_err(|e| RenderError::Io(e.to_string()))?;

    // Титры уходят в файл, а не в аргумент: см. заметку в шапке модуля.
    let credits_file = out_dir.join("credits.txt");
    std::fs::write(&credits_file, credits).map_err(|e| RenderError::Io(e.to_string()))?;

    let output = out_dir.join("render.mp4");
    let filter = build_filter_complex(files.len(), &credits_file);
    let args = build_args(files, &filter, &output);

    let mut command = tokio::process::Command::new(FFMPEG_BIN);
    // Каждый аргумент — отдельный элемент argv. Оболочка не участвует.
    for arg in &args {
        command.arg(arg);
    }

    let started = Instant::now();
    let result = tokio::time::timeout(RENDER_TIMEOUT, command.output()).await;

    let output_result = match result {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => return Err(RenderError::Spawn(err.to_string())),
        Err(_) => {
            return Err(RenderError::TimedOut {
                seconds: RENDER_TIMEOUT.as_secs(),
            })
        }
    };
    let elapsed_ms = started.elapsed().as_millis();

    if !output_result.status.success() {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        return Err(RenderError::Failed {
            code: output_result.status.code(),
            stderr_tail: tail_lines(&stderr, 3),
        });
    }

    // Нулевой код возврата — ещё не результат: проверяем сам файл.
    let bytes = std::fs::metadata(&output)
        .map_err(|e| RenderError::Io(e.to_string()))?
        .len();
    if bytes == 0 {
        return Err(RenderError::EmptyOutput);
    }

    let head = std::fs::read(&output)
        .map_err(|e| RenderError::Io(e.to_string()))?
        .into_iter()
        .take(12)
        .collect::<Vec<u8>>();
    if !looks_like_mp4(&head) {
        return Err(RenderError::NotAnMp4 {
            head: head
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" "),
        });
    }

    Ok(RenderOutcome {
        path: output,
        bytes,
        elapsed_ms,
    })
}

/// Последние строки stderr — ffmpeg многословен, а причина всегда в конце.
fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fetch::ImageFormat;

    fn asset(path: &str) -> DownloadedAsset {
        DownloadedAsset {
            asset_id: "openverse:test".to_string(),
            path: PathBuf::from(path),
            bytes: 1024,
            format: ImageFormat::Jpeg,
        }
    }

    #[test]
    fn every_argument_is_a_separate_argv_element() {
        // Ключевое свойство: текст титров и пути не склеиваются в строку.
        // Каждый путь — отдельный элемент, даже если содержит пробелы.
        let files = vec![asset("C:/Кирпич Space/a b.jpg")];
        let args = build_args(&files, "filter", Path::new("C:/out dir/render.mp4"));

        assert!(args.contains(&"C:/Кирпич Space/a b.jpg".to_string()));
        assert!(args.contains(&"C:/out dir/render.mp4".to_string()));
        assert!(args.contains(&"filter".to_string()));
        // Ни один элемент не содержит склейки двух аргументов через пробел.
        assert!(!args.iter().any(|a| a.contains("-i C:/")));
    }

    #[test]
    fn filter_escapes_windows_paths() {
        let filter = build_filter_complex(2, Path::new("C:/renders/credits.txt"));

        // Двоеточие диска обязано быть экранировано — иначе оно разделит
        // опции фильтра.
        assert!(filter.contains("C\\:/renders/credits.txt"));
        assert!(filter.contains("C\\:/Windows/Fonts/arial.ttf"));
        // Оба входа приведены к общему кадру и склеены.
        assert!(filter.contains("[0:v]scale=1280:720"));
        assert!(filter.contains("[1:v]scale=1280:720"));
        assert!(filter.contains("concat=n=2:v=1:a=0"));
    }

    #[test]
    fn credits_text_never_becomes_an_argument() {
        // Титры приходят из внешнего источника и содержат кавычки, тире и
        // двоеточия. В аргументы уходит путь к файлу, а не сам текст.
        let nasty = "\"A: B\" by O'Brien — CC BY 2.0; rm -rf /";
        let filter = build_filter_complex(1, Path::new("C:/r/credits.txt"));
        let args = build_args(&[asset("a.jpg")], &filter, Path::new("out.mp4"));

        assert!(!args.iter().any(|a| a.contains(nasty)));
        assert!(filter.contains("textfile="));
        assert!(!filter.contains("text='"));
    }

    #[test]
    fn mp4_signature_is_checked_not_assumed() {
        let mut good = vec![0u8, 0, 0, 0x20];
        good.extend_from_slice(b"ftypisom");
        assert!(looks_like_mp4(&good));

        assert!(!looks_like_mp4(b"<!DOCTYPE html>"));
        assert!(!looks_like_mp4(&[]));
    }

    #[test]
    fn empty_input_list_is_refused() {
        // Отдельная проверка до всякого запуска процесса.
        let filter = build_filter_complex(0, Path::new("c.txt"));
        assert!(filter.contains("concat=n=0"));
    }

    #[tokio::test]
    async fn ffmpeg_is_present_on_this_machine() {
        // Не мок: реально запускает `ffmpeg -version`.
        assert!(
            ffmpeg_available().await,
            "ffmpeg должен быть в PATH для медиа-задач"
        );
    }

    #[tokio::test]
    async fn no_inputs_fails_before_spawning() {
        let result = render_slideshow(&[], "credits", Path::new("./unused")).await;
        assert_eq!(result, Err(RenderError::NoInputs));
    }

    #[tokio::test]
    async fn broken_asset_path_gives_failed_not_panic() {
        // Живая проверка отказа: ffmpeg реально запускается и реально
        // отвергает несуществующий вход.
        let dir = std::env::temp_dir().join(format!("kaic-render-fail-{}", uuid::Uuid::new_v4()));
        let files = vec![asset("C:/definitely/missing/asset.jpg")];

        let result = render_slideshow(&files, "credits", &dir).await;

        match result {
            Err(RenderError::Failed { code, stderr_tail }) => {
                assert_ne!(code, Some(0), "код возврата должен быть ненулевым");
                assert!(!stderr_tail.is_empty(), "причина обязана попасть в отчёт");
            }
            other => panic!("ожидался Failed от ffmpeg, получено {other:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
