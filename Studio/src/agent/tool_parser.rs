//! Моделезависимые адаптеры вызовов инструментов (Qwen/Hermes/Llama/Mistral),
//! детерминированное восстановление повреждённого JSON, валидация схемы и генератор GBNF-грамматик.
use crate::llama::ToolCall;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDialect {
    Auto,
    OpenAiNative,
    QwenXml,
    HermesTag,
    LlamaJson,
    MistralBracket,
}

/// Определяет предпочтительный диалект tool calling по имени модели.
pub fn detect_dialect(model_name: &str) -> ToolDialect {
    let m = model_name.to_ascii_lowercase();
    if m.contains("qwen") {
        ToolDialect::QwenXml
    } else if m.contains("hermes") {
        ToolDialect::HermesTag
    } else if m.contains("mistral") || m.contains("ministral") || m.contains("devstral") {
        ToolDialect::MistralBracket
    } else if m.contains("llama")
        || m.contains("granite")
        || m.contains("gemma")
        || m.contains("glm")
    {
        ToolDialect::LlamaJson
    } else {
        ToolDialect::Auto
    }
}

/// Извлекает вызовы инструментов из текстового ответа локальных моделей, когда сервер
/// не вернул нативный OpenAI `tool_calls`. Исполняются ТОЛЬКО имена из `allowed_tools`;
/// произвольный текст никогда не трактуется как инструмент.
pub fn extract_fallback_tool_calls(
    model_name: &str,
    text: &str,
    allowed_tools: &[&str],
) -> Vec<ToolCall> {
    if text.trim().is_empty() || allowed_tools.is_empty() {
        return Vec::new();
    }
    let dialect = detect_dialect(model_name);
    let mut out = Vec::new();

    // 1. Qwen / Hermes: <tool_call>...</tool_call>
    if matches!(
        dialect,
        ToolDialect::Auto | ToolDialect::QwenXml | ToolDialect::HermesTag | ToolDialect::LlamaJson
    ) {
        extract_xml_tool_calls(text, allowed_tools, &mut out);
        if !out.is_empty() {
            return out;
        }
    }

    // 2. Mistral: [TOOL_CALLS] [...] или [TOOL_CALLS]name{...}
    if matches!(
        dialect,
        ToolDialect::Auto | ToolDialect::MistralBracket | ToolDialect::LlamaJson
    ) {
        extract_mistral_calls(text, allowed_tools, &mut out);
        if !out.is_empty() {
            return out;
        }
    }

    // 3. Llama / Granite: <function=name>{...}</function>
    if matches!(dialect, ToolDialect::Auto | ToolDialect::LlamaJson) {
        extract_llama_function_tags(text, allowed_tools, &mut out);
        if !out.is_empty() {
            return out;
        }
    }

    // 4. Явный ```json ... ``` блок или целиком JSON-объект ответа { "name": "...", "arguments": {...} }
    extract_fenced_or_whole_json(text, allowed_tools, &mut out);
    out
}

fn push_if_allowed(val: &Value, allowed_tools: &[&str], out: &mut Vec<ToolCall>) {
    let Some(obj) = val.as_object() else {
        return;
    };
    let Some(name) = obj
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("tool").and_then(|v| v.as_str()))
    else {
        return;
    };
    let trimmed_name = name.trim();
    if !allowed_tools.contains(&trimmed_name) {
        return;
    }
    let args_val = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("args"));
    let arguments = match args_val {
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
        None => "{}".into(),
    };
    let idx = out.len();
    out.push(ToolCall {
        id: obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("fallback_call_{idx}")),
        name: trimmed_name.to_string(),
        arguments,
    });
}

fn extract_xml_tool_calls(text: &str, allowed_tools: &[&str], out: &mut Vec<ToolCall>) {
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        let after_start = &rest[start + "<tool_call>".len()..];
        let Some(end) = after_start.find("</tool_call>") else {
            break;
        };
        let inner = after_start[..end].trim();
        rest = &after_start[end + "</tool_call>".len()..];
        if let Ok(v) = parse_relaxed_json_value(inner) {
            if let Some(arr) = v.as_array() {
                for item in arr {
                    push_if_allowed(item, allowed_tools, out);
                }
            } else {
                push_if_allowed(&v, allowed_tools, out);
            }
        }
    }
}

