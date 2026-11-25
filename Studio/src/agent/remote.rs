//! Нативные HTTP-бэкенды провайдеров для единого агентного runtime KAIC:
//! - `OllamaBackend`: локальный `/api/chat` + `/api/tags` строго на loopback (никогда не уходит в облако);
//! - `OpenAiCompatBackend`: `/v1/chat/completions` для локальных (LM Studio/vLLM/llama.cpp) или явно разрешённых HTTPS API;
//! - `AnthropicApiBackend`: нативный `/v1/messages` с потоковой сборкой `tool_use` и `Cancel`.

use super::{AgentModelBackend, ProviderKind};
use crate::{
    kaic::Secret,
    llama::{AgentTurnOutput, ToolCall},
    network::{Cancel, LocalEndpoint},
    sse::{SseDecoder, StreamEvent},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, net::IpAddr, pin::Pin, time::Duration};
use url::Url;

fn api_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(600))
        .build()?)
}

fn is_loopback_host(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Проверяет endpoint: локальный loopback разрешён по `http://`,
/// удалённый хост требует `https://` и явного согласия пользователя (`external_consent == true`).
pub fn validate_api_endpoint(raw_url: &str, external_consent: bool) -> Result<(Url, bool)> {
    let url = Url::parse(raw_url.trim()).context("Некорректный URL провайдера")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("Учётные данные в URL запрещены; используйте переменные окружения для ключей");
    }
    let loopback = is_loopback_host(&url);
    if loopback {
        if url.scheme() != "http" {
            bail!("Для loopback-сервера ожидается http://");
        }
    } else {
        if url.scheme() != "https" {
            bail!("Удалённый API разрешён только по HTTPS");
        }
        if !external_consent {
            bail!(
                "Внешний API ({}) заблокирован без явного согласия на передачу контекста workspace",
                url.host_str().unwrap_or("remote")
            );
        }
    }
    Ok((url, !loopback))
}

fn join_route(base: &Url, route: &str) -> Url {
    let mut u = base.clone();
    let prefix = u.path().trim_end_matches('/');
    let suffix = route.trim_start_matches('/');
    if prefix.is_empty() {
        u.set_path(&format!("/{suffix}"));
    } else {
        u.set_path(&format!("{prefix}/{suffix}"));
    }
    u
}

/// Локальный бэкенд Ollama (`/api/chat` NDJSON, строго loopback).
pub struct OllamaBackend {
    endpoint: LocalEndpoint,
    model: String,
    context: u32,
}

impl OllamaBackend {
    pub fn new(endpoint_url: &str, model: String, context: u32) -> Result<Self> {
        let trimmed = if endpoint_url.trim().is_empty() {
            "http://127.0.0.1:11434/"
        } else {
            endpoint_url.trim()
        };
        let endpoint = LocalEndpoint::parse(trimmed).context(
            "Ollama локальный бэкенд принимает только loopback-адрес http://127.0.0.1:11434 (в облако молча не маршрутизируется)",
        )?;
        Ok(Self {
            endpoint,
            model,
            context: context.max(512),
        })
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let http = api_client()?;
        let resp = http.get(self.endpoint.route("/api/tags")).send().await?;
        if !resp.status().is_success() {
            bail!("Ollama /api/tags: HTTP {}", resp.status().as_u16());
        }
        let body: Value = resp.json().await?;
        let names = body["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str().map(str::to_string))
            .collect();
        Ok(names)
    }
}

fn messages_for_ollama(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            if role == "assistant"
                && let Some(tcs) = m["tool_calls"].as_array()
            {
                let converted: Vec<Value> = tcs
                    .iter()
                    .map(|tc| {
                        let raw_args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                        let args_obj: Value =
                            serde_json::from_str(raw_args).unwrap_or_else(|_| json!({}));
                        json!({
                            "function": {
                                "name": tc["function"]["name"],
                                "arguments": args_obj,
                            }
                        })
                    })
                    .collect();
                let mut obj = json!({
                    "role": "assistant",
                    "content": m["content"].as_str().unwrap_or(""),
                    "tool_calls": converted,
                });
                if obj["content"].is_null() {
                    obj["content"] = json!("");
                }
                obj
            } else {
                let mut obj = m.clone();
                if obj["content"].is_null() {
                    obj["content"] = json!("");
                }
                obj
            }
        })
        .collect()
}

