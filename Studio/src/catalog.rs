use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
pub const MODEL_ROOT: &str = "/home/kirpich/models";
pub const BUILTIN_MEDIA_ROOT: &str = "/home/kirpich/.local/opt/kaic-studio/models";
pub const PROFILES: &str = "/home/kirpich/models/context-tuning/profiles.json";
const GROUPS: [&str; 6] = [
    "niche-selection",
    "niche-selection/extra",
    "niche-selection/modern",
    "quality-selection",
    "extravagant-selection",
    "ibm-selection",
];
pub const GEMMA: &str = "unsloth/gemma-4-26B-A4B-it-GGUF";
pub const GLM: &str = "unsloth/GLM-4.7-Flash-GGUF";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Text,
    Image,
    Video,
}

/// Определяет назначение локальной модели (текст / фото / видео) по имени репозитория и файла.
pub fn classify_model(repo: &str, filename: &str) -> ModelKind {
    let hay = format!("{} {}", repo, filename).to_ascii_lowercase();
    let is_video = [
        "t2v",
        "i2v",
        "wan2",
        "wan-",
        "wan_",
        "hunyuanvideo",
        "hunyuan-video",
        "ltx-video",
        "ltx_video",
        "ltx2",
        "cogvideo",
        "mochi",
        "minimax-h3",
        "minimax_h3",
        "fl2va",
        "ref2va",
        "vid_gen",
        "text-to-video",
        "image-to-video",
    ]
    .iter()
    .any(|k| hay.contains(k));
    if is_video {
        return ModelKind::Video;
    }
    let is_image = [
        "flux",
        "sdxl",
        "sd3",
        "sd-3",
        "sd-1",
        "sd15",
        "sd-v1",
        "sd-v2",
        "stable-diffusion",
        "qwen-image",
        "qwen_image",
        "z-image",
        "z_image",
        "chroma",
        "hidream",
        "lumina",
        "kolors",
        "pixart",
        "sana",
        "omnigen",
        "juggernaut",
        "dreamshaper",
        "illustrious",
        "text-to-image",
        "txt2img",
    ]
    .iter()
    .any(|k| hay.contains(k));
    if is_image {
        return ModelKind::Image;
    }
    ModelKind::Text
}

/// Проверяет, является ли файл вспомогательным энкодером/VAE/проектором, а не основными весами модели.
pub fn is_auxiliary_encoder(filename: &str) -> bool {
    let low = filename.to_ascii_lowercase();
    low.contains("mmproj")
        || low.contains("vae")
        || low.starts_with("ae.")
        || low.contains("clip_")
        || low.contains("t5xxl")
        || low.contains("t5-v1")
        || low.contains("t5_")
        || low.contains("umt5")
        || low.contains("imatrix")
        || low.starts_with("qwen3vl_32b_minimax_h3")
}

/// Находит установленную полную модель генерации фото или видео (включая встроенный каталог `/home/kirpich/.local/opt/kaic-studio/models`).
pub fn find_working_media_model(models: &[Model], kind: ModelKind) -> Option<Model> {
    let mut pool: Vec<Model> = models.to_vec();
    for extra_root in [BUILTIN_MEDIA_ROOT, MODEL_ROOT] {
        for m in discover_media_models(Path::new(extra_root)) {
            if !pool.iter().any(|existing| existing.path == m.path) {
                pool.push(m);
            }
        }
    }
    let has_complete_bundle = |m: &Model| -> bool {
        let low = m.filename.to_ascii_lowercase();
        if low.contains("dreamshaper") || low.contains("sdxl") || low.contains("sd_turbo") {
            return true;
        }
        m.path
            .parent()
            .and_then(|d| fs::read_dir(d).ok())
            .is_some_and(|rd| {
                rd.flatten().any(|e| {
                    let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                    n.contains("vae") || n.starts_with("ae.")
                })
            })
    };
    pool.iter()
        .filter(|m| {
            m.kind() == kind
                && m.loadable()
                && !is_auxiliary_encoder(&m.filename)
                && has_complete_bundle(m)
        })
        .max_by_key(|m| {
            (
                m.path.starts_with(BUILTIN_MEDIA_ROOT),
                m.path.starts_with(MODEL_ROOT),
            )
        })
        .cloned()
        .or_else(|| {
            // Если полная видео-модель (с её VAE/энкодерами) не установлена, используем самодостаточную фото-модель
            // (sd_cpp сгенерирует ключевой кадр на CUDA и соберёт плавный H.264 MP4-ролик через ffmpeg).
            pool.into_iter()
                .filter(|m| {
                    m.loadable() && !is_auxiliary_encoder(&m.filename) && has_complete_bundle(m)
                })
                .max_by_key(|m| {
                    (
                        m.path.starts_with(BUILTIN_MEDIA_ROOT),
                        m.path.starts_with(MODEL_ROOT),
                    )
                })
        })
}

