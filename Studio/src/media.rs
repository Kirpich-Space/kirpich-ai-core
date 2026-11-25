use crate::{
    network::{Cancel, LocalEndpoint, client, json_response},
    storage::save_json,
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OutputRef {
    pub filename: String,
    #[serde(default)]
    pub subfolder: String,
    #[serde(rename = "type", default = "output_type")]
    pub kind: String,
}
fn output_type() -> String {
    "output".into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub source: OutputRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaRecord {
    pub local_id: Uuid,
    pub prompt_id: String,
    pub prompt: String,
    pub workflow: Value,
    pub seeds: Vec<Value>,
    pub elapsed_seconds: f64,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_unix: u64,
    pub outputs: Vec<Artifact>,
}
pub fn safe_output_name(id: &str, index: usize, name: &str) -> Result<String> {
    if name.is_empty()
        || name.contains(['/', '\\', '\0'])
        || name == "."
        || name == ".."
        || name.chars().any(char::is_control)
    {
        bail!("Небезопасное имя результата ComfyUI");
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ![
        "png", "jpg", "jpeg", "webp", "gif", "mp4", "webm", "mkv", "mov", "avif",
    ]
    .contains(&ext.as_str())
    {
        bail!("Неподдерживаемый формат результата ComfyUI");
    }
    // Ignore server-controlled basename and job id, retain only a checked extension.
    let safe = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(80)
        .collect::<String>();
    if safe.is_empty() {
        bail!("Нет безопасного идентификатора результата");
    }
    Ok(format!("{safe}-{index}.{ext}"))
}
pub fn set_positive_prompt(w: &mut Value, id: &str, text: &str) -> Result<()> {
    let node = w
        .get_mut(id)
        .context("Узел положительного промпта не найден")?;
    let input = node
        .get_mut("inputs")
        .and_then(|v| v.get_mut("text"))
        .filter(|v| v.is_string())
        .context(
            "В указанном узле нет строкового inputs.text. Укажите ID нужного узла API workflow.",
        )?;
    *input = Value::String(text.into());
    Ok(())
}
pub fn parse_history(v: &Value, id: &str) -> Result<Option<Vec<OutputRef>>> {
    let Some(h) = v.get(id) else { return Ok(None) };
    let status = &h["status"];
    let error = status["status_str"].as_str() == Some("error")
        || status["messages"].as_array().is_some_and(|a| {
            a.iter().any(|m| {
                matches!(
                    m[0].as_str(),
                    Some("execution_error" | "execution_interrupted")
                )
            })
        });
    if error {
        bail!(
            "ComfyUI сообщил ошибку или прерывание исполнения workflow. Проверьте узлы, установленные модели и очередь backend."
        );
    }
    if status["completed"].as_bool() != Some(true)
        || status["status_str"].as_str() != Some("success")
    {
        return Ok(None);
    }
    let mut outputs = vec![];
    if let Some(nodes) = h["outputs"].as_object() {
        for n in nodes.values() {
            for key in ["images", "gifs", "videos"] {
                if let Some(items) = n[key].as_array() {
                    for item in items {
                        let r: OutputRef = serde_json::from_value(item.clone())
                            .context("Несовместимое описание артефакта ComfyUI")?;
                        safe_output_name("validation", 0, &r.filename)?;
                        if r.subfolder.contains('\\')
                            || Path::new(&r.subfolder).is_absolute()
                            || r.subfolder.split('/').any(|x| x == "..")
                            || !["output", "temp"].contains(&r.kind.as_str())
                        {
                            bail!("Небезопасный путь артефакта ComfyUI");
                        }
                        outputs.push(r);
                    }
                }
            }
        }
    }
    if outputs.is_empty() {
        bail!("ComfyUI завершил workflow без изображений или видео; успех медиа не подтверждён");
    }
    Ok(Some(outputs))
}
pub struct ComfyClient {
    endpoint: LocalEndpoint,
    http: reqwest::Client,
}
impl ComfyClient {
    pub fn new(endpoint: LocalEndpoint) -> Result<Self> {
        Ok(Self {
            endpoint,
            http: client()?,
        })
    }
    pub async fn inspect(&self) -> Result<Value> {
        let stats = json_response(
            self.http
                .get(self.endpoint.route("/system_stats"))
                .timeout(Duration::from_secs(5))
                .send()
                .await?,
        )
        .await?;
        let objects = json_response(
            self.http
                .get(self.endpoint.route("/object_info"))
                .timeout(Duration::from_secs(10))
                .send()
                .await?,
        )
        .await?;
        Ok(json!({"system_stats":stats,"object_info":objects}))
    }
    pub async fn run(
        &self,
        workflow: Value,
        prompt: String,
        outputs: &Path,
        cancel: &Cancel,
        mut status: impl FnMut(String),
    ) -> Result<MediaRecord> {
        if cancel.is_cancelled() {
            bail!("Запуск медиа остановлен до отправки workflow");
        }
        if !workflow.is_object()
            || workflow.as_object().is_none_or(|m| m.is_empty())
            || workflow.get("nodes").is_some()
        {
            bail!(
                "Нужен API workflow: объект ID → {{class_type, inputs}}, а не визуальный граф ComfyUI"
            );
        }
        for node in workflow.as_object().unwrap().values() {
            if !node["class_type"].is_string() || !node["inputs"].is_object() {
                bail!("Некорректный узел API workflow");
            }
        }
        let local_id = Uuid::new_v4();
        let start = Instant::now();
        let mut record = MediaRecord {
            local_id,
            prompt_id: String::new(),
            prompt,
            workflow: workflow.clone(),
            seeds: vec![],
            elapsed_seconds: 0.0,
            state: "отправка".into(),
            error: None,
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            outputs: vec![],
        };
        for n in workflow.as_object().unwrap().values() {
            for k in ["seed", "noise_seed"] {
                if n["inputs"][k].is_number() {
                    record.seeds.push(n["inputs"][k].clone());
                }
            }
        }
        fs::create_dir_all(outputs)?;
        save_json(&outputs.join(format!("{local_id}.json")), &record)?;
        let sending = async {
            json_response(
                self.http
                    .post(self.endpoint.route("/prompt"))
                    .timeout(Duration::from_secs(15))
                    .json(&json!({"prompt":workflow,"client_id":local_id}))
                    .send()
                    .await?,
            )
            .await
        };
        let submitted = tokio::select! {biased;_=cancel.cancelled()=>Err(anyhow::anyhow!("Ожидание создания задания ComfyUI остановлено. Backend мог принять workflow; prompt_id не получен. Проверьте очередь перед повторной отправкой.")),r=sending=>r};
        let v = match submitted {
            Ok(v) => v,
            Err(e) => {
                record.state = "исход отправки неизвестен".into();
                record.error = Some(format!("{e:#}"));
                record.elapsed_seconds = start.elapsed().as_secs_f64();
                save_json(&outputs.join(format!("{local_id}.json")), &record)?;
                return Err(e);
            }
        };
        if v.get("error").is_some()
            || v.get("node_errors")
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
        {
            record.state = "workflow отклонён backend".into();
            record.error = Some("Проверьте узлы, входы и модели".into());
            save_json(&outputs.join(format!("{local_id}.json")), &record)?;
            bail!("ComfyUI отклонил workflow: проверьте узлы, входы и модели");
        }
        let id = match v["prompt_id"].as_str().filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => {
                record.state = "исход отправки неизвестен".into();
                save_json(&outputs.join(format!("{local_id}.json")), &record)?;
                bail!("ComfyUI не вернул prompt_id; проверьте очередь до повторной отправки");
            }
        };
        record.prompt_id = id.clone();
        record.state = "наблюдение".into();
        save_json(&outputs.join(format!("{local_id}.json")), &record)?;
        status(format!(
            "В очереди ComfyUI · {id}. Остановка наблюдения не отменяет backend."
        ));
        let result:Result<()>=async {
        let refs = loop {
            if cancel.is_cancelled() {
                bail!("Наблюдение остановлено. Задание ComfyUI {id} НЕ отменено на backend.");
            }
            if start.elapsed() > Duration::from_secs(3600) {
                bail!("Истёк час наблюдения. Задание ComfyUI {id} может продолжаться.");
            }
            let v = tokio::select! {_=cancel.cancelled()=>bail!("Наблюдение остановлено; backend ComfyUI продолжает задание {id}"),r=self.http.get(self.endpoint.segment("/history/",&id)).timeout(Duration::from_secs(10)).send()=>json_response(r?).await?};
            if let Some(refs) = parse_history(&v, &id)? {
                break refs;
            }
            // Queue API distinguishes a real running job from a still queued one.
            if let Ok(r) = self.http.get(self.endpoint.route("/queue")).timeout(Duration::from_secs(3)).send().await
                && let Ok(q)=json_response(r).await
                && q["queue_running"].as_array().is_some_and(|a|a.iter().any(|x|x[1].as_str()==Some(&id))) {
                status(format!("ComfyUI выполняет {id}"));
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        };
        if refs.len()>32 {bail!("Workflow вернул более 32 артефактов; загрузка отклонена");}
        let mut total_bytes=0usize;
        for (i, r) in refs.into_iter().enumerate() {
            if cancel.is_cancelled() {
                bail!("Загрузка результатов остановлена; backend не прерывался");
            }
            let mut u = self.endpoint.route("/view");
            u.query_pairs_mut()
                .append_pair("filename", &r.filename)
                .append_pair("subfolder", &r.subfolder)
                .append_pair("type", &r.kind);
            let response = tokio::select!{_=cancel.cancelled()=>bail!("Загрузка остановлена; backend не прерывался"),r=self.http.get(u).timeout(Duration::from_secs(120)).send()=>r?};
            if !response.status().is_success() {
                bail!(
                    "Не удалось скачать артефакт: HTTP {}",
                    response.status().as_u16()
                );
            }
            let final_path = outputs.join(safe_output_name(&local_id.to_string(), i, &r.filename)?);
            let path=final_path.with_file_name(format!("{}.part",final_path.file_name().unwrap().to_string_lossy()));
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            let mut stream = response.bytes_stream();
            let mut size = 0usize;
            loop {
                let next = tokio::select! {_=cancel.cancelled()=>bail!("Загрузка остановлена; частичный файл не считается результатом"),v=stream.next()=>v};
                let Some(b) = next else { break };
                let b = b?;
                size += b.len(); total_bytes+=b.len();
                if total_bytes>1024*1024*1024 {bail!("Сумма результатов превышает 1 ГиБ");}
                if size > 512 * 1024 * 1024 {
                    bail!("Результат превышает 512 МиБ");
                }
                f.write_all(&b)?;
            }
            if size == 0 {
                bail!("ComfyUI вернул пустой файл");
            }
            f.sync_all()?;
            drop(f); validate_artifact(&path)?;
            if final_path.exists(){bail!("Итоговый файл уже существует; он сохранён без изменений");}
            fs::rename(&path,&final_path)?;
            record.outputs.push(Artifact { path:final_path, source: r });
        }
        Ok(())
        }.await;
        record.elapsed_seconds = start.elapsed().as_secs_f64();
        record.state = if result.is_ok() {
            "получено"
        } else if cancel.is_cancelled() {
            "наблюдение остановлено; backend не отменён"
        } else {
            "ошибка"
        }
        .into();
        record.error = result.as_ref().err().map(|e| format!("{e:#}"));
        save_json(&outputs.join(format!("{local_id}.json")), &record)?;
        result?;
        Ok(record)
    }
}
pub fn model_choices(info: &Value) -> Vec<(String, String, Vec<String>)> {
    let mut out = vec![];
    if let Some(nodes) = info["object_info"].as_object() {
        for (node, def) in nodes {
            for section in ["required", "optional"] {
                if let Some(inputs) = def["input"][section].as_object() {
                    for (name, def) in inputs {
                        if !["ckpt", "model", "vae", "unet", "clip"]
                            .iter()
                            .any(|key| name.contains(key))
                        {
                            continue;
                        }
                        let Some(values) = def[0].as_array() else {
                            continue;
                        };
                        let v = values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect::<Vec<_>>();
                        if !v.is_empty() {
                            out.push((node.clone(), name.clone(), v));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Проверка сигнатуры скачанного медиа; ответ HTML/JSON не считается артефактом.
pub fn validate_artifact(path: &Path) -> Result<()> {
    use std::io::Read;
    let mut head = [0u8; 32];
    let n = std::fs::File::open(path)?.read(&mut head)?;
    let b = &head[..n];
    let stem = path.file_stem().map(Path::new);
    let ext = if path.extension().is_some_and(|x| x == "part") {
        stem.and_then(|p| p.extension())
            .and_then(|x| x.to_str())
            .unwrap_or("")
    } else {
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    };
    let valid = match ext {
        "png" => b.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => b.starts_with(&[0xff, 0xd8, 0xff]),
        "webp" => b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"),
        "gif" => b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a"),
        "mp4" | "mov" | "avif" => b.get(4..8) == Some(b"ftyp"),
        "webm" | "mkv" => b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        _ => false,
    };
    if !valid {
        bail!("Скачанный файл не имеет сигнатуры заявленного медиаформата");
    }
    if matches!(ext, "png" | "jpg" | "jpeg") {
        decode_image(path)?;
    } else {
        let mut cmd = std::process::Command::new("ffprobe");
        cmd.args([
            "-v",
            "error",
            "-protocol_whitelist",
            "file",
            "-show_entries",
            "stream=codec_type,width,height:format=format_name",
            "-of",
            "json",
        ])
        .arg(path);
        let (status, bytes) = crate::process::capture(cmd, Duration::from_secs(10), 65536)
            .context("Файл скачан в .part, но медиа не подтверждено: нужен локальный ffprobe")?;
        let info: Value =
            serde_json::from_slice(&bytes).context("ffprobe не вернул описание медиа")?;
        if !status.success()
            || !info["streams"].as_array().is_some_and(|a| {
                a.iter().any(|s| {
                    s["codec_type"] == "video"
                        && s["width"].as_u64().is_some_and(|x| x > 0)
                        && s["height"].as_u64().is_some_and(|x| x > 0)
                })
            })
        {
            bail!("Файл скачан в .part, но валидатор не подтвердил изображение/видео");
        }
    }
    Ok(())
}

pub fn decode_image(path: &Path) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(96 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .context("Скачанный файл изображения повреждён или превышает лимиты декодирования")
}