impl AgentModelBackend for OllamaBackend {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::OllamaLocal
    }

    fn model_label(&self) -> &str {
        &self.model
    }

    fn turn<'a>(
        &'a mut self,
        messages: &'a [Value],
        tools: &'a [Value],
        max_tokens: u32,
        cancel: &'a Cancel,
        on_event: &'a mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'a>> {
        Box::pin(async move {
            let http = api_client()?;
            let mut body = json!({
                "model": self.model,
                "messages": messages_for_ollama(messages),
                "stream": true,
                "options": {
                    "temperature": 0.2,
                    "num_predict": max_tokens,
                    "num_ctx": self.context,
                }
            });
            if !tools.is_empty() {
                body["tools"] = json!(tools);
            }

            let operation = async {
                let r = http
                    .post(self.endpoint.route("/api/chat"))
                    .json(&body)
                    .send()
                    .await?;
                if !r.status().is_success() {
                    let code = r.status().as_u16();
                    let detail = r.text().await.unwrap_or_default();
                    bail!(
                        "Ollama /api/chat: HTTP {code}: {}",
                        detail.chars().take(240).collect::<String>()
                    );
                }
                let mut bytes = r.bytes_stream();
                let mut buf = Vec::new();
                let mut out = AgentTurnOutput::default();
                let mut tc_idx = 0usize;

                while let Some(chunk) = bytes.next().await {
                    let chunk = chunk?;
                    buf.extend_from_slice(&chunk);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                        let line = String::from_utf8_lossy(&line_bytes);
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let v: Value = serde_json::from_str(trimmed)
                            .with_context(|| format!("Некорректный NDJSON Ollama: {trimmed}"))?;
                        if let Some(err) = v["error"].as_str() {
                            bail!("Ollama ошибка: {err}");
                        }
                        if let Some(msg) = v.get("message") {
                            if let Some(thinking) = msg["thinking"].as_str()
                                && !thinking.is_empty()
                            {
                                on_event(StreamEvent::Reasoning(thinking.to_string()));
                            }
                            if let Some(txt) = msg["content"].as_str()
                                && !txt.is_empty()
                            {
                                out.text.push_str(txt);
                                on_event(StreamEvent::Text(txt.to_string()));
                            }
                            if let Some(calls) = msg["tool_calls"].as_array() {
                                for c in calls {
                                    let name = c["function"]["name"]
                                        .as_str()
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if name.is_empty() {
                                        continue;
                                    }
                                    let args_str = match &c["function"]["arguments"] {
                                        Value::String(s) => s.clone(),
                                        other => serde_json::to_string(other)
                                            .unwrap_or_else(|_| "{}".into()),
                                    };
                                    out.tool_calls.push(ToolCall {
                                        id: format!("ollama_call_{tc_idx}"),
                                        name,
                                        arguments: args_str,
                                    });
                                    tc_idx += 1;
                                }
                            }
                        }
                        if v["done"].as_bool() == Some(true) {
                            if let Some(eval_count) = v["eval_count"].as_u64()
                                && let Some(eval_ns) = v["eval_duration"].as_u64()
                                && eval_ns > 0
                            {
                                let secs = eval_ns as f32 / 1_000_000_000.0;
                                let tps = eval_count as f32 / secs.max(0.001);
                                out.timings = Some((eval_count as u32, tps, secs));
                                on_event(StreamEvent::Timings {
                                    tokens: eval_count as u32,
                                    tps,
                                    seconds: secs,
                                });
                            }
                            on_event(StreamEvent::Done);
                            break;
                        }
                    }
                }
                if out.text.trim().is_empty() && out.tool_calls.is_empty() {
                    bail!("Ollama завершила шаг без текста и без вызовов инструментов");
                }
                Ok(out)
            };

            tokio::select! {
                _ = cancel.cancelled() => bail!("Операция Ollama остановлена пользователем"),
                r = operation => r,
            }
        })
    }
}

/// OpenAI-совместимый SSE бэкенд (локальный endpoint или явно разрешённый HTTPS API).
pub struct OpenAiCompatBackend {
    base_url: Url,
    model: String,
    api_key: Option<Secret>,
    external_endpoint: bool,
}

impl OpenAiCompatBackend {
    pub fn new(
        base_url: &str,
        model: String,
        api_key: Option<Secret>,
        external_consent: bool,
    ) -> Result<Self> {
        let (url, external_endpoint) = validate_api_endpoint(base_url, external_consent)?;
        Ok(Self {
            base_url: url,
            model,
            api_key,
            external_endpoint,
        })
    }

    pub fn is_external_endpoint(&self) -> bool {
        self.external_endpoint
    }
}

