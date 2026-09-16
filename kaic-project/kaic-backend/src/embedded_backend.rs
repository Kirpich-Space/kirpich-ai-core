//! Embedded llama.cpp backend.
//!
//! Models are kept resident, while a context is created for each generation.
//! This avoids a self-referential model/context structure and keeps the first
//! integration small and explicit. GPU offload is selected by the constructor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use encoding_rs::UTF_8;
use llama_cpp_4::prelude::*;
use tokio::sync::Mutex;

use crate::model_backend::{
    BackendCapability, BackendStatus, GenerateRequest, GenerateResponse, Message, ModelBackend,
    Role, ToolCall,
};

const CONTEXT_SIZE: u32 = 4096;
const BATCH_SIZE: usize = 4096;
const MAX_GENERATED_TOKENS: usize = 256;

fn requires_strict_tool_grammar(request: &GenerateRequest) -> bool {
    !request.tools.is_empty()
        && !request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
}

pub struct EmbeddedBackend {
    backend: LlamaBackend,
    model_path: PathBuf,
    gpu_layers: u32,
    models: Mutex<HashMap<String, LlamaModel>>,
}

impl EmbeddedBackend {
    pub fn new(model_dir: impl Into<PathBuf>, gpu_layers: u32) -> Result<Self> {
        let mut backend = LlamaBackend::init().context("не удалось инициализировать llama.cpp")?;
        // llama.cpp пишет подробный процесс загрузки напрямую в stderr, минуя
        // tracing. Ошибки по-прежнему возвращаются через Result, а native-шум
        // не должен смешиваться с ответом ассистента ни в terminal, ни в API.
        backend.void_logs();
        Ok(Self {
            backend,
            model_path: model_dir.into(),
            gpu_layers,
            models: Mutex::new(HashMap::new()),
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn gpu_layers(&self) -> u32 {
        self.gpu_layers
    }

    /// Освобождает все resident-модели. `LlamaBackend` завершит native
    /// runtime после уничтожения последнего экземпляра.
    pub async fn shutdown(&self) -> Result<()> {
        self.models.lock().await.clear();
        Ok(())
    }
}

/// Resolves an explicit GGUF path or a model name under the configured dir.
pub fn resolve_model_path(model: &str, model_dir: impl AsRef<Path>) -> PathBuf {
    let explicit = Path::new(model);
    if explicit.extension().is_some() || explicit.is_absolute() {
        explicit.to_path_buf()
    } else {
        model_dir.as_ref().join(format!("{model}.gguf"))
    }
}

fn build_chat_messages(messages: &[Message]) -> Result<Vec<LlamaChatMessage>> {
    messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            LlamaChatMessage::new(role.to_string(), message.content.clone())
                .context("не удалось построить chat message")
        })
        .collect()
}

fn common_wire_inputs(request: &GenerateRequest) -> Result<(String, String)> {
    let messages = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let mut value = serde_json::json!({
                "role": role,
                "content": message.content,
            });
            if let Some(tool_call_id) = &message.tool_call_id {
                value["tool_call_id"] = serde_json::Value::String(tool_call_id.clone());
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = serde_json::Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            value
        })
        .collect::<Vec<_>>();
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    Ok((
        serde_json::to_string(&messages)?,
        serde_json::to_string(&tools)?,
    ))
}

fn parse_common_response(
    parsed: serde_json::Value,
    allowed_tools: &[crate::model_backend::ToolSpec],
) -> Result<GenerateResponse> {
    let calls = parsed
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !calls.is_empty() {
        let mut result = Vec::with_capacity(calls.len());
        for call in calls {
            let function = call
                .get("function")
                .context("llama common вернул tool_call без function")?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .context("llama common вернул tool_call без имени")?;
            if !allowed_tools.iter().any(|tool| tool.name == name) {
                bail!("модель запросила невыданный инструмент: {name}");
            }
            let arguments = match function.get("arguments") {
                Some(serde_json::Value::String(arguments)) => arguments.clone(),
                Some(arguments) => serde_json::to_string(arguments)?,
                None => "{}".to_string(),
            };
            let id = call
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
            result.push(ToolCall {
                id,
                name: name.to_string(),
                arguments,
            });
        }
        return Ok(GenerateResponse::ToolCalls(result));
    }

    let text = parsed
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let text = strip_hidden_reasoning(text);
    if text.is_empty() {
        bail!("EmbeddedBackend получил пустой ответ от llama common parser");
    }
    Ok(GenerateResponse::Text(text))
}