/// Сканирует каталог пользователя на предмет дополнительных GGUF/safetensors моделей фото и видео.
pub fn discover_media_models(dir: &Path) -> Vec<Model> {
    let mut found = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((cur, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&cur) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(meta) = fs::metadata(&p) else {
                continue;
            };
            if meta.is_dir() && depth < 2 {
                let name = entry.file_name();
                if !name.to_string_lossy().starts_with('.') {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            if !meta.is_file() || meta.len() < 1_000_000 {
                continue;
            }
            let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let ext_low = ext.to_ascii_lowercase();
            if ext_low != "gguf" && ext_low != "safetensors" {
                continue;
            }
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.contains(".part") || fname.contains(".range") {
                continue;
            }
            // Пропускаем вспомогательные VAE / CLIP / T5 / mmproj файлы как самостоятельные модели
            if is_auxiliary_encoder(&fname) {
                continue;
            }
            let parent_name = p
                .parent()
                .and_then(|d| d.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "local".into());
            let repo = format!(
                "{parent_name}/{}",
                fname.trim_end_matches(&format!(".{ext}"))
            );
            if classify_model(&repo, &fname) == ModelKind::Text {
                continue;
            }
            found.push(Model {
                repo,
                filename: fname,
                path: p,
                size: meta.len(),
                sha256: String::new(),
                status: "verified-local-media".into(),
                generation_tps: None,
                meets_10_tps: None,
                chat_exit: None,
            });
        }
    }
    found.sort_by(|a, b| a.filename.cmp(&b.filename));
    found
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub repo: String,
    pub filename: String,
    #[serde(default)]
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub status: String,
    #[serde(skip)]
    pub generation_tps: Option<f64>,
    #[serde(skip)]
    pub meets_10_tps: Option<bool>,
    #[serde(skip)]
    pub chat_exit: Option<i64>,
}
pub fn resolve_selection<'a>(models: &'a [Model], selection: &str) -> Option<&'a Model> {
    models
        .iter()
        .find(|m| m.selection_id() == selection)
        .or_else(|| models.iter().find(|m| m.repo == selection))
}
impl Model {
    pub fn selection_id(&self) -> String {
        format!("{}::{}", self.repo, self.path.display())
    }

    pub fn kind(&self) -> ModelKind {
        classify_model(&self.repo, &self.filename)
    }

    pub fn is_generative_media(&self) -> bool {
        matches!(self.kind(), ModelKind::Image | ModelKind::Video)
    }

    pub fn loadable(&self) -> bool {
        let valid_ext = self
            .path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| {
                x.eq_ignore_ascii_case("gguf")
                    || (self.is_generative_media() && x.eq_ignore_ascii_case("safetensors"))
            });
        self.status.starts_with("verified")
            && !self.filename.contains(".part")
            && !self.filename.contains(".range")
            && valid_ext
            && self
                .path
                .file_name()
                .is_some_and(|x| x == self.filename.as_str())
            && self
                .path
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() == self.size)
    }
    pub fn alias(&self) -> &str {
        match self.repo.as_str() {
            GEMMA => "gemma26",
            GLM => "glm30",
            _ => &self.filename,
        }
    }
    pub fn display_name(&self) -> String {
        let family = match self.repo.as_str() {
            GEMMA => "Gemma 4 · 26B A4B",
            GLM => "GLM 4.7 Flash · 30B",
            _ => self
                .repo
                .rsplit('/')
                .next()
                .unwrap_or(&self.repo)
                .trim_end_matches("-GGUF"),
        };
        let stem = self
            .filename
            .trim_end_matches(".gguf")
            .trim_end_matches(".safetensors");
        let quant = stem.rfind('Q').map(|i| &stem[i..]).unwrap_or("");
        if quant.is_empty() {
            family.to_string()
        } else {
            format!("{family} · {quant}")
        }
    }
    pub fn experimental_note(&self) -> Option<&'static str> {
        if self.repo.contains("granite-switch-4.1-30b") {
            Some(
                "Экспериментальная плотная модель 30B. ≥10 ток/с не подтверждено. Специализированные aLoRA/guardian/decision адаптеры не активируются загрузкой GGUF.",
            )
        } else {
            None
        }
    }
    pub fn measurement(&self) -> String {
        let speed = self
            .generation_tps
            .map(|x| format!("{x:.2} ток/с"))
            .unwrap_or_else(|| "скорость не измерена".into());
        let class = match (self.generation_tps, self.meets_10_tps) {
            (Some(x), Some(true)) if x >= 10.0 => "≥10 подтверждено",
            (Some(x), _) if x < 10.0 => "экспериментальная / медленная",
            _ => "порог скорости не подтверждён",
        };
        format!(
            "{speed} · {class} · чат: {}",
            match self.chat_exit {
                Some(0) => "успешный сохранённый тест".into(),
                Some(x) => format!("код {x}"),
                None => "нет сохранённого теста".into(),
            }
        )
    }
}
pub fn benchmark_matches(m: &Model, b: &Value) -> bool {
    b["repo"].as_str() == Some(m.repo.as_str())
        && b["path"].as_str() == m.path.to_str()
        && b["sha256"].as_str() == Some(m.sha256.as_str())
}
pub fn discover(root: &Path) -> (Vec<Model>, Vec<String>) {
    let mut models = vec![];
    let mut warnings = vec![];
    let mut benchmarks = vec![];
    for group in GROUPS {
        let dir = root.join(group);
        match fs::read(dir.join("progress.json"))
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<Vec<Model>>(&b)?))
        {
            Ok(m) => models.extend(m),
            Err(_) => warnings.push(format!(
                "Не удалось прочитать каталог {group}/progress.json"
            )),
        }
        if let Ok(bytes) = fs::read(dir.join("benchmark-results.json")) {
            match serde_json::from_slice::<Vec<Value>>(&bytes) {
                Ok(v) => benchmarks.extend(v),
                Err(_) => warnings.push(format!("Некорректные измерения: {group}")),
            }
        }
    }
    for m in &mut models {
        if let Some(b) = benchmarks.iter().find(|b| benchmark_matches(m, b)) {
            m.generation_tps = b["generation_tps"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0);
            m.meets_10_tps = b["meets_10_tps"].as_bool();
            m.chat_exit = b["chat_smoke"]["exit"].as_i64();
        }
    }
    (models, warnings)
}
pub fn default_model(models: &[Model]) -> Option<&Model> {
    [GEMMA, GLM]
        .iter()
        .find_map(|r| models.iter().find(|m| m.repo == *r && m.loadable()))
        .or_else(|| models.iter().find(|m| m.loadable()))
}