impl AgentModelBackend for OpenAiCompatBackend {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompat
    }

    fn model_label(&self) -> &str {
        &self.model
    }

    fn turn<'a>(
        &'a mut self,
        messages: &'a [Value],
        tools: &'a [Value],
        max_tokens: u32,
        cancel: &'a Cancel,
        on_event: &'a mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'a>> {
        Box::pin(async move {
            let http = api_client()?;
            let mut body = json!({
                "model": self.model,
                "messages": messages,
                "stream": true,
                "max_tokens": max_tokens,
                "temperature": 0.2,
            });
            if !tools.is_empty() {
                body["tools"] = json!(tools);
                body["tool_choice"] = json!("auto");
            }
            let target = join_route(&self.base_url, "/v1/chat/completions");

            let operation = async {
                let mut req = http.post(target).json(&body);
                if let Some(k) = &self.api_key
                    && !k.expose().trim().is_empty()
                {
                    req = req.bearer_auth(k.expose());
                }
                let r = req.send().await?;
                if !r.status().is_success() {
                    let code = r.status().as_u16();
                    let detail = r.text().await.unwrap_or_default();
                    bail!(
                        "OpenAI-compat API: HTTP {code}: {}",
                        detail.chars().take(240).collect::<String>()
                    );
                }
                let mut bytes = r.bytes_stream();
                let mut decoder = SseDecoder::default();
                let mut done = false;
                let mut out = AgentTurnOutput::default();
                let mut partial_calls: BTreeMap<usize, ToolCall> = BTreeMap::new();
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
                                let entry =
                                    partial_calls.entry(*index).or_insert_with(|| ToolCall {
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
                            } => out.timings = Some((*tokens, *tps, *seconds)),
                            StreamEvent::Limit => out.hit_limit = true,
                            StreamEvent::Done => done = true,
                            StreamEvent::Reasoning(_) | StreamEvent::ToolCallsDone => {}
                        }
                        on_event(event);
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
                    bail!("OpenAI-compat endpoint завершил ответ без текста и tool_calls");
                }
                Ok(out)
            };

            tokio::select! {
                _ = cancel.cancelled() => bail!("Операция OpenAI-compat API остановлена пользователем"),
                r = operation => r,
            }
        })
    }
}

/// Нативный клиент Anthropic `/v1/messages` (SSE + `tool_use` / `tool_result`).
pub struct AnthropicApiBackend {
    base_url: Url,
    model: String,
    api_key: Secret,
}

impl AnthropicApiBackend {
    pub fn new(
        base_url: &str,
        model: String,
        api_key: Secret,
        external_consent: bool,
    ) -> Result<Self> {
        let trimmed = if base_url.trim().is_empty() {
            "https://api.anthropic.com"
        } else {
            base_url.trim()
        };
        let (url, _) = validate_api_endpoint(trimmed, external_consent)?;
        if api_key.expose().trim().is_empty() {
            bail!(
                "Для Anthropic API требуется ключ (ANTHROPIC_API_KEY или KAIC_ANTHROPIC_API_KEY)"
            );
        }
        Ok(Self {
            base_url: url,
            model,
            api_key,
        })
    }
}

fn convert_to_anthropic_payload(
    model: &str,
    messages: &[Value],
    tools: &[Value],
    max_tokens: u32,
) -> Value {
    let mut system_parts = Vec::new();
    let mut out_msgs = Vec::new();

    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        match role {
            "system" => {
                if let Some(c) = m["content"].as_str()
                    && !c.trim().is_empty()
                {
                    system_parts.push(c.to_string());
                }
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(txt) = m["content"].as_str()
                    && !txt.is_empty()
                {
                    blocks.push(json!({"type": "text", "text": txt}));
                }
                if let Some(tcs) = m["tool_calls"].as_array() {
                    for tc in tcs {
                        let raw_args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(raw_args).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc["id"].as_str().unwrap_or("toolu_0"),
                            "name": tc["function"]["name"],
                            "input": input,
                        }));
                    }
                }
                if !blocks.is_empty() {
                    out_msgs.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap_or("toolu_0");
                let content = m["content"].as_str().unwrap_or("");
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                });
                if let Some(last) = out_msgs.last_mut()
                    && last["role"].as_str() == Some("user")
                    && last["content"].is_array()
                {
                    last["content"].as_array_mut().unwrap().push(block);
                } else {
                    out_msgs.push(json!({"role": "user", "content": [block]}));
                }
            }
            _ => {
                let text = m["content"].as_str().unwrap_or("");
                out_msgs.push(json!({"role": "user", "content": text}));
            }
        }
    }

    let anthropic_tools: Vec<Value> = tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f["name"],
                "description": f["description"].as_str().unwrap_or(""),
                "input_schema": f["parameters"],
            }))
        })
        .collect();

    let mut payload = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": out_msgs,
    });
    if !system_parts.is_empty() {
        payload["system"] = json!(system_parts.join("\n\n"));
    }
    if !anthropic_tools.is_empty() {
        payload["tools"] = json!(anthropic_tools);
    }
    payload
}