/// Parses the exact peg-native envelope emitted by GPTOSS20 after a tool
/// result. This is deliberately a narrow fallback after llama.cpp common's
/// template parser rejected the same text: it accepts one canonical envelope,
/// one advertised function name, and a single JSON object only.
fn parse_gptoss_tool_envelope(
    output: &str,
    allowed_tools: &[crate::model_backend::ToolSpec],
) -> Result<GenerateResponse> {
    const PREFIX: &str = "<｜channel｜>commentary to=functions.";
    const JSON_MARKER: &str = "<｜channel｜>commentary <｜constrain｜>json<｜message｜>";

    let remainder = output
        .strip_prefix(PREFIX)
        .context("ответ не является GPTOSS20 tool envelope")?;
    let (name, arguments) = remainder
        .split_once(JSON_MARKER)
        .context("GPTOSS20 tool envelope не содержит JSON-маркер")?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("GPTOSS20 tool envelope содержит недопустимое имя инструмента");
    }
    let arguments: serde_json::Value = serde_json::from_str(arguments.trim())
        .context("GPTOSS20 tool envelope содержит невалидные JSON-аргументы")?;
    if !arguments.is_object() {
        bail!("GPTOSS20 tool envelope должен содержать JSON-объект аргументов");
    }
    parse_common_response(
        serde_json::json!({
            "tool_calls": [{
                "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]
        }),
        allowed_tools,
    )
}

fn oversized_grammar_repetition(grammar: &str) -> Option<usize> {
    let bytes = grammar.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            if let Some(end) = bytes[index + 1..].iter().position(|byte| *byte == b'}') {
                let end = index + 1 + end;
                let value = &grammar[index + 1..end];
                if let Ok(count) = value.parse::<usize>() {
                    if count > 8192 {
                        return Some(count);
                    }
                }
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    None
}

fn decode_piece(decoder: &mut encoding_rs::Decoder, bytes: &[u8], output: &mut String) {
    output.reserve(bytes.len() + 3);
    let _ = decoder.decode_to_string(bytes, output, false);
}

fn strip_tagged_section(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let lowered = text.to_ascii_lowercase();
    let mut clean = String::with_capacity(text.len());
    let mut cursor = 0;

    while cursor < text.len() {
        let next_open = lowered[cursor..].find(&open).map(|index| cursor + index);
        let next_close = lowered[cursor..].find(&close).map(|index| cursor + index);
        match (next_open, next_close) {
            (Some(open_start), Some(close_start)) if open_start < close_start => {
                clean.push_str(&text[cursor..open_start]);
                cursor = close_start + close.len();
            }
            (Some(open_start), None) => {
                clean.push_str(&text[cursor..open_start]);
                cursor = text.len();
            }
            (_, Some(close_start)) => {
                clean.push_str(&text[cursor..close_start]);
                cursor = close_start + close.len();
            }
            (None, None) => {
                clean.push_str(&text[cursor..]);
                break;
            }
        }
    }
    clean
}

fn strip_hidden_reasoning(text: &str) -> String {
    strip_tagged_section(&strip_tagged_section(text, "think"), "analysis")
        .trim()
        .to_string()
}

#[async_trait]
impl ModelBackend for EmbeddedBackend {
    async fn is_loaded(&self, model: &str) -> Result<bool> {
        Ok(self.models.lock().await.contains_key(model))
    }

    async fn loaded_instances(&self) -> Result<Vec<String>> {
        Ok(self.models.lock().await.keys().cloned().collect())
    }
    async fn load(&self, model: &str) -> Result<()> {
        self.load_with_path(model, None).await
    }

    async fn load_with_path(&self, model: &str, configured_path: Option<&str>) -> Result<()> {
        let mut models = self.models.lock().await;
        if models.contains_key(model) {
            return Ok(());
        }

        let path = configured_path.map(PathBuf::from).unwrap_or_else(|| {
            if self.model_path.is_file()
                || self.model_path.extension().is_some()
                || self.model_path.is_absolute() && !self.model_path.is_dir()
            {
                self.model_path.clone()
            } else {
                resolve_model_path(model, &self.model_path)
            }
        });
        if !path.is_file() {
            bail!("модель не найдена: {}", path.display());
        }

        let params = LlamaModelParams::default().with_n_gpu_layers(self.gpu_layers);
        let loaded = LlamaModel::load_from_file(&self.backend, &path, &params)
            .with_context(|| format!("не удалось загрузить GGUF: {}", path.display()))?;
        models.insert(model.to_string(), loaded);
        Ok(())
    }

    async fn unload(&self, model: &str) -> Result<()> {
        self.models.lock().await.remove(model);
        Ok(())
    }

    async fn generate(&self, model: &str, request: GenerateRequest) -> Result<GenerateResponse> {
        let models = self.models.lock().await;
        let loaded = models
            .get(model)
            .with_context(|| format!("модель не загружена: {model}"))?;

        let mut common_context = None;
        let prompt = if request.tools.is_empty() {
            let messages = build_chat_messages(&request.messages)?;
            loaded
                .apply_chat_template(None, &messages, true)
                .context("не удалось применить chat template")?
        } else {
            let template = loaded
                .meta_val_str("tokenizer.chat_template", 1_048_576)
                .context("GGUF не содержит читаемый tokenizer.chat_template для tool calling")?;
            let (messages_json, tools_json) = common_wire_inputs(&request)?;
            let prepared =
                crate::llama_common_adapter::prepare_chat(&template, &messages_json, &tools_json)
                    .context("llama common не смог подготовить tool-aware prompt")?;
            if let Some(repetition) = oversized_grammar_repetition(&prepared.grammar) {
                bail!("llama common вернул неподдерживаемое повторение grammar: {repetition}");
            }
            let prompt = prepared.prompt.clone();
            common_context = Some((template, messages_json, tools_json, prepared));
            prompt
        };
        let tokens = loaded
            .str_to_token(&prompt, AddBos::Always)
            .context("не удалось токенизировать prompt")?;
        if tokens.is_empty()
            || tokens.len() >= BATCH_SIZE
            || tokens.len().saturating_add(MAX_GENERATED_TOKENS) > CONTEXT_SIZE as usize
        {
            bail!(
                "prompt имеет неподдерживаемый размер: {} токенов (лимит с запасом генерации: {})",
                tokens.len(),
                CONTEXT_SIZE as usize - MAX_GENERATED_TOKENS
            );
        }

        let context_params =
            LlamaContextParams::default().with_n_ctx(std::num::NonZeroU32::new(CONTEXT_SIZE));
        let mut context = loaded
            .new_context(&self.backend, context_params)
            .context("не удалось создать llama context")?;
        let mut batch = LlamaBatch::new(BATCH_SIZE, 1);
        for (position, token) in tokens.iter().enumerate() {
            batch
                .add(*token, position as i32, &[0], position + 1 == tokens.len())
                .context("не удалось добавить prompt token")?;
        }
        context.decode(&mut batch).context("ошибка decode prompt")?;

        let temperature = request.temperature.unwrap_or(0.8).max(0.0);
        let strict_tool_grammar = requires_strict_tool_grammar(&request);
        let mut sampler_chain = Vec::new();
        if strict_tool_grammar {
            if let Some((_, _, _, prepared)) = &common_context {
                if !prepared.grammar.is_empty() {
                    // The initial agent turn must start with a structured tool envelope.
                    // After a tool result, leave grammar disabled so the model can either
                    // request another tool or finish with a normal answer.
                    let grammar = LlamaSampler::grammar(loaded, &prepared.grammar, "root");
                    sampler_chain.push(grammar);
                }
            }
        }
        if temperature == 0.0 {
            sampler_chain.push(LlamaSampler::greedy());
        } else {
            sampler_chain.push(LlamaSampler::temp(temperature));
            sampler_chain.push(LlamaSampler::dist(0));
        }
        let mut sampler = LlamaSampler::chain_simple(sampler_chain);
        if let Some((_, _, _, prepared)) = &common_context {
            if !prepared.grammar_lazy && !prepared.generation_prompt.is_empty() {
                let prefill = loaded
                    .str_to_token(&prepared.generation_prompt, AddBos::Never)
                    .context("не удалось токенизировать grammar prefill")?;
                sampler.accept_many(&prefill);
            }
            // Accessing the list here is deliberate: llama common exposes
            // preserved delimiters for tokenizer implementations that support
            // custom preservation. llama-cpp-4 tokenizes special tokens as
            // special already, so no second conversion is required.
            let _ = &prepared.preserved_tokens;
        }
        let mut decoder = UTF_8.new_decoder();
        let mut output = String::new();

        // Tool-call envelopes are intentionally bounded: a valid call is
        // short, while an unconstrained reasoning continuation can consume
        // minutes on CPU before the common parser ever sees a delimiter.
        let generation_limit = if strict_tool_grammar {
            64
        } else {
            MAX_GENERATED_TOKENS
        };
        for (generated, _) in (0..generation_limit).enumerate() {
            let position = tokens.len() as i32 + generated as i32;
            let token = sampler.sample(&context, batch.n_tokens() - 1);
            if loaded.is_eog_token(token) {
                break;
            }
            let bytes = loaded
                // Tool-call delimiters are special/control tokens. Preserve them
                // so llama common can parse the generated assistant message.
                .token_to_raw_bytes(token, Special::Tokenize)
                .context("не удалось декодировать token")?;
            decode_piece(&mut decoder, &bytes, &mut output);
            let matched_stop = common_context.as_ref().and_then(|(_, _, _, prepared)| {
                prepared
                    .additional_stops
                    .iter()
                    .find(|stop| !stop.is_empty() && output.ends_with(stop.as_str()))
                    .map(String::len)
            });
            if let Some(stop_len) = matched_stop {
                output.truncate(output.len() - stop_len);
                break;
            }

            batch.clear();
            batch
                .add(token, position, &[0], true)
                .context("не удалось добавить generated token")?;
            context
                .decode(&mut batch)
                .context("ошибка decode generated token")?;
        }

        if let Some((template, messages_json, tools_json, _)) = common_context {
            let parsed = crate::llama_common_adapter::parse_prepared_chat(
                &template,
                &messages_json,
                &tools_json,
                &output,
            );
            return match parsed {
                Ok(parsed) => parse_common_response(parsed, &request.tools),
                Err(common_error) => parse_gptoss_tool_envelope(&output, &request.tools)
                    .with_context(|| {
                        format!("llama common не смог разобрать ответ модели: {common_error:#}")
                    }),
            };
        }

        let output = strip_hidden_reasoning(&output);
        if output.is_empty() {
            bail!("EmbeddedBackend получил пустой ответ от модели");
        }
        Ok(GenerateResponse::Text(output))
    }

    fn backend_name(&self) -> &'static str {
        "embedded"
    }

    fn capabilities(&self) -> crate::model_backend::BackendCapabilities {
        crate::model_backend::BackendCapabilities {
            tool_calling: BackendCapability::Supported,
        }
    }

    fn backend_config(&self) -> Option<String> {
        Some(format!(
            "model_path={}, gpu_layers={}",
            self.model_path().display(),
            self.gpu_layers()
        ))
    }

    async fn runtime_status(&self) -> Result<BackendStatus> {
        let models = self.models.lock().await;
        let mut loaded_resident_models: Vec<String> = models.keys().cloned().collect();
        loaded_resident_models.sort();

        let mut devices = Vec::new();
        for (model_name, model) in models.iter() {
            for device in model.devices() {
                let name = device.name().unwrap_or("unknown");
                devices.push(format!("{model_name}: {name} ({:?})", device.device_type()));
            }
        }
        devices.sort();

        Ok(BackendStatus {
            healthy: true,
            initialized: true,
            loaded_resident_models,
            configured_gpu_layers: Some(self.gpu_layers),
            device_offload_summary: (!devices.is_empty()).then(|| devices.join(", ")),
            capabilities: self.capabilities(),
            cancellation_supported: false,
            timeout_supported: false,
        })
    }

    async fn shutdown(&self) -> Result<()> {
        EmbeddedBackend::shutdown(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_chat_messages, oversized_grammar_repetition, parse_common_response,
        resolve_model_path, strip_hidden_reasoning,
    };
    use crate::model_backend::{
        BackendCapability, GenerateRequest, GenerateResponse, Message, ModelBackend, Role, ToolSpec,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;

    static BACKEND_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn strict_tool_grammar_is_disabled_after_a_tool_result() {
        let request = GenerateRequest {
            messages: vec![
                Message::new(Role::User, "create a directory"),
                Message::tool_result("call-1", "Каталог существует: demo"),
            ],
            temperature: None,
            tools: vec![ToolSpec {
                name: "workspace_inspect_path".to_string(),
                description: "inspect".to_string(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
        };
        assert!(!super::requires_strict_tool_grammar(&request));
    }

    #[test]
    fn gptoss_post_tool_envelope_is_a_validated_tool_call() {
        let tools = vec![ToolSpec {
            name: "workspace_write_new_file".to_string(),
            description: "create".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        }];
        let response = super::parse_gptoss_tool_envelope(
            r#"<｜channel｜>commentary to=functions.workspace_write_new_file<｜channel｜>commentary <｜constrain｜>json<｜message｜>{"path":"/home/kirpich/Документы/Test/Test.md","content":""}"#,
            &tools,
        )
        .expect("the exact GPTOSS20 envelope must become a validated call");
        let GenerateResponse::ToolCalls(calls) = response else {
            panic!("expected a tool call");
        };
        assert_eq!(calls[0].name, "workspace_write_new_file");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&calls[0].arguments).unwrap(),
            serde_json::json!({"path":"/home/kirpich/Документы/Test/Test.md","content":""})
        );
    }

    #[test]
    fn oversized_grammar_repetition_is_rejected_before_sampler_creation() {
        assert_eq!(
            oversized_grammar_repetition("value ::= char{1048576}"),
            Some(1048576)
        );
        assert_eq!(oversized_grammar_repetition("value ::= char{8192}"), None);
        assert_eq!(oversized_grammar_repetition("value ::= char+"), None);
    }

    #[test]
    fn hidden_reasoning_is_removed_from_the_model_reply() {
        assert_eq!(
            strip_hidden_reasoning("<think>internal plan</think>\n\nПривет!"),
            "Привет!"
        );
        assert_eq!(
            strip_hidden_reasoning("До <think>secret</think> после"),
            "До  после"
        );
        assert_eq!(
            strip_hidden_reasoning("<THINK>unfinished private reasoning"),
            ""
        );
        assert_eq!(
            strip_hidden_reasoning("</think>Готовый ответ"),
            "Готовый ответ"
        );
        assert_eq!(
            strip_hidden_reasoning("<analysis>private</analysis>Чистый ответ"),
            "Чистый ответ"
        );
    }

    #[test]
    fn explicit_existing_path_wins() {
        let path = PathBuf::from("/tmp/model.gguf");
        assert_eq!(resolve_model_path("/tmp/model.gguf", "/models"), path);
    }

    #[test]
    fn model_name_resolves_under_configured_directory() {
        assert_eq!(
            resolve_model_path("Fable9B", "/models"),
            PathBuf::from("/models/Fable9B.gguf")
        );
    }

    #[test]
    fn chat_messages_preserve_roles_and_content() {
        let messages = vec![
            Message::new(Role::System, "system"),
            Message::new(Role::User, "question"),
            Message::new(Role::Assistant, "answer"),
        ];
        let chat = build_chat_messages(&messages).expect("valid chat messages");
        assert_eq!(chat.len(), 3);
    }

    #[test]
    fn embedded_backend_reports_native_tool_calling_support() {
        let _guard = BACKEND_TEST_LOCK.lock().unwrap();
        let backend = super::EmbeddedBackend::new("/tmp/models", 0)
            .expect("llama backend initialization must succeed without loading a model");
        assert_eq!(
            backend.capabilities().tool_calling,
            BackendCapability::Supported
        );
    }

    #[test]
    fn common_parser_output_becomes_a_validated_tool_call() {
        let tools = vec![ToolSpec {
            name: "workspace_create_directory".to_string(),
            description: "create".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let response = parse_common_response(
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "workspace_create_directory",
                        "arguments": {"path": "demo"}
                    }
                }]
            }),
            &tools,
        )
        .expect("advertised call must parse");
        match response {
            GenerateResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "workspace_create_directory");
                assert_eq!(calls[0].arguments, r#"{"path":"demo"}"#);
            }
            GenerateResponse::Text(_) => panic!("expected tool call"),
        }
    }

    #[test]
    fn common_parser_cannot_invent_an_unadvertised_tool() {
        let error = match parse_common_response(
            serde_json::json!({
                "tool_calls": [{"function": {"name": "shell", "arguments": {}}}]
            }),
            &[],
        ) {
            Ok(_) => panic!("unadvertised calls must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("невыданный инструмент"));
    }

    #[tokio::test]
    async fn runtime_status_reports_safe_embedded_configuration() {
        let _guard = BACKEND_TEST_LOCK.lock().unwrap();
        let backend = super::EmbeddedBackend::new("/tmp/models", 35)
            .expect("llama backend initialization must succeed without loading a model");
        let status = backend
            .runtime_status()
            .await
            .expect("runtime metrics must be readable");

        assert!(status.healthy);
        assert!(status.initialized);
        assert!(status.loaded_resident_models.is_empty());
        assert_eq!(status.configured_gpu_layers, Some(35));
        assert_eq!(status.device_offload_summary, None);
        assert!(!status.cancellation_supported);
        assert!(!status.timeout_supported);
    }

    #[tokio::test]
    async fn explicit_model_path_returns_normal_error() {
        let _guard = BACKEND_TEST_LOCK.lock().unwrap();
        let backend = super::EmbeddedBackend::new("/tmp/models", 0)
            .expect("llama backend initialization must succeed without loading a model");
        let error = backend
            .load_with_path(
                "Fable9B",
                Some("/tmp/kaic-explicit-model-that-must-not-exist.gguf"),
            )
            .await
            .expect_err("missing explicit GGUF must be reported as an error");
        assert!(error.to_string().contains("модель не найдена"));
    }

    #[tokio::test]
    async fn invalid_model_path_returns_normal_error() {
        let _guard = BACKEND_TEST_LOCK.lock().unwrap();
        let backend =
            super::EmbeddedBackend::new("/tmp/kaic-model-that-must-not-exist-20260908.gguf", 0)
                .expect("llama backend initialization must succeed without loading a model");
        let error = ModelBackend::load(&backend, "Fable9B")
            .await
            .expect_err("missing GGUF must be reported as an error");
        assert!(error.to_string().contains("модель не найдена"));
    }
}
