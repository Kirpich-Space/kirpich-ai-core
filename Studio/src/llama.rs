use crate::{
    catalog::{Model, Profile, live_profile},
    network::{Cancel, LocalEndpoint, client, json_response},
    sse::{SseDecoder, StreamEvent},
    storage::Message,
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub const BINDIR: &str = "/home/kirpich/.llama.cpp/build/bin";
pub fn check_budget(messages: &[Message], context: u32, output: u32) -> Result<()> {
    // UTF-8 bytes are deliberately conservative; exact tokenizer check follows startup.
    let estimate: usize =
        messages.iter().map(|m| m.content.len() + 64).sum::<usize>() + output as usize + 256;
    if estimate > context as usize {
        bail!(
            "История и вложения могут не поместиться в контекст ({estimate} при лимите {context}, включая ответ). Уменьшите вложения, начните новый разговор или явно измените контекст. Данные не обрезаны."
        );
    }
    Ok(())
}
pub fn server_args(path: &Path, port: u16, p: &Profile) -> Result<Vec<String>> {
    let mut a = vec![
        "--model".into(),
        path.to_str().context("Путь модели не UTF-8")?.into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--fit".into(),
        "on".into(),
        "-c".into(),
        p.context.to_string(),
        "-b".into(),
        "512".into(),
        "-ub".into(),
        "128".into(),
        "-fa".into(),
        "on".into(),
        "-fitt".into(),
        "768".into(),
        "-t".into(),
        p.threads.to_string(),
        "-ctk".into(),
        p.cache.clone(),
        "-ctv".into(),
        p.cache.clone(),
        "-np".into(),
        "1".into(),
        "--offline".into(),
        "--no-webui".into(),
        "--no-context-shift".into(),
        "--log-disable".into(),
        "--reasoning".into(),
        "off".into(),
    ];
    for pair in p.flags.chunks(2) {
        if pair.len() != 2 {
            bail!("Непарный экспертный флаг профиля");
        }
        match pair[0].as_str() {
            "-ncmoe" | "--n-cpu-moe" => {
                let n = pair[1]
                    .parse::<u32>()
                    .context("Неверное число CPU MoE слоёв")?;
                if n > 1024 {
                    bail!("Слишком много MoE слоёв");
                }
            }
            "-ot" | "--override-tensor" => {
                if pair[1].len() > 1024
                    || !pair[1].ends_with("=CPU")
                    || pair[1].chars().any(char::is_control)
                {
                    bail!("Разрешена только карта экспертных тензоров в CPU");
                }
            }
            _ => bail!("Не разрешён флаг профиля: {}", pair[0]),
        }
        a.extend_from_slice(pair);
    }
    Ok(a)
}
pub fn gpu_available() -> Result<()> {
    gpu_available_with(std::ffi::OsStr::new("nvidia-smi"))
}
fn gpu_available_with(program: &std::ffi::OsStr) -> Result<()> {
    let output = Command::new(program)
        .args([
            "--query-compute-apps=pid,process_name",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .context("Не удалось проверить GPU через nvidia-smi. Проверьте драйвер NVIDIA.")?;
    if !output.status.success() {
        bail!(
            "GPU недоступен: nvidia-smi не связался с драйвером. Запуск модели отложен; проверьте драйвер и доступ к GPU."
        );
    }
    // Other compute workloads are not a launch veto.
    // llama.cpp reports actual capacity errors; never kill unrelated processes.
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelIdentity {
    path: PathBuf,
    sha: String,
    size: u64,
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl ModelIdentity {
    fn of(model: &Model) -> Result<Self> {
        if !model.loadable() {
            bail!("Модель не проверена или файл изменился");
        }
        let path = model.path.canonicalize()?;
        let m = path.metadata()?;
        Ok(Self {
            path,
            sha: model.sha256.clone(),
            size: m.len(),
            dev: m.dev(),
            ino: m.ino(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        })
    }
}
struct InstanceKey {
    path: PathBuf,
    secret: crate::kaic::Secret,
}
impl InstanceKey {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("kaic-studio-{}.key", uuid::Uuid::new_v4()));
        let secret =
            crate::kaic::Secret::new(format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()));
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        let owned = Self { path, secret };
        f.write_all(owned.secret.expose().as_bytes())?;
        f.sync_all()?;
        Ok(owned)
    }
}
impl Drop for InstanceKey {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
/// Проверяет через платформенный адаптер (`crate::platform::listener_owned_by`), что LISTEN-сокет принадлежит child `pid`.
pub fn listener_owned_by(pid: u32, port: u16) -> Result<bool> {
    crate::platform::listener_owned_by(pid, port)
}
fn require_owned_listener(child: &mut Child, port: u16) -> Result<()> {
    if child.try_wait()?.is_some() || !listener_owned_by(child.id(), port)? {
        bail!("Endpoint не принадлежит собственному живому llama-server; история не отправлена");
    }
    Ok(())
}
pub struct ManagedServer {
    child: Child,
    pub repo: String,
    pub profile: Profile,
    pub endpoint: LocalEndpoint,
    identity: ModelIdentity,
    key: InstanceKey,
    port: u16,
}
impl Drop for ManagedServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl ManagedServer {
    pub async fn start(model: &Model, override_context: u32, cancel: &Cancel) -> Result<Self> {
        if cancel.is_cancelled() {
            bail!("Запуск модели остановлен");
        }
        if !model.loadable() {
            bail!("Модель не проверена, отсутствует или размер не совпадает с манифестом");
        }
        // Re-read at every new model start, never cache tuning values across launches.
        let mut profile = live_profile(&model.repo)?;
        if override_context > 0 {
            if !(512..=2_097_152).contains(&override_context) {
                bail!("Контекст должен быть от 512 до 2097152");
            }
            profile.context = override_context;
        }
        let identity = ModelIdentity::of(model)?;
        gpu_available()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let mut args = server_args(&identity.path, port, &profile)?;
        let key = InstanceKey::new()?;
        let alias = format!("kaic-studio-{}", uuid::Uuid::new_v4());
        args.extend([
            "--api-key-file".into(),
            key.path.to_string_lossy().into(),
            "--alias".into(),
            alias.clone(),
        ]);
        let help = Command::new(format!("{BINDIR}/llama-server"))
            .env_clear()
            .env("LD_LIBRARY_PATH", format!("{BINDIR}:/opt/cuda/lib64"))
            .arg("--help")
            .output()
            .context("Не найден llama-server")?;
        let help = String::from_utf8_lossy(&help.stdout);
        for flag in [
            "--fit",
            "--no-context-shift",
            "--no-webui",
            "--reasoning",
            "--offline",
            "--api-key-file",
            "--alias",
        ] {
            if !help.contains(flag) {
                bail!("Установленный llama-server не подтверждает параметр {flag}");
            }
        }
        let mut launch = Command::new(format!("{BINDIR}/llama-server"));
        // Own child must die with KAIC Studio: an orphaned server would hold VRAM
        // and make the next launch fail. Only our own child receives the signal.
        unsafe {
            use std::os::unix::process::CommandExt;
            launch.pre_exec(|| crate::platform::set_parent_death_signal());
        }
        let mut child = launch
            .env_clear()
            .env("LD_LIBRARY_PATH", format!("{BINDIR}:/opt/cuda/lib64"))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Не удалось запустить llama-server")?;
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let t = line.trim().to_string();
                    if t.is_empty() {
                        continue;
                    }
                    if let Ok(mut buf) = tail.lock() {
                        buf.push(t);
                        if buf.len() > 20 {
                            let excess = buf.len() - 20;
                            buf.drain(0..excess);
                        }
                    }
                }
            });
        }
        let mut server = Self {
            child,
            repo: model.repo.clone(),
            profile,
            endpoint: LocalEndpoint::parse(&format!("http://127.0.0.1:{port}"))?,
            identity,
            key,
            port,
        };
        let http = client()?;
        let start = Instant::now();
        loop {
            if cancel.is_cancelled() {
                bail!("Загрузка отменена; собственный процесс выгружен");
            }
            if let Some(status) = server.child.try_wait()? {
                std::thread::sleep(Duration::from_millis(50));
                let detail = stderr_tail
                    .lock()
                    .ok()
                    .map(|buf| {
                        let errs: Vec<&str> = buf
                            .iter()
                            .filter(|l| {
                                l.contains(" E ")
                                    || l.contains("error")
                                    || l.contains("failed")
                                    || l.contains("not found")
                                    || l.contains("out of memory")
                            })
                            .map(String::as_str)
                            .collect();
                        if !errs.is_empty() {
                            errs.into_iter()
                                .rev()
                                .take(3)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            buf.iter()
                                .rev()
                                .take(4)
                                .cloned()
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    })
                    .unwrap_or_default();
                if detail.is_empty() {
                    bail!(
                        "llama-server завершился ({status}). Проверьте совместимость GGUF, объём памяти и профиль."
                    );
                } else {
                    bail!("llama-server завершился ({status}):\n{detail}");
                }
            }
            if start.elapsed() > Duration::from_secs(180) {
                bail!("llama-server не готов за 180 секунд; собственный процесс остановлен");
            }
            let health = http
                .get(server.endpoint.route("/health"))
                .bearer_auth(server.key.secret.expose())
                .timeout(Duration::from_secs(2))
                .send();
            tokio::select! {
              _=cancel.cancelled()=>bail!("Загрузка отменена"),
              r=health=>if r.is_ok_and(|r|r.status().is_success()){
                require_owned_listener(&mut server.child,port)?;
                let catalog=tokio::select!{_=cancel.cancelled()=>bail!("Загрузка отменена"),v=async {json_response(http.get(server.endpoint.route("/v1/models")).bearer_auth(server.key.secret.expose()).timeout(Duration::from_secs(3)).send().await?).await}=>v?};
                if !catalog["data"].as_array().is_some_and(|a|a.iter().any(|m|m["id"].as_str()==Some(&alias))){bail!("Каталог endpoint не подтверждает случайную идентичность собственного сервера");}
                require_owned_listener(&mut server.child,port)?;
                break;
              }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(server)
    }
    pub fn matches(&mut self, model: &Model, context: u32) -> bool {
        self.child.try_wait().is_ok_and(|s| s.is_none())
            && self.repo == model.repo
            && context == self.profile.context
            && ModelIdentity::of(model).is_ok_and(|id| id == self.identity)
    }
    pub async fn chat(
        &mut self,
        messages: &[Message],
        max_tokens: u32,
        cancel: &Cancel,
        emit: impl FnMut(StreamEvent),
    ) -> Result<()> {
        require_owned_listener(&mut self.child, self.port)?;
        stream_chat_authorized(
            &self.endpoint,
            self.profile.context,
            messages,
            max_tokens,
            cancel,
            emit,
            Some(&self.key.secret),
        )
        .await
    }
    pub async fn agent_turn(
        &mut self,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
        max_tokens: u32,
        cancel: &Cancel,
        emit: impl FnMut(StreamEvent),
    ) -> Result<AgentTurnOutput> {
        require_owned_listener(&mut self.child, self.port)?;
        stream_agent_turn_authorized(
            &self.endpoint,
            self.profile.context,
            messages,
            tools,
            max_tokens,
            cancel,
            emit,
            Some(&self.key.secret),
            None,
        )
        .await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default)]
pub struct AgentTurnOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub hit_limit: bool,
    pub timings: Option<(u32, f32, f32)>,
}

/// Агентный шаг по тому же loopback HTTP/SSE-транспорту (поддерживает tools и tool_calls).
pub async fn stream_agent_turn(
    endpoint: &LocalEndpoint,
    context: u32,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    max_tokens: u32,
    cancel: &Cancel,
    emit: impl FnMut(StreamEvent),
) -> Result<AgentTurnOutput> {
    stream_agent_turn_authorized(
        endpoint, context, messages, tools, max_tokens, cancel, emit, None, None,
    )
    .await
}

/// Агентный шаг с опциональной GBNF-грамматикой для локальных моделей llama.cpp;
/// при HTTP 400/422 из-за неподдерживаемого параметра `grammar` повторяет запрос без него.
#[allow(clippy::too_many_arguments)]
pub async fn stream_agent_turn_with_grammar(
    endpoint: &LocalEndpoint,
    context: u32,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    max_tokens: u32,
    grammar: Option<&str>,
    cancel: &Cancel,
    emit: impl FnMut(StreamEvent),
) -> Result<AgentTurnOutput> {
    stream_agent_turn_authorized(
        endpoint, context, messages, tools, max_tokens, cancel, emit, None, grammar,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_agent_turn_authorized(
    endpoint: &LocalEndpoint,
    context: u32,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    max_tokens: u32,
    cancel: &Cancel,
    mut emit: impl FnMut(StreamEvent),
    key: Option<&crate::kaic::Secret>,
    grammar: Option<&str>,
) -> Result<AgentTurnOutput> {
    let serialized = serde_json::to_string(&json!({"messages": messages, "tools": tools}))?;
    let conservative_estimate = serialized.len() / 2 + max_tokens as usize + 256;
    if conservative_estimate > context as usize * 4 {
        bail!(
            "Контекст агентной задачи превышает безопасный лимит ({conservative_estimate} при контексте {context}). История не обрезана."
        );
    }
    let http = client()?;
    let auth = |r: reqwest::RequestBuilder| {
        if let Some(key) = key {
            r.bearer_auth(key.expose())
        } else {
            r
        }
    };
    let operation = async {
        // Проверяем реальный размер токенов через /tokenize без молчаливой обрезки.
        let tok_resp = auth(http.post(endpoint.route("/tokenize")))
            .json(&json!({"content": serialized, "add_special": true}))
            .send()
            .await?;
        if tok_resp.status().is_success() {
            let tokens = json_response(tok_resp).await?;
            if let Some(count) = tokens["tokens"].as_array().map(Vec::len)
                && count + max_tokens as usize + 32 > context as usize
            {
                bail!(
                    "Точный подсчёт: {count} токенов (с описанием инструментов и историей) плюс ответ не помещаются в контекст {context}. История не обрезана."
                );
            }
        }
        let mut body = json!({
            "messages": messages,
            "stream": true,
            "max_tokens": max_tokens,
            "temperature": 0.2,
            "cache_prompt": true,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(g) = grammar
            && !g.trim().is_empty()
        {
            body["grammar"] = json!(g);
        }
        let mut r = auth(http.post(endpoint.route("/v1/chat/completions")))
            .json(&body)
            .send()
            .await?;
        if !r.status().is_success()
            && body.get("grammar").is_some()
            && matches!(r.status().as_u16(), 400 | 422)
        {
            body.as_object_mut().map(|m| m.remove("grammar"));
            r = auth(http.post(endpoint.route("/v1/chat/completions")))
                .json(&body)
                .send()
                .await?;
        }
        if !r.status().is_success() {
            let code = r.status().as_u16();
            let detail = r.text().await.unwrap_or_default();
            let trimmed = detail.trim();
            if trimmed.to_ascii_lowercase().contains("tool")
                || trimmed.to_ascii_lowercase().contains("template")
                || code == 400
                || code == 422
            {
                bail!(
                    "Локальная модель или шаблон llama.cpp не поддерживает структурированный tool calling (HTTP {code}): {}",
                    trimmed.chars().take(240).collect::<String>()
                );
            }
            bail!("llama.cpp (agent): HTTP {code}");
        }
        let mut bytes = r.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut done = false;
        let mut out = AgentTurnOutput::default();
        let mut partial_calls: std::collections::BTreeMap<usize, ToolCall> =
            std::collections::BTreeMap::new();
        while let Some(chunk) = bytes.next().await {
            for event in decoder.push(&chunk?)? {
                match &event {
                    StreamEvent::Text(s) => out.text.push_str(s),
                    StreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_delta,
                    } => {
                        let entry = partial_calls.entry(*index).or_insert_with(|| ToolCall {
                            id: format!("call_{index}"),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        if let Some(id) = id
                            && !id.is_empty()
                        {
                            entry.id = id.clone();
                        }
                        if let Some(name) = name {
                            entry.name.push_str(name);
                        }
                        if let Some(arg) = arguments_delta {
                            entry.arguments.push_str(arg);
                        }
                    }
                    StreamEvent::Timings {
                        tokens,
                        tps,
                        seconds,
                    } => {
                        out.timings = Some((*tokens, *tps, *seconds));
                    }
                    StreamEvent::Limit => out.hit_limit = true,
                    StreamEvent::Done => done = true,
                    StreamEvent::Reasoning(_) | StreamEvent::ToolCallsDone => {}
                }
                emit(event);
            }
            if done {
                break;
            }
        }
        decoder.finish()?;
        out.tool_calls = partial_calls
            .into_values()
            .filter(|tc| !tc.name.trim().is_empty())
            .collect();
        if out.text.trim().is_empty() && out.tool_calls.is_empty() {
            bail!("Сервер завершил агентный шаг без текста и без вызовов инструментов");
        }
        Ok(out)
    };
    tokio::select! {
        _ = cancel.cancelled() => bail!("Агентная операция остановлена пользователем; соединение закрыто, сервер остаётся загруженным"),
        r = operation => r,
    }
}
/// Общий HTTP/SSE transport; контрактные тесты используют loopback fixture.
pub async fn stream_chat(
    endpoint: &LocalEndpoint,
    context: u32,
    messages: &[Message],
    max_tokens: u32,
    cancel: &Cancel,
    emit: impl FnMut(StreamEvent),
) -> Result<()> {
    stream_chat_authorized(endpoint, context, messages, max_tokens, cancel, emit, None).await
}
async fn stream_chat_authorized(
    endpoint: &LocalEndpoint,
    context: u32,
    messages: &[Message],
    max_tokens: u32,
    cancel: &Cancel,
    mut emit: impl FnMut(StreamEvent),
    key: Option<&crate::kaic::Secret>,
) -> Result<()> {
    check_budget(messages, context, max_tokens)?;
    let http = client()?;
    let auth = |r: reqwest::RequestBuilder| {
        if let Some(key) = key {
            r.bearer_auth(key.expose())
        } else {
            r
        }
    };
    let wire = messages
        .iter()
        .map(|m| json!({"role":m.role,"content":m.content}))
        .collect::<Vec<_>>();
    let has_images = messages.iter().any(|m| {
        m.media
            .iter()
            .any(|gm| gm.kind == "image" && gm.path.is_file())
    });
    let wire_multimodal = if has_images {
        Some(
            messages
                .iter()
                .map(|m| {
                    let imgs: Vec<String> = m
                        .media
                        .iter()
                        .filter(|gm| gm.kind == "image" && gm.path.is_file())
                        .filter_map(|gm| crate::attachments::image_data_url(&gm.path).ok())
                        .collect();
                    if imgs.is_empty() {
                        json!({"role": m.role, "content": m.content})
                    } else {
                        let mut parts = vec![json!({"type": "text", "text": m.content})];
                        for url in imgs {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": url}
                            }));
                        }
                        json!({"role": m.role, "content": parts})
                    }
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let operation = async {
        // The server applies the model's real chat template; count actual tokens too.
        let template = json_response(
            auth(http.post(endpoint.route("/apply-template")))
                .json(&json!({"messages":wire,"add_generation_prompt":true}))
                .send()
                .await?,
        )
        .await?;
        let prompt = template["prompt"]
            .as_str()
            .context("llama-server не вернул шаблон диалога")?;
        let tokens = json_response(
            auth(http.post(endpoint.route("/tokenize")))
                .json(&json!({"content":prompt,"add_special":true}))
                .send()
                .await?,
        )
        .await?;
        let count = tokens["tokens"]
            .as_array()
            .context("Нет результата токенизации")?
            .len();
        if count + max_tokens as usize + 32 > context as usize {
            bail!(
                "Точный подсчёт: {count} входных токенов плюс ответ не помещаются. История не обрезана."
            );
        }
        let mut r = if let Some(mm) = &wire_multimodal {
            let mm_resp = auth(http.post(endpoint.route("/v1/chat/completions")))
                .json(&json!({"messages":mm,"stream":true,"max_tokens":max_tokens,"temperature":0.6,"cache_prompt":true}))
                .send()
                .await?;
            if mm_resp.status().is_success() {
                mm_resp
            } else {
                auth(http.post(endpoint.route("/v1/chat/completions")))
                    .json(&json!({"messages":wire,"stream":true,"max_tokens":max_tokens,"temperature":0.6,"cache_prompt":true}))
                    .send()
                    .await?
            }
        } else {
            auth(http.post(endpoint.route("/v1/chat/completions")))
                .json(&json!({"messages":wire,"stream":true,"max_tokens":max_tokens,"temperature":0.6,"cache_prompt":true}))
                .send()
                .await?
        };
        if !r.status().is_success() {
            let _ = &mut r;
            bail!("llama.cpp: HTTP {}", r.status().as_u16());
        }
        let mut bytes = r.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut done = false;
        let mut has_text = false;
        while let Some(chunk) = bytes.next().await {
            for event in decoder.push(&chunk?)? {
                if let StreamEvent::Text(s) = &event {
                    has_text |= !s.trim().is_empty();
                }
                if event == StreamEvent::Done {
                    done = true;
                }
                emit(event);
            }
            if done {
                break;
            }
        }
        decoder.finish()?;
        if !has_text {
            bail!("Сервер завершил поток без непустого текстового ответа");
        }
        Ok(())
    };
    tokio::select! {_=cancel.cancelled()=>bail!("Ответ остановлен пользователем; соединение закрыто, сервер остаётся загруженным"),r=operation=>r}
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    fn model(path: PathBuf) -> Model {
        Model {
            repo: "same-repo".into(),
            filename: path.file_name().unwrap().to_string_lossy().into(),
            path,
            size: 4,
            sha256: "sha".into(),
            status: "verified".into(),
            generation_tps: None,
            meets_10_tps: None,
            chat_exit: None,
        }
    }
    fn server(first: &Model) -> ManagedServer {
        ManagedServer {
            child: Command::new("sleep").arg("30").spawn().unwrap(),
            repo: first.repo.clone(),
            profile: Profile::default(),
            endpoint: LocalEndpoint::parse("http://127.0.0.1:12345").unwrap(),
            identity: ModelIdentity::of(first).unwrap(),
            key: InstanceKey::new().unwrap(),
            port: 12345,
        }
    }
    #[test]
    fn model_reuse_requires_same_gguf_file() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.gguf");
        let b = d.path().join("b.gguf");
        std::fs::write(&a, b"GGUF").unwrap();
        std::fs::write(&b, b"GGUF").unwrap();
        let first = model(a);
        let second = model(b);
        let mut s = server(&first);
        assert!(s.matches(&first, 4096));
        assert!(!s.matches(&second, 4096));
        let mut changed = first.clone();
        changed.sha256 = "new-sha".into();
        assert!(!s.matches(&changed, 4096));
        assert!(!s.matches(&first, 8192));
    }
    #[test]
    fn drop_reaps_only_owned_child_and_removes_own_private_key() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.gguf");
        std::fs::write(&p, b"GGUF").unwrap();
        let s = server(&model(p));
        let pid = s.child.id();
        let keypath = s.key.path.clone();
        assert_eq!(keypath.metadata().unwrap().mode() & 0o777, 0o600);
        drop(s);
        assert!(!crate::platform::process_alive(pid));
        assert!(!keypath.exists());
    }
    #[test]
    fn fake_http_listener_is_rejected_even_if_child_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listener_owned_by(std::process::id(), port).unwrap());
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let r = require_owned_listener(&mut child, port);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            r.is_err(),
            "Чужой /health200 не подтверждает владение endpoint"
        );
    }
}

#[cfg(test)]
mod gpu_sharing_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn competing_compute_processes_do_not_block_model_testing() {
        let d = tempfile::tempdir().unwrap();
        let probe = d.path().join("nvidia-smi");
        std::fs::write(
            &probe,
            "#!/bin/sh\nprintf '123, python3\\n456, llama-server\\n'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(gpu_available_with(probe.as_os_str()).is_ok());
    }
    #[test]
    fn actual_driver_failure_remains_an_error() {
        let d = tempfile::tempdir().unwrap();
        let probe = d.path().join("nvidia-smi");
        std::fs::write(&probe, "#!/bin/sh\nexit 9\n").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(gpu_available_with(probe.as_os_str()).is_err());
    }
}
