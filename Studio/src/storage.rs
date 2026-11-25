use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GeneratedMedia {
    pub kind: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_path: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<GeneratedMedia>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_events: Vec<crate::agent::AgentEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<crate::agent::SubtaskRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
}
impl Message {
    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content,
            reasoning: String::new(),
            state: String::new(),
            error: None,
            tps: None,
            tokens: None,
            duration_secs: None,
            media: Vec::new(),
            agent_events: Vec::new(),
            subtasks: Vec::new(),
            changed_files: Vec::new(),
        }
    }
    pub fn assistant() -> Self {
        Self {
            role: "assistant".into(),
            content: String::new(),
            reasoning: String::new(),
            state: "ожидание".into(),
            error: None,
            tps: None,
            tokens: None,
            duration_secs: None,
            media: Vec::new(),
            agent_events: Vec::new(),
            subtasks: Vec::new(),
            changed_files: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaGenOptions {
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f32,
    pub seed: i64,
    pub video_frames: u32,
    pub fps: u32,
    pub negative_prompt: String,
    pub vae_path: String,
    pub text_encoder_path: String,
    pub offload_to_cpu: bool,
    pub vae_tiling: bool,
}
impl Default for MediaGenOptions {
    fn default() -> Self {
        Self {
            width: 512,
            height: 512,
            steps: 20,
            cfg_scale: 5.0,
            seed: -1,
            video_frames: 17,
            fps: 16,
            negative_prompt: String::new(),
            vae_path: String::new(),
            text_encoder_path: String::new(),
            offload_to_cpu: true,
            vae_tiling: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatOptions {
    pub context: u32,
    pub system_prompt: String,
    pub skills: String,
    pub media_gen: MediaGenOptions,
    pub agent_enabled: bool,
    pub workspace_path: String,
    pub agent_policy: crate::agent::AgentPolicy,
    pub exec_policy: crate::agent::ExecPolicy,
    pub provider: crate::agent::ProviderKind,
    #[serde(default)]
    pub provider_endpoint: String,
    #[serde(default)]
    pub provider_model: String,
    pub external_provider_consent: bool,
    /// Динамическое взаимодействие (мульти-курсоры и виртуальные клавиатуры) выключено по умолчанию.
    #[serde(default)]
    pub dynamic_interaction_enabled: bool,
}
impl ChatOptions {
    pub fn messages(&self, mut messages: Vec<Message>) -> Result<Vec<Message>> {
        if (self.context != 0 && !(512..=2_097_152).contains(&self.context))
            || self.system_prompt.len() > 65536
            || self.skills.len() > 65536
        {
            bail!(
                "Контекст: 0 (профиль) или 512–2097152; системный промпт и skills — до 64 КиБ каждый"
            );
        }
        let mut system = self.system_prompt.trim().to_owned();
        if !self.skills.trim().is_empty() {
            system.push_str("\n\nSKILLS — инструкции для этого диалога:\n");
            system.push_str(self.skills.trim());
        }
        if !system.is_empty() {
            let mut m = Message::user(system);
            m.role = "system".into();
            messages.insert(0, m);
        }
        Ok(messages)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    #[serde(default)]
    pub options: ChatOptions,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub updated_at: Option<u64>,
    pub id: Uuid,
    pub title: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub kaic_task: Option<String>,
    #[serde(default)]
    pub kaic_base: usize,
    #[serde(default)]
    pub kaic_endpoint: Option<String>,
}
pub fn epoch_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|v| v.as_secs())
}
impl Chat {
    pub fn new() -> Self {
        Self {
            options: ChatOptions::default(),
            created_at: epoch_now(),
            updated_at: epoch_now(),
            id: Uuid::new_v4(),
            title: "Новый разговор".into(),
            messages: vec![],
            status: String::new(),
            model: String::new(),
            kaic_task: None,
            kaic_base: 0,
            kaic_endpoint: None,
        }
    }
}
impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}
pub fn default_sd_cli_path() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/opt/kaic-studio/bin/sd-cli");
        if local.is_file() {
            return local.to_string_lossy().into_owned();
        }
        return local.to_string_lossy().into_owned();
    }
    "sd-cli".into()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub kaic_url: String,
    pub comfy_url: String,
    pub use_kaic: bool,
    pub selected_repo: String,
    pub context_override: u32,
    pub max_tokens: u32,
    pub language: String,
    pub sd_cli_path: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            kaic_url: "http://127.0.0.1:4545".into(),
            comfy_url: "http://127.0.0.1:8188".into(),
            use_kaic: false,
            selected_repo: crate::catalog::GEMMA.into(),
            context_override: 0,
            max_tokens: 1024,
            language: "en".into(),
            sd_cli_path: default_sd_cli_path(),
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    pub next_chat_options: ChatOptions,
    pub chats: Vec<Chat>,
    pub settings: Settings,
}
pub fn resolve_data_dir(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.is_absolute() {
            bail!(
                "KAIC_STUDIO_DATA_DIR должен быть абсолютным путём; автоматического перехода к другим данным нет"
            );
        }
        return Ok(p);
    }
    if let Some(p) = xdg {
        if !p.is_absolute() {
            bail!("XDG_DATA_HOME должен быть абсолютным путём; резервный каталог не используется");
        }
        return Ok(p.join("kaic-studio"));
    }
    let home = home
        .filter(|p| p.is_absolute())
        .context("Не найден абсолютный каталог пользователя")?;
    Ok(home.join(".local/share/kaic-studio"))
}
pub fn data_dir() -> Result<PathBuf> {
    resolve_data_dir(
        std::env::var_os("KAIC_STUDIO_DATA_DIR").map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let temp = parent.join(format!(".save-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > 32 * 1024 * 1024 {
        bail!("Данные превышают 32 МиБ; предыдущий файл сохранён");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    if path.metadata()?.len() > 32 * 1024 * 1024 {
        bail!("Файл данных превышает 32 МиБ");
    }
    serde_json::from_slice(&fs::read(path)?)
        .context("Некорректный JSON: исходный файл сохранён без изменений")
}