/// Безопасно удаляет файл модели (.gguf / .safetensors) с диска и очищает запись в progress.json.
pub fn delete_model_from_disk(root: &Path, model: &Model) -> Result<()> {
    let ext_ok = model
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf") || e.eq_ignore_ascii_case("safetensors"));
    if !ext_ok {
        bail!("Разрешено удалять только файлы моделей .gguf и .safetensors");
    }
    if model.path.is_file() {
        fs::remove_file(&model.path)
            .with_context(|| format!("Не удалось удалить файл {}", model.path.display()))?;
    }
    if let Some(parent) = model.path.parent()
        && parent != root
    {
        let is_group_dir = GROUPS.iter().any(|g| root.join(g) == parent);
        if !is_group_dir && model.is_generative_media() && discover_media_models(parent).is_empty()
        {
            if let Ok(rd) = fs::read_dir(parent) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_file()
                        && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                            e.eq_ignore_ascii_case("gguf")
                                || e.eq_ignore_ascii_case("safetensors")
                                || e.eq_ignore_ascii_case("part")
                        })
                    {
                        let _ = fs::remove_file(p);
                    }
                }
            }
        }
        let _ = fs::remove_dir(parent);
    }
    for group in GROUPS {
        let manifest = root.join(group).join("progress.json");
        let Ok(bytes) = fs::read(&manifest) else {
            continue;
        };
        let Ok(mut list) = serde_json::from_slice::<Vec<Model>>(&bytes) else {
            continue;
        };
        let before = list.len();
        list.retain(|m| {
            m.path != model.path && !(m.repo == model.repo && m.filename == model.filename)
        });
        if list.len() != before {
            let _ = crate::storage::save_json(&manifest, &list);
        }
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub context: u32,
    pub cache: String,
    pub flags: Vec<String>,
    pub threads: u32,
    pub generation_tps: Option<f64>,
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            context: 4096,
            cache: "q8_0".into(),
            flags: vec![],
            threads: 8,
            generation_tps: None,
        }
    }
}
pub fn profile_from_value(v: &Value, repo: &str) -> Result<Profile> {
    let p = match v.get(repo) {
        Some(p) => serde_json::from_value::<Profile>(p.clone()).context("Некорректный профиль")?,
        None => Profile::default(),
    };
    if p.context < 512
        || p.context > 2_097_152
        || !(1..=256).contains(&p.threads)
        || !["q8_0", "f16", "bf16"].contains(&p.cache.as_str())
    {
        bail!("Небезопасные параметры профиля контекста");
    }
    Ok(p)
}
pub fn live_profile(repo: &str) -> Result<Profile> {
    match fs::read(PROFILES) {
        Ok(b) => profile_from_value(&serde_json::from_slice::<Value>(&b)?, repo),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Profile::default()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_model_removes_file_and_updates_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let group_dir = dir.path().join("niche-selection");
        fs::create_dir_all(&group_dir).unwrap();
        let model_file = group_dir.join("test-model.gguf");
        fs::write(&model_file, b"GGUFdata").unwrap();
        let m = Model {
            repo: "owner/test-model-GGUF".into(),
            filename: "test-model.gguf".into(),
            path: model_file.clone(),
            size: 8,
            sha256: "abc".into(),
            status: "verified".into(),
            generation_tps: None,
            meets_10_tps: None,
            chat_exit: None,
        };
        crate::storage::save_json(&group_dir.join("progress.json"), &vec![m.clone()]).unwrap();
        delete_model_from_disk(dir.path(), &m).unwrap();
        assert!(!model_file.exists());
        let remaining: Vec<Model> =
            crate::storage::load_json(&group_dir.join("progress.json")).unwrap();
        assert!(remaining.is_empty());
    }
}
