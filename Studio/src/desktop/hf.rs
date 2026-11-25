//! Hugging Face public GGUF: native Rust HTTP, pinned revisions, resumable private parts.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        bail!("Укажите публичный репозиторий owner/name, не URL");
    }
    Ok(())
}
pub fn validate_file(file: &str) -> Result<()> {
    if !file.ends_with(".gguf")
        || file.len() > 512
        || file.split('/').any(|s| {
            s.is_empty()
                || s == "."
                || s == ".."
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        bail!("Недопустимый путь GGUF");
    }
    // Split GGUF needs all shards; never register one shard as a complete model.
    if file.contains("-of-") {
        bail!("Разделённый GGUF не поддерживается: выберите цельный файл");
    }
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteFile {
    pub filename: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub revision: String,
}
pub fn parse_files(v: &Value) -> Result<Vec<RemoteFile>> {
    let revision = v["sha"].as_str().context("HF не вернул revision")?;
    if revision.len() != 40 || !revision.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("Некорректный revision HF");
    }
    let mut out = vec![];
    for f in v["siblings"].as_array().context("HF не вернул файлы")? {
        let Some(name) = f["rfilename"].as_str() else {
            continue;
        };
        if validate_file(name).is_err() || crate::catalog::is_auxiliary_encoder(name) {
            continue;
        }
        let Some(size) = f["size"]
            .as_u64()
            .or(f["lfs"]["size"].as_u64())
            .filter(|x| *x >= 8)
        else {
            continue;
        };
        let hash = f["lfs"]["sha256"].as_str().map(str::to_owned);
        if hash
            .as_ref()
            .is_some_and(|h| h.len() != 64 || !h.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            bail!("Некорректный SHA256 HF");
        }
        out.push(RemoteFile {
            filename: name.into(),
            size,
            sha256: hash,
            revision: revision.into(),
        });
    }
    Ok(out)
}
fn trusted(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url.host_str().is_some_and(|h| {
            h == "huggingface.co"
                || h.ends_with(".huggingface.co")
                || h == "hf.co"
                || h.ends_with(".hf.co")
                || h.ends_with(".cloudfront.net")
                || h.ends_with(".amazonaws.com")
        })
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() < 8 && trusted(a.url()) {
                a.follow()
            } else {
                a.stop()
            }
        }))
        .user_agent("KAIC-Studio/0.1")
        .build()?)
}
fn read_hf_token(data: &Path) -> Option<String> {
    for var in ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    let mut paths = vec![data.join("hf-token")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".cache/huggingface/token"));
    }
    for p in paths {
        if let Ok(raw) = fs::read_to_string(p) {
            let t = raw.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn save_hf_token(data: &Path, token: &str) -> Result<()> {
    let path = data.join("hf-token");
    let trimmed = token.trim();
    if trimmed.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    if trimmed.len() > 512 || trimmed.chars().any(|c| c.is_control()) {
        bail!("Некорректный формат HF_TOKEN");
    }
    fs::create_dir_all(data)?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(trimmed.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

fn apply_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(t) = token {
        req.bearer_auth(t)
    } else {
        req
    }
}

fn retry_after_secs(r: &reqwest::Response) -> Option<u64> {
    r.headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn check_response(r: &reqwest::Response) -> Result<()> {
    let s = r.status();
    if s.is_success() {
        return Ok(());
    }
    if s == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let wait = retry_after_secs(r);
        match wait {
            Some(sec) => bail!(
                "Лимит запросов Hugging Face для текущего IP (HTTP 429). Подождите {sec} с или укажите бесплатный HF_TOKEN (кнопка «Токен»)."
            ),
            None => bail!(
                "Лимит запросов Hugging Face для текущего IP (HTTP 429). Повторите чуть позже или укажите бесплатный HF_TOKEN (кнопка «Токен»)."
            ),
        }
    }
    if matches!(
        s,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        bail!(
            "Hugging Face HTTP {}. Для gated/private моделей нужен HF_TOKEN с принятой лицензией.",
            s.as_u16()
        );
    }
    bail!("Hugging Face HTTP {}", s.as_u16());
}

async fn metadata(repo: &str, token: Option<&str>) -> Result<Vec<RemoteFile>> {
    validate_repo(repo)?;
    let http = client()?;
    let mut r = apply_auth(
        http.get(format!("https://huggingface.co/api/models/{repo}"))
            .query(&[("blobs", "true")]),
        token,
    )
    .send()
    .await
    .map_err(|_| anyhow::anyhow!("Hugging Face: ошибка сети"))?;
    if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        && let Some(sec) = retry_after_secs(&r)
        && sec <= 5
    {
        tokio::time::sleep(Duration::from_secs(sec.max(1))).await;
        r = apply_auth(
            http.get(format!("https://huggingface.co/api/models/{repo}"))
                .query(&[("blobs", "true")]),
            token,
        )
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Hugging Face: ошибка сети"))?;
    }
    check_response(&r)?;
    parse_files(
        &r.json::<Value>()
            .await
            .context("Некорректные метаданные HF")?,
    )
}
pub fn resume_offset(status: u16, range: Option<&str>, offset: u64, total: u64) -> Result<u64> {
    if status == 200 {
        return Ok(0);
    }
    let expected = format!("bytes {offset}-{}/{}", total.saturating_sub(1), total);
    if status == 206 && offset > 0 && range == Some(expected.as_str()) {
        return Ok(offset);
    }
    bail!("Сервер вернул неожиданный статус или Content-Range; .part сохранён")
}
pub fn validate_payload(path: &Path, size: u64, hash: Option<&str>) -> Result<String> {
    validate_payload_cancellable(path, size, hash, &AtomicBool::new(false))
}
pub fn validate_payload_cancellable(
    path: &Path,
    size: u64,
    hash: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String> {
    if cancel.load(Ordering::SeqCst) {
        bail!("Проверка остановлена; файл не зарегистрирован");
    }
    let mut f = fs::File::open(path)?;
    if f.metadata()?.len() != size {
        bail!("Размер GGUF не совпадает с HF; файл не зарегистрирован");
    }
    let mut magic = [0; 4];
    f.read_exact(&mut magic)?;
    if magic != *b"GGUF" {
        bail!("Файл не является GGUF");
    }
    let mut digest = Sha256::new();
    digest.update(magic);
    let mut buf = [0; 262144];
    loop {
        if cancel.load(Ordering::SeqCst) {
            bail!("Проверка остановлена; файл не зарегистрирован");
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    let actual = format!("{:x}", digest.finalize());
    if hash.is_some_and(|h| !h.eq_ignore_ascii_case(&actual)) {
        bail!("SHA256 не совпадает с HF; файл не зарегистрирован");
    }
    Ok(actual)
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Download {
    #[serde(default)]
    pub resumed_from: u64,
    #[serde(default)]
    pub response_status: Option<u16>,
    #[serde(default)]
    pub speed_bps: u64,
    #[serde(default)]
    pub eta_secs: Option<u64>,
    pub id: String,
    pub repo: String,
    pub file: RemoteFile,
    pub directory: PathBuf,
    pub downloaded: u64,
    pub state: String,
    pub error: Option<String>,
    pub path: Option<PathBuf>,
    pub actual_sha256: Option<String>,
}
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}
const CACHE_TTL: Duration = Duration::from_secs(600);

struct Inner {
    error: Option<String>,
    data: PathBuf,
    jobs: Mutex<Vec<Download>>,
    search_cache: Mutex<HashMap<String, (Instant, Value)>>,
    files_cache: Mutex<HashMap<String, (Instant, Vec<RemoteFile>)>>,
    active: AtomicBool,
    cancel: AtomicBool,
}
impl Manager {
    pub fn open(data: PathBuf) -> Self {
        let path = data.join("hf-jobs.json");
        let (mut jobs, error): (Vec<Download>, Option<String>) = if path.exists() {
            match crate::storage::load_json(&path){Ok(v)=>(v,None),Err(_)=>(vec![],Some("Реестр загрузок повреждён; исходный файл сохранён. Восстановите hf-jobs.json из резервной копии.".into()))}
        } else {
            (vec![], None)
        };
        for j in &mut jobs {
            if ["downloading", "validating", "connecting"].contains(&j.state.as_str()) {
                j.state = "paused".into();
            }
            j.speed_bps = 0;
            j.eta_secs = None;
        }
        Self {
            inner: Arc::new(Inner {
                error,
                data,
                jobs: Mutex::new(jobs),
                search_cache: Mutex::new(HashMap::new()),
                files_cache: Mutex::new(HashMap::new()),
                active: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
            }),
        }
    }
    pub fn snapshot(&self) -> Value {
        let default_dir = std::env::var_os("KAIC_STUDIO_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.inner.data.join("models"));
        let free_bytes = fs2::available_space(&default_dir)
            .or_else(|_| fs2::available_space(&self.inner.data))
            .ok();
        json!({
            "error": self.inner.error,
            "downloads": *self.inner.jobs.lock().unwrap(),
            "default_directory": default_dir,
            "free_bytes": free_bytes,
            "active": self.inner.active.load(Ordering::SeqCst),
            "token_present": read_hf_token(&self.inner.data).is_some(),
        })
    }
    fn update(&self, id: &str, f: impl FnOnce(&mut Download)) -> Result<()> {
        let mut jobs = self.inner.jobs.lock().unwrap();
        f(jobs
            .iter_mut()
            .find(|j| j.id == id)
            .context("Загрузка не найдена")?);
        crate::storage::save_json(&self.inner.data.join("hf-jobs.json"), &*jobs)
    }
    pub fn cancel(&self, id: &str) -> Result<Value> {
        let jobs = self.inner.jobs.lock().unwrap();
        if !jobs.iter().any(|j| {
            j.id == id && ["connecting", "downloading", "validating"].contains(&j.state.as_str())
        }) {
            bail!("Нет активной загрузки");
        }
        self.inner.cancel.store(true, Ordering::SeqCst);
        Ok(json!({"cancelling":true}))
    }

    /// Запускает скачивание в выделенном фоновом потоке с собственным Tokio-рантаймом,
    /// чтобы завершение короткоживущего рантайма UI-запроса не обрывало передачу на 0%.
    fn spawn_transfer(&self, task_id: String) {
        let m = self.clone();
        std::thread::spawn(move || {
            let outcome: Result<()> = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("Не удалось создать рантайм загрузки HF")
                .and_then(|rt| rt.block_on(m.transfer(&task_id)));
            if let Err(e) = outcome {
                let cancelled = m.inner.cancel.load(Ordering::SeqCst);
                let _ = m.update(&task_id, |j| {
                    j.state = if cancelled { "paused" } else { "error" }.into();
                    j.speed_bps = 0;
                    j.eta_secs = None;
                    j.error = Some(format!("{e:#}"));
                });
            }
            m.inner.active.store(false, Ordering::SeqCst);
        });
    }

    /// Удаляет запись о загрузке (и её `.part` / готовый файл) по `id`.
    pub fn remove_job(&self, id: &str, delete_ready_file: bool) -> Result<()> {
        let is_active_job = {
            let jobs = self.inner.jobs.lock().unwrap();
            jobs.iter().any(|j| {
                j.id == id
                    && ["connecting", "downloading", "validating"].contains(&j.state.as_str())
            })
        };
        if is_active_job && self.inner.active.load(Ordering::SeqCst) {
            self.inner.cancel.store(true, Ordering::SeqCst);
            bail!("Остановите активную загрузку перед удалением");
        }
        let mut jobs = self.inner.jobs.lock().unwrap();
        let Some(pos) = jobs.iter().position(|j| j.id == id) else {
            return Ok(());
        };
        let removed = jobs.remove(pos);
        crate::storage::save_json(&self.inner.data.join("hf-jobs.json"), &*jobs)?;
        let folder = removed.directory.join(&removed.id);
        let base_name = removed
            .file
            .filename
            .rsplit('/')
            .next()
            .unwrap_or(&removed.file.filename);
        let final_path = folder.join(base_name);
        let part_path = final_path.with_extension("gguf.part");
        let _ = fs::remove_file(&part_path);
        if delete_ready_file {
            if let Some(p) = &removed.path {
                let _ = fs::remove_file(p);
            }
            let _ = fs::remove_file(&final_path);
        }
        let _ = fs::remove_dir(&folder);
        Ok(())
    }

    /// Удаляет все записи в `hf-jobs.json`, ссылающиеся на указанный путь файла модели.
    pub fn remove_by_path(&self, target: &Path) -> Result<()> {
        let mut jobs = self.inner.jobs.lock().unwrap();
        let before = jobs.len();
        let mut folders_to_clean = Vec::new();
        jobs.retain(|j| {
            let folder = j.directory.join(&j.id);
            let base = j
                .file
                .filename
                .rsplit('/')
                .next()
                .unwrap_or(&j.file.filename);
            let candidate = folder.join(base);
            let matches = j.path.as_deref() == Some(target) || candidate == target;
            if matches {
                folders_to_clean.push((folder, candidate.with_extension("gguf.part")));
            }
            !matches
        });
        if jobs.len() != before {
            crate::storage::save_json(&self.inner.data.join("hf-jobs.json"), &*jobs)?;
        }
        for (folder, part) in folders_to_clean {
            let _ = fs::remove_file(part);
            let _ = fs::remove_dir(folder);
        }
        Ok(())
    }
    pub async fn api(&self, path: &str, body: &[u8]) -> Result<Value> {
        if let Some(e) = &self.inner.error
            && !["/api/hf/search", "/api/hf/files", "/api/hf/token"].contains(&path)
        {
            bail!("{e}");
        }
        let v: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(body)?
        };
        let token = read_hf_token(&self.inner.data);
        match path {
            "/api/hf/token" => {
                save_hf_token(&self.inner.data, v["token"].as_str().unwrap_or(""))?;
                self.inner.search_cache.lock().unwrap().clear();
                self.inner.files_cache.lock().unwrap().clear();
                Ok(json!({ "token_present": read_hf_token(&self.inner.data).is_some() }))
            }
            "/api/hf/search" => {
                let q = v["query"].as_str().unwrap_or("").trim();
                if q.len() > 200 {
                    bail!("Введите запрос до 200 байт");
                }
                let sort = match v["sort"].as_str().unwrap_or("downloads") {
                    "likes" => "likes",
                    "lastModified" => "lastModified",
                    "trendingScore" => "trendingScore",
                    _ => "downloads",
                };
                let all_types = v["all_types"].as_bool().unwrap_or(true);
                let cache_key = format!("{q}\n{sort}\n{all_types}");
                if let Some((at, cached)) = self.inner.search_cache.lock().unwrap().get(&cache_key)
                    && at.elapsed() < CACHE_TTL
                {
                    return Ok(cached.clone());
                }
                let mut params: Vec<(&str, &str)> = vec![
                    ("filter", "gguf"),
                    ("sort", sort),
                    ("direction", "-1"),
                    ("limit", "60"),
                ];
                if !all_types {
                    params.push(("pipeline_tag", "text-generation"));
                }
                if !q.is_empty() {
                    params.push(("search", q));
                }
                let http = client()?;
                let mut r = apply_auth(
                    http.get("https://huggingface.co/api/models").query(&params),
                    token.as_deref(),
                )
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Hugging Face недоступен"))?;
                if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    && let Some(sec) = retry_after_secs(&r)
                    && sec <= 5
                {
                    tokio::time::sleep(Duration::from_secs(sec.max(1))).await;
                    r = apply_auth(
                        http.get("https://huggingface.co/api/models").query(&params),
                        token.as_deref(),
                    )
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Hugging Face недоступен"))?;
                }
                check_response(&r)?;
                let raw = r.json::<Vec<Value>>().await?;
                let mut repos = Vec::new();
                let mut items = Vec::new();
                for entry in raw {
                    let Some(id) = entry["id"].as_str().filter(|s| validate_repo(s).is_ok()) else {
                        continue;
                    };
                    repos.push(id.to_owned());
                    let tags: Vec<&str> = entry["tags"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| t.as_str())
                                .filter(|t| {
                                    !t.starts_with("region:")
                                        && !t.starts_with("base_model:")
                                        && !t.starts_with("dataset:")
                                        && !t.starts_with("arxiv:")
                                        && *t != "endpoints_compatible"
                                        && *t != "conversational"
                                })
                                .take(6)
                                .collect()
                        })
                        .unwrap_or_default();
                    items.push(json!({
                        "id": id,
                        "downloads": entry["downloads"].as_u64().unwrap_or(0),
                        "likes": entry["likes"].as_u64().unwrap_or(0),
                        "last_modified": entry["lastModified"].as_str().unwrap_or(""),
                        "pipeline_tag": entry["pipeline_tag"].as_str().unwrap_or(""),
                        "gated": entry["gated"].as_bool().unwrap_or(false) || entry["gated"].is_string(),
                        "tags": tags,
                    }));
                }
                let out = json!({"repos": repos, "items": items});
                self.inner
                    .search_cache
                    .lock()
                    .unwrap()
                    .insert(cache_key, (Instant::now(), out.clone()));
                Ok(out)
            }
            "/api/hf/files" => {
                let repo = v["repo"].as_str().context("Укажите репозиторий")?;
                if let Some((at, cached)) = self.inner.files_cache.lock().unwrap().get(repo)
                    && at.elapsed() < CACHE_TTL
                {
                    return Ok(
                        json!({"files": cached, "note": "Разделённые GGUF исключены: нужен цельный файл."}),
                    );
                }
                let files = metadata(repo, token.as_deref()).await?;
                self.inner
                    .files_cache
                    .lock()
                    .unwrap()
                    .insert(repo.to_string(), (Instant::now(), files.clone()));
                Ok(
                    json!({"files": files, "note": "Разделённые GGUF исключены: нужен цельный файл."}),
                )
            }
            "/api/hf/start" => {
                let repo = v["repo"]
                    .as_str()
                    .context("Укажите репозиторий")?
                    .to_owned();
                let filename = v["filename"].as_str().context("Укажите файл")?;
                validate_file(filename)?;
                let cached_file =
                    self.inner
                        .files_cache
                        .lock()
                        .unwrap()
                        .get(&repo)
                        .and_then(|(at, list)| {
                            (at.elapsed() < CACHE_TTL)
                                .then(|| list.iter().find(|f| f.filename == filename).cloned())
                                .flatten()
                        });
                let file = match cached_file {
                    Some(f) => f,
                    None => {
                        let list = metadata(&repo, token.as_deref()).await?;
                        self.inner
                            .files_cache
                            .lock()
                            .unwrap()
                            .insert(repo.clone(), (Instant::now(), list.clone()));
                        list.into_iter()
                            .find(|f| f.filename == filename)
                            .context("Цельный GGUF не найден")?
                    }
                };
                let dir = PathBuf::from(
                    v["directory"]
                        .as_str()
                        .context("Укажите абсолютный каталог")?,
                );
                if !dir.is_absolute() {
                    bail!("Каталог должен быть абсолютным");
                }
                fs::create_dir_all(&dir)?;
                let dir = dir.canonicalize()?;
                let id = format!(
                    "{:x}",
                    Sha256::digest(format!(
                        "{repo}\n{}\n{filename}\n{}",
                        file.revision,
                        dir.display()
                    ))
                );
                if self
                    .inner
                    .active
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    bail!("Одновременно разрешена одна загрузка");
                }
                self.inner.cancel.store(false, Ordering::SeqCst);
                let result = (|| -> Result<()> {
                    let mut jobs = self.inner.jobs.lock().unwrap();
                    if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
                        if j.state == "ready" {
                            bail!("Файл уже готов");
                        }
                        j.state = "connecting".into();
                        j.speed_bps = 0;
                        j.eta_secs = None;
                        j.error = None;
                    } else {
                        jobs.push(Download {
                            resumed_from: 0,
                            response_status: None,
                            speed_bps: 0,
                            eta_secs: None,
                            id: id.clone(),
                            repo,
                            file,
                            directory: dir,
                            downloaded: 0,
                            state: "connecting".into(),
                            error: None,
                            path: None,
                            actual_sha256: None,
                        });
                    }
                    crate::storage::save_json(&self.inner.data.join("hf-jobs.json"), &*jobs)
                })();
                if let Err(e) = result {
                    self.inner.active.store(false, Ordering::SeqCst);
                    return Err(e);
                }
                self.spawn_transfer(id.clone());
                Ok(json!({"id":id}))
            }
            "/api/hf/resume" => {
                let id = v["id"].as_str().context("Нет id")?.to_owned();
                {
                    let jobs = self.inner.jobs.lock().unwrap();
                    let j = jobs
                        .iter()
                        .find(|j| j.id == id)
                        .context("Загрузка не найдена")?;
                    if j.state == "ready" {
                        bail!("Файл уже готов");
                    }
                    validate_repo(&j.repo)?;
                    validate_file(&j.file.filename)?;
                }
                if self
                    .inner
                    .active
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    bail!("Одновременно разрешена одна загрузка");
                }
                self.inner.cancel.store(false, Ordering::SeqCst);
                if let Err(e) = self.update(&id, |j| {
                    j.state = "connecting".into();
                    j.speed_bps = 0;
                    j.eta_secs = None;
                    j.error = None;
                }) {
                    self.inner.active.store(false, Ordering::SeqCst);
                    return Err(e);
                }
                self.spawn_transfer(id.clone());
                Ok(json!({"id":id}))
            }
            "/api/hf/remove" => {
                let id = v["id"].as_str().context("Нет id")?;
                let delete_file = v["delete_file"].as_bool().unwrap_or(false);
                self.remove_job(id, delete_file)?;
                Ok(json!({"removed": id}))
            }
            "/api/hf/cancel" => self.cancel(v["id"].as_str().context("Нет id")?),
            _ => bail!("Неизвестный HF маршрут"),
        }
    }
    async fn transfer(&self, id: &str) -> Result<()> {
        let j = self
            .inner
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .context("Нет загрузки")?;
        let folder = j.directory.join(id);
        fs::create_dir_all(&folder)?;
        if fs::symlink_metadata(&folder)?.file_type().is_symlink() {
            bail!("Символическая ссылка не разрешена");
        }
        let final_path = folder.join(j.file.filename.rsplit('/').next().unwrap());
        let part = final_path.with_extension("gguf.part");
        if fs::symlink_metadata(&final_path).is_ok() {
            bail!("Конечный файл уже существует; он не будет перезаписан");
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let mut output = opts.open(&part)?;
        output
            .try_lock_exclusive()
            .context("Этот файл уже загружается другим окном")?;
        let mut offset = output.metadata()?.len();
        if offset > j.file.size {
            bail!("Повреждённый .part: размер больше ожидаемого; выберите другой каталог");
        }
        if fs2::available_space(&folder)?
            < j.file
                .size
                .saturating_sub(offset)
                .saturating_add(512 * 1024 * 1024)
        {
            bail!("Недостаточно места: требуется размер файла плюс резерв 512 МиБ");
        }
        if offset < j.file.size {
            let url = format!(
                "https://huggingface.co/{}/resolve/{}/{}",
                j.repo, j.file.revision, j.file.filename
            );
            let token = read_hf_token(&self.inner.data);
            let mut request = apply_auth(client()?.get(url), token.as_deref());
            if offset > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            let send = request.send();
            tokio::pin!(send);
            let r = loop {
                tokio::select! {r=&mut send=>break r.map_err(|_|anyhow::anyhow!("Сеть HF: соединение прервано, .part сохранён"))?,_=tokio::time::sleep(Duration::from_millis(100))=>{if self.inner.cancel.load(Ordering::SeqCst){bail!("Подключение остановлено; .part сохранён");}}}
            };
            check_response(&r)?;
            let next_offset = resume_offset(
                r.status().as_u16(),
                r.headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|h| h.to_str().ok()),
                offset,
                j.file.size,
            )?;
            if next_offset == 0 && offset > 0 {
                output.set_len(0)?;
            }
            offset = next_offset;
            self.update(id, |j| {
                j.downloaded = offset;
                j.resumed_from = offset;
                j.response_status = Some(r.status().as_u16());
                j.speed_bps = 0;
                j.eta_secs = None;
                j.state = "downloading".into();
            })?;
            let mut stream = r.bytes_stream();
            let mut saved = std::time::Instant::now();
            let mut last_offset = offset;
            let mut smoothed_bps: f64 = 0.0;
            loop {
                if self.inner.cancel.load(Ordering::SeqCst) {
                    output.sync_all()?;
                    self.update(id, |j| {
                        j.downloaded = offset;
                        j.speed_bps = 0;
                        j.eta_secs = None;
                    })?;
                    bail!("Загрузка приостановлена; .part сохранён для продолжения");
                }
                let chunk = tokio::select! {v=stream.next()=>v,_=tokio::time::sleep(Duration::from_millis(150))=>{continue;}};
                let Some(chunk) = chunk else { break };
                let chunk =
                    chunk.map_err(|_| anyhow::anyhow!("Передача прервана; .part сохранён"))?;
                if offset.saturating_add(chunk.len() as u64) > j.file.size {
                    bail!("Ответ больше ожидаемого файла");
                }
                if fs2::available_space(&folder)? < chunk.len() as u64 + 512 * 1024 * 1024 {
                    bail!("Достигнут резерв свободного места 512 МиБ");
                }
                output.write_all(&chunk)?;
                offset += chunk.len() as u64;
                let dt = saved.elapsed();
                if dt > Duration::from_millis(200) {
                    let delta = offset.saturating_sub(last_offset) as f64;
                    let inst_bps = delta / dt.as_secs_f64().max(0.05);
                    smoothed_bps = if smoothed_bps <= 1.0 {
                        inst_bps
                    } else {
                        smoothed_bps * 0.65 + inst_bps * 0.35
                    };
                    let bps = smoothed_bps.round().max(0.0) as u64;
                    let rem = j.file.size.saturating_sub(offset);
                    let eta = if bps > 1024 { Some(rem / bps) } else { None };
                    self.update(id, |j| {
                        j.downloaded = offset;
                        j.speed_bps = bps;
                        j.eta_secs = eta;
                    })?;
                    last_offset = offset;
                    saved = std::time::Instant::now();
                }
            }
        }
        output.sync_all()?;
        self.update(id, |j| {
            j.downloaded = offset;
            j.speed_bps = 0;
            j.eta_secs = None;
            j.state = "validating".into();
        })?;
        let p = part.clone();
        let size = j.file.size;
        let hash = j.file.sha256.clone();
        let cancel = self.inner.clone();
        let actual = tokio::task::spawn_blocking(move || {
            validate_payload_cancellable(&p, size, hash.as_deref(), &cancel.cancel)
        })
        .await??;
        if self.inner.cancel.load(Ordering::SeqCst) {
            bail!("Проверка остановлена; файл не зарегистрирован");
        }
        fs::hard_link(&part, &final_path)
            .context("Не удалось атомарно опубликовать GGUF без перезаписи")?;
        // The ready record itself is the persistent catalog, committed only after verification.
        self.update(id, |j| {
            j.state = "ready".into();
            j.path = Some(final_path);
            j.actual_sha256 = Some(actual);
            j.error = None;
        })?;
        fs::remove_file(part)?;
        Ok(())
    }
}
pub fn registered(data: &Path) -> Vec<crate::catalog::Model> {
    let jobs: Vec<Download> =
        crate::storage::load_json(&data.join("hf-jobs.json")).unwrap_or_default();
    jobs.into_iter()
        .filter(|j| j.state == "ready")
        .filter_map(|j| {
            Some(crate::catalog::Model {
                repo: j.repo,
                filename: j.file.filename.rsplit('/').next()?.into(),
                path: j.path?,
                size: j.file.size,
                sha256: j.actual_sha256?,
                status: if j.file.sha256.is_some() {
                    "verified-hf-sha256"
                } else {
                    "verified-size-gguf-local-sha256"
                }
                .into(),
                generation_tps: None,
                meets_10_tps: None,
                chat_exit: None,
            })
        })
        .collect()
}
