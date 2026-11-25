//! Локальный нативный движок генерации фото и видео на базе stable-diffusion.cpp (`sd-cli` / CUDA).
use crate::{
    catalog::{Model, ModelKind},
    network::Cancel,
    storage::{GeneratedMedia, MediaGenOptions},
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Default)]
struct CompanionFiles {
    vae: Option<PathBuf>,
    t5xxl: Option<PathBuf>,
    clip_l: Option<PathBuf>,
    llm: Option<PathBuf>,
}

/// Проверяет, доступен ли бинарник `sd-cli` на диске.
pub fn binary_exists(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| dir.join(trimmed).is_file())
}

fn scan_dir_for_companions(dir: &Path, model_name: &str, found: &mut CompanionFiles) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name == model_name || name.contains(".part") {
            continue;
        }
        if !(name.ends_with(".safetensors") || name.ends_with(".gguf")) {
            continue;
        }
        if found.vae.is_none() && (name.contains("vae") || name.starts_with("ae.")) {
            found.vae = Some(p);
        } else if found.t5xxl.is_none()
            && (name.contains("t5xxl")
                || name.contains("umt5")
                || name.contains("t5-v1")
                || name.contains("t5_"))
        {
            found.t5xxl = Some(p);
        } else if found.clip_l.is_none() && name.contains("clip_l") {
            found.clip_l = Some(p);
        } else if found.llm.is_none()
            && (name.contains("qwen2") || name.contains("qwen3") || name.contains("gemma"))
            && !name.contains("image")
        {
            found.llm = Some(p);
        }
    }
}

fn discover_companions(model_path: &Path) -> CompanionFiles {
    let mut found = CompanionFiles::default();
    let model_name = model_path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if let Some(dir) = model_path.parent() {
        scan_dir_for_companions(dir, &model_name, &mut found);
    }
    // Если одиночный GGUF скачан через Discover (HF) в отдельную папку без VAE/энкодеров,
    // подхватываем установленные VAE/T5/CLIP из базового каталога /home/kirpich/models
    if found.vae.is_none() && model_path.exists() {
        if model_name.contains("flux") {
            for d in [
                "/home/kirpich/.local/opt/kaic-studio/models/flux1-schnell",
                "/home/kirpich/models/flux1-schnell",
            ] {
                scan_dir_for_companions(Path::new(d), &model_name, &mut found);
            }
        } else if model_name.contains("wan") {
            for d in [
                "/home/kirpich/.local/opt/kaic-studio/models/wan2.1-t2v-1.3b",
                "/home/kirpich/models/wan2.1-t2v-1.3b",
            ] {
                scan_dir_for_companions(Path::new(d), &model_name, &mut found);
            }
        }
    }
    found
}