impl AgentModelBackend for AnthropicApiBackend {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::AnthropicApi
    }

    fn model_label(&self) -> &str {
        &self.model
    }

    fn turn<'a>(
        &'a mut self,
        messages: &'a [Value],
        tools: &'a [Value],
        max_tokens: u32,
        cancel: &'a Cancel,
        on_event: &'a mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'a>> {
        Box::pin(async move {
            let http = api_client()?;
            let body = convert_to_anthropic_payload(&self.model, messages, tools, max_tokens);
            let target = join_route(&self.base_url, "/v1/messages");

            let operation = async {
                let r = http
                    .post(target)
                    .header("x-api-key", self.api_key.expose())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await?;
                if !r.status().is_success() {
                    let code = r.status().as_u16();
                    let detail = r.text().await.unwrap_or_default();
                    bail!(
                        "Anthropic /v1/messages: HTTP {code}: {}",
                        detail.chars().take(240).collect::<String>()
                    );
                }

                let mut bytes = r.bytes_stream();
                let mut raw_bytes: Vec<u8> = Vec::new();
                let mut out = AgentTurnOutput::default();
                let mut partial_calls: BTreeMap<usize, ToolCall> = BTreeMap::new();

                while let Some(chunk) = bytes.next().await {
                    let chunk = chunk?;
                    for &b in &chunk {
                        if b != b'\r' {
                            raw_bytes.push(b);
                        }
                    }
                    while let Some(pos) = raw_bytes.windows(2).position(|w| w == b"\n\n") {
                        let block_bytes: Vec<u8> = raw_bytes.drain(..pos + 2).collect();
                        let block = String::from_utf8_lossy(&block_bytes);
                        for line in block.lines() {
                            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                                continue;
                            };
                            if data.is_empty() || data == "[DONE]" {
                                continue;
                            }
                            let Ok(v) = serde_json::from_str::<Value>(data) else {
                                continue;
                            };
                            match v["type"].as_str().unwrap_or("") {
                                "content_block_start" => {
                                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                                    let cb = &v["content_block"];
                                    if cb["type"].as_str() == Some("tool_use") {
                                        let id = cb["id"].as_str().unwrap_or("toolu_0").to_string();
                                        let name = cb["name"].as_str().unwrap_or("").to_string();
                                        partial_calls.insert(
                                            idx,
                                            ToolCall {
                                                id,
                                                name,
                                                arguments: String::new(),
                                            },
                                        );
                                    }
                                }
                                "content_block_delta" => {
                                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                                    let delta = &v["delta"];
                                    match delta["type"].as_str().unwrap_or("") {
                                        "text_delta" => {
                                            if let Some(txt) = delta["text"].as_str() {
                                                out.text.push_str(txt);
                                                on_event(StreamEvent::Text(txt.to_string()));
                                            }
                                        }
                                        "thinking_delta" => {
                                            if let Some(txt) = delta["thinking"].as_str() {
                                                on_event(StreamEvent::Reasoning(txt.to_string()));
                                            }
                                        }
                                        "input_json_delta" => {
                                            if let Some(part) = delta["partial_json"].as_str()
                                                && let Some(tc) = partial_calls.get_mut(&idx)
                                            {
                                                tc.arguments.push_str(part);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                "message_stop" => on_event(StreamEvent::Done),
                                "error" => {
                                    let msg = v["error"]["message"]
                                        .as_str()
                                        .unwrap_or("Anthropic SSE error");
                                    bail!("{msg}");
                                }
                                _ => {}
                            }
                        }
                    }
                }

                out.tool_calls = partial_calls
                    .into_values()
                    .filter(|tc| !tc.name.trim().is_empty())
                    .map(|mut tc| {
                        if tc.arguments.trim().is_empty() {
                            tc.arguments = "{}".into();
                        }
                        tc
                    })
                    .collect();
                if out.text.trim().is_empty() && out.tool_calls.is_empty() {
                    bail!("Anthropic API завершил шаг без текста и tool_use");
                }
                Ok(out)
            };

            tokio::select! {
                _ = cancel.cancelled() => bail!("Операция Anthropic API остановлена пользователем"),
                r = operation => r,
            }
        })
    }
}