fn extract_mistral_calls(text: &str, allowed_tools: &[&str], out: &mut Vec<ToolCall>) {
    let Some(pos) = text.find("[TOOL_CALLS]") else {
        return;
    };
    let body = text[pos + "[TOOL_CALLS]".len()..].trim();
    if let Ok(v) = parse_relaxed_json_value(body) {
        if let Some(arr) = v.as_array() {
            for item in arr {
                push_if_allowed(item, allowed_tools, out);
            }
        } else {
            push_if_allowed(&v, allowed_tools, out);
        }
        return;
    }
    // Формат Mistral v3+: [TOOL_CALLS]read_file[ARGS]{"path":"a.py"}
    if let Some((raw_name, raw_args)) = body.split_once("[ARGS]") {
        let name = raw_name.trim();
        if allowed_tools.contains(&name) {
            let idx = out.len();
            out.push(ToolCall {
                id: format!("mistral_call_{idx}"),
                name: name.to_string(),
                arguments: raw_args.trim().to_string(),
            });
        }
    }
}

fn extract_llama_function_tags(text: &str, allowed_tools: &[&str], out: &mut Vec<ToolCall>) {
    let mut rest = text;
    while let Some(start) = rest.find("<function=") {
        let after = &rest[start + "<function=".len()..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let name = after[..gt].trim();
        let after_gt = &after[gt + 1..];
        let Some(end) = after_gt.find("</function>") else {
            break;
        };
        let args_str = after_gt[..end].trim();
        rest = &after_gt[end + "</function>".len()..];
        if allowed_tools.contains(&name) {
            let idx = out.len();
            out.push(ToolCall {
                id: format!("llama_call_{idx}"),
                name: name.to_string(),
                arguments: if args_str.is_empty() {
                    "{}".into()
                } else {
                    args_str.to_string()
                },
            });
        }
    }
}

fn extract_fenced_or_whole_json(text: &str, allowed_tools: &[&str], out: &mut Vec<ToolCall>) {
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after_fence = &rest[start + 3..];
        let Some(nl) = after_fence.find('\n') else {
            break;
        };
        let lang = after_fence[..nl].trim().to_ascii_lowercase();
        let after_header = &after_fence[nl + 1..];
        let Some(end) = after_header.find("```") else {
            break;
        };
        let block = after_header[..end].trim();
        rest = &after_header[end + 3..];
        if (lang.is_empty() || lang == "json" || lang == "tool_call")
            && let Ok(v) = parse_relaxed_json_value(block)
        {
            if let Some(arr) = v.as_array() {
                for item in arr {
                    push_if_allowed(item, allowed_tools, out);
                }
            } else {
                push_if_allowed(&v, allowed_tools, out);
            }
        }
    }
    if !out.is_empty() {
        return;
    }
    // Целиком JSON-ответ (без окружающего разговорного текста)
    let trimmed = text.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        if let Ok(v) = parse_relaxed_json_value(trimmed) {
            if let Some(arr) = v.as_array() {
                for item in arr {
                    push_if_allowed(item, allowed_tools, out);
                }
            } else {
                push_if_allowed(&v, allowed_tools, out);
            }
        }
    }
}

/// Убирает завершающие запятые перед `}` и `]` с учётом строковых литералов.
fn strip_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    while i < bytes.len() {
        let ch = input[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        if in_str {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            i += ch_len;
            continue;
        }
        if ch == '"' {
            in_str = true;
            out.push(ch);
            i += ch_len;
            continue;
        }
        if ch == ',' {
            let rest = input[i + ch_len..].trim_start();
            if rest.starts_with('}') || rest.starts_with(']') {
                i += ch_len;
                continue;
            }
        }
        out.push(ch);
        i += ch_len;
    }
    out
}

fn strip_markdown_fences(raw: &str) -> String {
    let t = raw.trim();
    if let Some(after) = t.strip_prefix("```") {
        if let Some(nl) = after.find('\n') {
            let body = &after[nl + 1..];
            if let Some(end) = body.rfind("```") {
                return body[..end].trim().to_string();
            }
            return body.trim().to_string();
        }
    }
    t.to_string()
}

fn balance_closing_delimiters(raw: &str) -> String {
    let mut stack = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.last().copied() == Some(ch) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    if stack.is_empty() || stack.len() > 2 || in_str {
        return raw.to_string();
    }
    let mut repaired = raw.to_string();
    while let Some(close) = stack.pop() {
        repaired.push(close);
    }
    repaired
}