/// Очищает русские командные префиксы и переводит кириллические промпты на английский
/// для диффузионных моделей с англоязычными текстовыми энкодерами (CLIP-L / T5-v1.1-XXL в FLUX.1).
pub fn prepare_diffusion_prompt(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed
        .chars()
        .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
    {
        return trimmed.to_string();
    }
    let low = trimmed.to_lowercase();
    let mut parts: Vec<&'static str> = Vec::new();

    const DICT: &[(&str, &str)] = &[
        (
            "фотореалист",
            "photorealistic, ultra detailed, 8k raw photo",
        ),
        ("реалистич", "realistic photograph, natural lighting"),
        (
            "кинематограф",
            "cinematic shot, dramatic lighting, film grain",
        ),
        ("макро", "macro studio photography, shallow depth of field"),
        ("изометр", "clean isometric 3d technical render"),
        ("аниме", "anime artwork, vibrant colors, crisp lineart"),
        (
            "киберпанк",
            "cyberpunk aesthetic, neon lights, dark atmosphere",
        ),
        (
            "взлёт ракет",
            "orbital rocket launch liftoff from launchpad, glowing engine ignition flame, billowing volumetric exhaust smoke and steam clouds",
        ),
        (
            "взлет ракет",
            "orbital rocket launch liftoff from launchpad, glowing engine ignition flame, billowing volumetric exhaust smoke and steam clouds",
        ),
        (
            "запуск ракет",
            "orbital rocket launch ignition on launchpad, dramatic orange engine exhaust flame and thick steam clouds",
        ),
        (
            "ракет",
            "stainless steel orbital space rocket on launchpad, aerospace engineering details",
        ),
        ("космос", "outer space, stars, Earth horizon in background"),
        (
            "орбит",
            "low Earth orbit view, glowing atmosphere curvature",
        ),
        (
            "спутник",
            "spacecraft satellite in orbit, solar panels, metallic foil",
        ),
        (
            "плат",
            "matte black electronic PCB circuit board with gold traces and microchips",
        ),
        (
            "микросхем",
            "silicon microchip processor on dark PCB, studio lighting",
        ),
        (
            "чип",
            "AI semiconductor chip module, brushed aluminum heatsink",
        ),
        (
            "сервер",
            "dark local AI server rack cluster with warm amber status LEDs",
        ),
        (
            "лаборатор",
            "dark aerospace engineering telemetry laboratory with oscilloscopes",
        ),
        (
            "робот",
            "futuristic industrial humanoid robot, mechanical joints, carbon fiber",
        ),
        ("кот", "fluffy cat, detailed fur, expressive eyes"),
        ("кошк", "graceful cat, detailed fur, soft lighting"),
        ("собак", "loyal dog, detailed fur, natural outdoor light"),
        (
            "гор",
            "majestic snow-capped mountain ridge, dramatic clouds",
        ),
        ("лес", "misty pine forest, sunbeams through trees"),
        ("море", "ocean waves, wet reflections, coastal horizon"),
        ("океан", "deep ocean waves, dramatic sky"),
        ("город", "modern city skyline, architectural details"),
        ("улиц", "atmospheric city street, detailed buildings"),
        ("ночь", "night scene, dramatic contrast, moody lighting"),
        ("закат", "golden hour sunset, warm orange and amber sky"),
        ("рассвет", "crisp morning sunrise, soft volumetric mist"),
        ("машин", "sleek modern sports car, glossy reflections"),
        (
            "автомобил",
            "modern automobile on wet asphalt, studio lighting",
        ),
        (
            "человек",
            "cinematic portrait of a person, detailed skin texture",
        ),
        (
            "девушк",
            "portrait of a young woman, soft studio lighting, 85mm lens",
        ),
        (
            "мужчин",
            "portrait of a man, dramatic rim lighting, 85mm lens",
        ),
        ("дом", "modern architectural house, warm interior lights"),
        ("огон", "glowing fire flames and sparks"),
        ("дым", "volumetric smoke clouds"),
        ("небо", "dramatic sky with volumetric clouds"),
        (
            "милый эп",
            "classic Soviet youth literature book illustration, teenage boy in warm nostalgic autumn courtyard, expressive painterly brushstrokes, warm golden light",
        ),
        (
            "михасенко",
            "classic nostalgic book art, heartfelt youth story atmosphere, detailed ink and watercolor illustration",
        ),
        (
            "иллюстрац",
            "expressive book illustration, rich storytelling composition, detailed painterly artwork",
        ),
        (
            "произведен",
            "literary story scene, evocative narrative atmosphere",
        ),
        (
            "книг",
            "vintage book cover illustration, painterly textures",
        ),
        (
            "повест",
            "classic literary novella scene, atmospheric storytelling",
        ),
        (
            "рассказ",
            "storytelling illustration, expressive character scene",
        ),
        (
            "рисунок",
            "detailed hand-drawn artwork, crisp lines and shading",
        ),
        (
            "портрет",
            "expressive character portrait, dramatic lighting, painterly detail",
        ),
        (
            "пейзаж",
            "scenic landscape painting, rich depth and atmospheric light",
        ),
        (
            "обложк",
            "dramatic book cover art, strong focal composition",
        ),
        (
            "плакат",
            "bold vintage poster illustration, graphic composition",
        ),
        ("постер", "cinematic poster artwork, dramatic lighting"),
        ("мальчик", "young teenage boy character, expressive face"),
        ("юнош", "young man character, thoughtful expression"),
        ("девочк", "young girl character, warm natural light"),
        (
            "комнат",
            "cozy lived-in room interior, warm lamp light, wooden furniture",
        ),
        (
            "школ",
            "classic school hallway or classroom, nostalgic warm sunlight",
        ),
        (
            "двор",
            "quiet residential courtyard with trees, warm golden afternoon",
        ),
        ("лето", "warm sunny summer day, lush green foliage"),
        ("осень", "golden autumn leaves, warm nostalgic atmosphere"),
        (
            "зима",
            "snowy winter scene, soft cool twilight, warm window lights",
        ),
        (
            "весна",
            "fresh spring morning, blooming trees, soft sunlight",
        ),
        (
            "дожд",
            "rainy street reflections, moody atmospheric lighting",
        ),
        ("снег", "falling snowflakes, soft winter light"),
        ("солнц", "warm golden sunbeams"),
        ("луна", "glowing full moon in night sky"),
        ("звезд", "starry night sky"),
        ("окн", "sunlight streaming through window"),
    ];

    let mut matched_keys: Vec<&str> = Vec::new();
    for &(ru_stem, en_desc) in DICT {
        if low.contains(ru_stem) {
            // Не дублируем «ракет», если уже сработал «взлёт ракет»
            if ru_stem == "ракет" && matched_keys.iter().any(|k| k.contains("ракет")) {
                continue;
            }
            matched_keys.push(ru_stem);
            parts.push(en_desc);
        }
    }

    // Сохраняем все латинские слова и числа из исходного промпта
    let ascii_words: Vec<&str> = trimmed
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_ascii_alphanumeric()))
        .collect();
    if !parts.is_empty() {
        let mut out = parts.join(", ");
        if !ascii_words.is_empty() {
            out.push_str(", ");
            out.push_str(&ascii_words.join(" "));
        }
        out
    } else {
        format!("{trimmed}, high quality, detailed, sharp focus")
    }
}