fn parse_relaxed_json_value(raw: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        if let Value::String(inner) = v {
            return parse_relaxed_json_value(&inner);
        }
        return Ok(v);
    }
    let unfenced = strip_markdown_fences(raw);
    if let Ok(v) = serde_json::from_str::<Value>(&unfenced) {
        return Ok(v);
    }
    let balanced = balance_closing_delimiters(&unfenced);
    let balanced_no_trailing = strip_trailing_commas(&balanced);
    if let Ok(v) = serde_json::from_str::<Value>(&balanced_no_trailing) {
        return Ok(v);
    }
    let no_trailing = strip_trailing_commas(&unfenced);
    if let Ok(v) = serde_json::from_str::<Value>(&no_trailing) {
        return Ok(v);
    }
    // Если вокруг одного JSON-объекта остался посторонний текст (например комментарий модели)
    if let (Some(l), Some(r)) = (no_trailing.find('{'), no_trailing.rfind('}'))
        && r > l
    {
        let slice = &no_trailing[l..=r];
        if let Ok(v) = serde_json::from_str::<Value>(slice) {
            return Ok(v);
        }
        let slice_no_comma = strip_trailing_commas(slice);
        if let Ok(v) = serde_json::from_str::<Value>(&slice_no_comma) {
            return Ok(v);
        }
    }
    Err(format!(
        "Не удалось разобрать JSON аргументов: {}",
        raw.chars().take(120).collect::<String>()
    ))
}

/// Восстанавливает безопасные синтаксические дефекты JSON аргументов (fence, двойное
/// экранирование, хвостовые запятые, нехватка 1-2 закрывающих скобок) и валидирует
/// объект аргументов по JSON Schema определения инструмента (`required` + `properties.*.type`).
pub fn repair_and_validate_args(
    tool_name: &str,
    raw_args: &str,
    tool_defs: &[Value],
) -> Result<Value, String> {
    let parsed = if raw_args.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        parse_relaxed_json_value(raw_args)?
    };
    let Some(obj) = parsed.as_object() else {
        return Err(format!(
            "Аргументы инструмента {tool_name} должны быть JSON-объектом, получено: {parsed}"
        ));
    };

    let Some(def) = tool_defs
        .iter()
        .find(|d| d["function"]["name"].as_str() == Some(tool_name))
    else {
        return Err(format!("Неизвестный инструмент: {tool_name}"));
    };
    let params = &def["function"]["parameters"];
    if let Some(required) = params["required"].as_array() {
        for req in required {
            let Some(key) = req.as_str() else { continue };
            if !obj.contains_key(key) || obj[key].is_null() {
                return Err(format!(
                    "Отсутствует обязательный параметр «{key}» для инструмента {tool_name}"
                ));
            }
        }
    }
    if let Some(props) = params["properties"].as_object() {
        for (k, v) in obj {
            if let Some(schema) = props.get(k)
                && let Some(expected_type) = schema["type"].as_str()
            {
                let type_ok = match expected_type {
                    "string" => v.is_string(),
                    "integer" => v.is_i64() || v.is_u64(),
                    "number" => v.is_number(),
                    "boolean" => v.is_boolean(),
                    "array" => v.is_array(),
                    "object" => v.is_object(),
                    _ => true,
                };
                if !type_ok {
                    return Err(format!(
                        "Параметр «{k}» инструмента {tool_name} должен иметь тип {expected_type}"
                    ));
                }
            }
        }
    }
    Ok(Value::Object(obj.clone()))
}

/// Генерирует GBNF-грамматику для llama.cpp, ограничивающую вывод структурированным
/// вызовом одного из разрешённых инструментов `{"name": "<tool>", "arguments": {...}}`.
pub fn build_tool_call_gbnf(tool_names: &[&str]) -> String {
    let names_rule = if tool_names.is_empty() {
        "\"\\\"unknown\\\"\"".to_string()
    } else {
        tool_names
            .iter()
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .map(|n| format!("\"\\\"{n}\\\"\""))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    format!(
        "root ::= ws \"{{\\n\"? ws \"\\\"name\\\"\" ws \":\" ws tool-name ws \",\" ws \"\\\"arguments\\\"\" ws \":\" ws obj ws \"}}\" ws\n\
         tool-name ::= {names_rule}\n\
         obj ::= \"{{\" ws (pair (ws \",\" ws pair)*)? ws \"}}\"\n\
         pair ::= string ws \":\" ws value\n\
         arr ::= \"[\" ws (value (ws \",\" ws value)*)? ws \"]\"\n\
         value ::= obj | arr | string | number | \"true\" | \"false\" | \"null\"\n\
         string ::= \"\\\"\" ([^\"\\\\\\x7F\\x00-\\x1F] | \"\\\\\" ([\"\\\\bfnrt/] | \"u\" [0-9a-fA-F]{{4}}))* \"\\\"\"\n\
         number ::= \"-\"? ([0-9] | [1-9][0-9]*) (\".\" [0-9]+)? ([eE] [+-]? [0-9]+)?\n\
         ws ::= [ \\t\\n\\r]*"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::tool_definitions;

    #[test]
    fn extracts_qwen_hermes_llama_mistral_and_ignores_plain_prose() {
        let allowed = ["read_file", "edit_file", "run_check"];

        let qwen = "Думаю...\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n</tool_call>";
        let qc = extract_fallback_tool_calls("Qwen2.5-Coder", qwen, &allowed);
        assert_eq!(qc.len(), 1);
        assert_eq!(qc[0].name, "read_file");

        let llama = "<function=edit_file>{\"path\":\"a.py\",\"old_text\":\"1\",\"new_text\":\"2\"}</function>";
        let lc = extract_fallback_tool_calls("Llama-3.2", llama, &allowed);
        assert_eq!(lc.len(), 1);
        assert_eq!(lc[0].name, "edit_file");

        let mistral = "[TOOL_CALLS] [{\"name\":\"run_check\",\"arguments\":{\"program\":\"pytest\",\"args\":[]}}]";
        let mc = extract_fallback_tool_calls("Mistral-Small", mistral, &allowed);
        assert_eq!(mc.len(), 1);
        assert_eq!(mc[0].name, "run_check");

        // Произвольный текст или запрещённое имя инструмента НЕ парсятся
        let prose = "Я бы вызвал read_file(path='a.py') и rm_rf()";
        assert!(extract_fallback_tool_calls("Auto", prose, &allowed).is_empty());
        let disallowed =
            "<tool_call>{\"name\":\"shell_exec\",\"arguments\":{\"cmd\":\"id\"}}</tool_call>";
        assert!(extract_fallback_tool_calls("Qwen", disallowed, &allowed).is_empty());
    }

    #[test]
    fn repairs_malformed_json_and_rejects_missing_required_fields() {
        let defs = tool_definitions(false);
        // Trailing comma + fence + missing brace -> repaired
        let repaired = repair_and_validate_args(
            "read_file",
            "```json\n{\"path\": \"lib.rs\", \"start_line\": 1,\n```",
            &defs,
        )
        .unwrap();
        assert_eq!(repaired["path"], "lib.rs");
        assert_eq!(repaired["start_line"], 1);

        // Trailing comma with string containing `,}` is NOT mangled
        let s = repair_and_validate_args(
            "edit_file",
            "{\"path\":\"a.rs\",\"old_text\":\"a,}\",\"new_text\":\"b\",}",
            &defs,
        )
        .unwrap();
        assert_eq!(s["old_text"], "a,}");

        // Double-encoded string -> unwrapped
        let inner_quoted = serde_json::to_string("{\"path\":\"x.py\"}").unwrap();
        let unwrapped = repair_and_validate_args("read_file", &inner_quoted, &defs).unwrap();
        assert_eq!(unwrapped["path"], "x.py");

        // Missing required field -> error (does not fabricate)
        assert!(repair_and_validate_args("read_file", "{}", &defs).is_err());
        // Wrong parameter type -> error
        assert!(repair_and_validate_args("read_file", "{\"path\": 123}", &defs).is_err());
    }

    #[test]
    fn generates_valid_gbnf_grammar_for_tools() {
        let g = build_tool_call_gbnf(&["read_file", "apply_patch"]);
        assert!(g.contains("root ::="));
        assert!(g.contains("\"\\\"read_file\\\"\""));
        assert!(g.contains("\"\\\"apply_patch\\\"\""));
    }
}