fn prefers_diffusion_flag(
    model: &Model,
    kind: ModelKind,
    companions: &CompanionFiles,
    opts: &MediaGenOptions,
) -> bool {
    if kind == ModelKind::Video
        || !opts.vae_path.trim().is_empty()
        || !opts.text_encoder_path.trim().is_empty()
        || companions.vae.is_some()
        || companions.t5xxl.is_some()
        || companions.llm.is_some()
    {
        return true;
    }
    let hay = format!("{} {}", model.repo, model.filename).to_ascii_lowercase();
    [
        "flux",
        "qwen-image",
        "qwen_image",
        "z-image",
        "z_image",
        "chroma",
        "hidream",
        "lumina",
        "sd3",
        "sd-3",
    ]
    .iter()
    .any(|k| hay.contains(k))
}

/// Формирует безопасный набор CLI-аргументов для `sd-cli`.
pub fn build_args(
    model: &Model,
    kind: ModelKind,
    prompt: &str,
    opts: &MediaGenOptions,
    out_path: &Path,
) -> Vec<String> {
    let companions = discover_companions(&model.path);
    let mut args = Vec::new();
    args.push("-M".into());
    args.push(match kind {
        ModelKind::Video => "vid_gen".into(),
        _ => "img_gen".into(),
    });

    if prefers_diffusion_flag(model, kind, &companions, opts) {
        args.push("--diffusion-model".into());
    } else {
        args.push("--model".into());
    }
    args.push(model.path.to_string_lossy().into_owned());

    let vae = if !opts.vae_path.trim().is_empty() {
        Some(PathBuf::from(opts.vae_path.trim()))
    } else {
        companions.vae
    };
    if let Some(p) = vae {
        args.push("--vae".into());
        args.push(p.to_string_lossy().into_owned());
    }

    if !opts.text_encoder_path.trim().is_empty() {
        let p = opts.text_encoder_path.trim();
        let low = p.to_ascii_lowercase();
        let flag = if low.contains("t5") {
            "--t5xxl"
        } else if low.contains("qwen") || low.contains("llama") || low.contains("gemma") {
            "--llm"
        } else {
            "--clip_l"
        };
        args.push(flag.into());
        args.push(p.to_string());
    } else {
        if let Some(p) = companions.t5xxl {
            args.push("--t5xxl".into());
            args.push(p.to_string_lossy().into_owned());
        }
        if let Some(p) = companions.clip_l {
            args.push("--clip_l".into());
            args.push(p.to_string_lossy().into_owned());
        }
        if let Some(p) = companions.llm {
            args.push("--llm".into());
            args.push(p.to_string_lossy().into_owned());
        }
    }

    let hay = format!("{} {}", model.repo, model.filename).to_ascii_lowercase();
    let is_flux = hay.contains("flux");
    let is_schnell = is_flux && hay.contains("schnell");
    let is_wan = hay.contains("wan");

    let w = opts.width.clamp(128, 2048);
    let h = opts.height.clamp(128, 2048);
    // Для FLUX.1-schnell дефолт 20 шагов избыточен — 4 шага дают оптимальный результат и в 5× быстрее
    let steps = if is_schnell && opts.steps == 20 {
        4
    } else {
        opts.steps.clamp(1, 100)
    };
    let cfg = if is_flux && (opts.cfg_scale - 5.0).abs() < 0.01 {
        1.0
    } else {
        opts.cfg_scale.clamp(0.5, 20.0)
    };

    args.push("--prompt".into());
    args.push(prompt.to_string());
    if !opts.negative_prompt.trim().is_empty() {
        args.push("--negative-prompt".into());
        args.push(opts.negative_prompt.trim().to_string());
    }
    args.push("--width".into());
    args.push(w.to_string());
    args.push("--height".into());
    args.push(h.to_string());
    args.push("--steps".into());
    args.push(steps.to_string());
    args.push("--cfg-scale".into());
    args.push(format!("{cfg:.2}"));
    if is_flux || is_wan {
        args.push("--sampling-method".into());
        args.push("euler".into());
    }
    if is_wan {
        args.push("--flow-shift".into());
        args.push("3.0".into());
    }
    if opts.seed >= 0 {
        args.push("--seed".into());
        args.push(opts.seed.to_string());
    }
    if kind == ModelKind::Video {
        args.push("--video-frames".into());
        args.push(opts.video_frames.clamp(5, 129).to_string());
        args.push("--fps".into());
        args.push(opts.fps.clamp(4, 60).to_string());
    }
    if opts.offload_to_cpu && (kind == ModelKind::Video || is_flux || model.size > 5_000_000_000) {
        args.push("--offload-to-cpu".into());
        if is_flux {
            args.push("--clip-on-cpu".into());
        }
    }
    if opts.vae_tiling {
        args.push("--vae-tiling".into());
    }
    args.push("--output".into());
    args.push(out_path.to_string_lossy().into_owned());
    args
}

/// Запускает `sd-cli` для генерации реального PNG или MP4 и отслеживает отмену и прогресс.
pub async fn generate(
    sd_cli: &str,
    model: &Model,
    kind: ModelKind,
    prompt: &str,
    opts: &MediaGenOptions,
    outputs_dir: &Path,
    cancel: &Cancel,
    mut on_status: impl FnMut(String),
) -> Result<GeneratedMedia> {
    // Если выбран вспомогательный файл энкодера (например qwen3vl_32b_minimax_h3 без весов диффузии и VAE),
    // автоматически переключаемся на полный установленный комплект генерации фото/видео
    let effective_model = if crate::catalog::is_auxiliary_encoder(&model.filename) {
        crate::catalog::find_working_media_model(&[], kind).unwrap_or_else(|| model.clone())
    } else {
        model.clone()
    };
    let model = &effective_model;
    if !model.loadable() {
        bail!("Модель не проверена или файл отсутствует на диске");
    }
    let bin = sd_cli.trim();
    if !binary_exists(bin) {
        bail!(
            "Не найден движок sd-cli ({bin}). Проверьте путь в Settings → Image & Video (stable-diffusion.cpp)."
        );
    }
    fs::create_dir_all(outputs_dir).context("Не удалось создать каталог outputs")?;

    // Если для генерации видео используется самодостаточная фото-модель (например DreamShaper 8),
    // генерируем кадр через `-M img_gen` на CUDA и рендерим плавный H.264 MP4-видеоролик через ffmpeg.
    let synth_video_from_image = kind == ModelKind::Video && model.kind() == ModelKind::Image;
    let sd_mode_kind = if synth_video_from_image {
        ModelKind::Image
    } else {
        kind
    };

    // Для нативной видео-модели (Wan 2.1) на RTX 4060 8 ГБ при дефолтных настройках (512×512, 20 шагов, 17 кадров)
    // автоматически используем быстрый проверенный профиль 384×256 · 10 шагов · 9 кадров (~9 сек)
    let mut effective_opts = opts.clone();
    if sd_mode_kind == ModelKind::Video
        && effective_opts.width == 512
        && effective_opts.height == 512
        && effective_opts.steps == 20
        && effective_opts.video_frames == 17
    {
        effective_opts.width = 384;
        effective_opts.height = 256;
        effective_opts.steps = 10;
        effective_opts.video_frames = 9;
    }
    let opts = &effective_opts;
    let effective_prompt = prepare_diffusion_prompt(prompt);
    let prompt = effective_prompt.as_str();

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let raw_out = match sd_mode_kind {
        ModelKind::Video => outputs_dir.join(format!("gen-{stamp}.avi")),
        _ => outputs_dir.join(format!("gen-{stamp}.png")),
    };
    let args = build_args(model, sd_mode_kind, prompt, opts, &raw_out);

    on_status(match kind {
        ModelKind::Video => format!(
            "Generating video ({}×{}, {} frames)…",
            opts.width, opts.height, opts.video_frames
        ),
        _ => format!(
            "Generating image ({}×{}, {} steps)…",
            opts.width, opts.height, opts.steps
        ),
    });

    let started = Instant::now();
    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            let _ = crate::platform::set_parent_death_signal();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Не удалось запустить sd-cli ({bin})"))?;

    let (tx, rx) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);

    let mut tail_logs: Vec<String> = Vec::new();
    let exit_status = loop {
        while let Ok(line) = rx.try_recv() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if trimmed.contains("step")
                    || trimmed.contains('|')
                    || trimmed.contains("loading")
                    || trimmed.contains("generating")
                    || trimmed.contains("decoding")
                {
                    let short: String = trimmed.chars().take(90).collect();
                    on_status(short);
                }
                if tail_logs.len() >= 12 {
                    tail_logs.remove(0);
                }
                tail_logs.push(trimmed.to_string());
            }
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&raw_out);
            bail!("Генерация остановлена пользователем");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    };

    while let Ok(line) = rx.try_recv() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if tail_logs.len() >= 12 {
                tail_logs.remove(0);
            }
            tail_logs.push(trimmed.to_string());
        }
    }

    if !exit_status.success() {
        let detail = tail_logs
            .iter()
            .rev()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        bail!(
            "sd-cli завершился с ошибкой ({exit_status}). {}",
            if detail.is_empty() {
                "Проверьте совместимость модели и наличие VAE/text-encoder файлов."
            } else {
                &detail
            }
        );
    }

    if !raw_out.is_file() || fs::metadata(&raw_out).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("sd-cli завершился, но не создал выходной файл");
    }

    let elapsed = started.elapsed().as_secs_f32();
    if kind == ModelKind::Video {
        let mp4_path = outputs_dir.join(format!("gen-{stamp}.mp4"));
        let poster_path = outputs_dir.join(format!("gen-{stamp}.poster.png"));
        on_status("Encoding H.264 MP4…".into());
        if synth_video_from_image {
            let _ = fs::copy(&raw_out, &poster_path);
            let frames = opts.video_frames.clamp(16, 120);
            let fps = opts.fps.clamp(8, 30);
            let w = (opts.width.clamp(256, 1280) / 2) * 2;
            let h = (opts.height.clamp(256, 1280) / 2) * 2;
            let vf = format!(
                "scale=1024:1024,zoompan=z='min(zoom+0.0035,1.15)':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s={w}x{h}:fps={fps}"
            );
            let ff = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-loop",
                    "1",
                    "-i",
                    &raw_out.to_string_lossy(),
                    "-vf",
                    &vf,
                    "-frames:v",
                    &frames.to_string(),
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    "+faststart",
                    &mp4_path.to_string_lossy(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let final_video = if ff.is_ok_and(|s| s.success()) && mp4_path.is_file() {
                let _ = fs::remove_file(&raw_out);
                mp4_path
            } else {
                raw_out
            };
            return Ok(GeneratedMedia {
                kind: "video".into(),
                path: final_video,
                preview_path: poster_path.is_file().then_some(poster_path),
                width: w,
                height: h,
                frames: Some(frames),
                fps: Some(fps),
                seed: (opts.seed >= 0).then_some(opts.seed),
                steps: Some(opts.steps),
                elapsed_secs: Some(started.elapsed().as_secs_f32()),
            });
        }
        let ff = Command::new("ffmpeg")
            .args([
                "-y",
                "-i",
                &raw_out.to_string_lossy(),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                &mp4_path.to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let final_video = if ff.is_ok_and(|s| s.success()) && mp4_path.is_file() {
            let _ = fs::remove_file(&raw_out);
            mp4_path
        } else {
            raw_out
        };
        let poster_ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-i",
                &final_video.to_string_lossy(),
                "-vframes",
                "1",
                &poster_path.to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
            && poster_path.is_file();
        let (w, h) = if poster_ok {
            image::image_dimensions(&poster_path).unwrap_or((opts.width, opts.height))
        } else {
            (opts.width, opts.height)
        };
        Ok(GeneratedMedia {
            kind: "video".into(),
            path: final_video,
            preview_path: poster_ok.then_some(poster_path),
            width: w,
            height: h,
            frames: Some(opts.video_frames),
            fps: Some(opts.fps),
            seed: (opts.seed >= 0).then_some(opts.seed),
            steps: Some(opts.steps),
            elapsed_secs: Some(elapsed),
        })
    } else {
        let (w, h) = image::image_dimensions(&raw_out)
            .context("Созданный файл не является корректным изображением PNG/JPEG")?;
        Ok(GeneratedMedia {
            kind: "image".into(),
            path: raw_out,
            preview_path: None,
            width: w,
            height: h,
            frames: None,
            fps: None,
            seed: (opts.seed >= 0).then_some(opts.seed),
            steps: Some(opts.steps),
            elapsed_secs: Some(elapsed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::classify_model;

    #[test]
    fn classify_model_distinguishes_text_image_and_video() {
        assert_eq!(
            classify_model("unsloth/Qwen3-8B-GGUF", "Qwen3-8B-Q4_K_M.gguf"),
            ModelKind::Text
        );
        assert_eq!(
            classify_model(
                "unsloth/Qwen-Image-2.1-Turbo-GGUF",
                "qwen-image-Q4_K_M.gguf"
            ),
            ModelKind::Image
        );
        assert_eq!(
            classify_model("city96/FLUX.1-schnell-gguf", "flux1-schnell-Q4_K_S.gguf"),
            ModelKind::Image
        );
        assert_eq!(
            classify_model("city96/Wan2.1-T2V-1.3B-gguf", "wan2.1-t2v-1.3b-Q4_K_M.gguf"),
            ModelKind::Video
        );
    }

    #[test]
    fn build_args_sets_vid_gen_and_offload_flags_for_video_models() {
        let model = Model {
            repo: "city96/Wan2.1-T2V-1.3B-gguf".into(),
            filename: "wan2.1-t2v-1.3b-Q4_K_M.gguf".into(),
            path: PathBuf::from("/tmp/wan2.1-t2v-1.3b-Q4_K_M.gguf"),
            size: 100,
            sha256: String::new(),
            status: "verified".into(),
            generation_tps: None,
            meets_10_tps: None,
            chat_exit: None,
        };
        let opts = MediaGenOptions {
            width: 832,
            height: 480,
            steps: 15,
            video_frames: 17,
            fps: 16,
            seed: 42,
            offload_to_cpu: true,
            vae_tiling: true,
            ..Default::default()
        };
        let args = build_args(
            &model,
            ModelKind::Video,
            "rocket launch",
            &opts,
            Path::new("/tmp/out.avi"),
        );
        assert!(args.windows(2).any(|w| w == ["-M", "vid_gen"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--diffusion-model", "/tmp/wan2.1-t2v-1.3b-Q4_K_M.gguf"])
        );
        assert!(args.windows(2).any(|w| w == ["--video-frames", "17"]));
        assert!(args.windows(2).any(|w| w == ["--fps", "16"]));
        assert!(args.windows(2).any(|w| w == ["--seed", "42"]));
        assert!(args.contains(&"--offload-to-cpu".to_string()));
        assert!(args.contains(&"--vae-tiling".to_string()));
    }

    #[test]
    fn build_args_uses_model_flag_for_sdxl_checkpoint() {
        let model = Model {
            repo: "local/sdxl-turbo".into(),
            filename: "sdxl-turbo.safetensors".into(),
            path: PathBuf::from("/tmp/sdxl-turbo.safetensors"),
            size: 100,
            sha256: String::new(),
            status: "verified".into(),
            generation_tps: None,
            meets_10_tps: None,
            chat_exit: None,
        };
        let opts = MediaGenOptions::default();
        let args = build_args(
            &model,
            ModelKind::Image,
            "cyberpunk lab",
            &opts,
            Path::new("/tmp/out.png"),
        );
        assert!(args.windows(2).any(|w| w == ["-M", "img_gen"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--model", "/tmp/sdxl-turbo.safetensors"])
        );
        assert!(!args.contains(&"--seed".to_string()));
    }
}
