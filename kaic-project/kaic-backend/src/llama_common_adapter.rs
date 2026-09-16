//! Stable Rust-facing boundary for the project-owned llama.cpp common shim.

use anyhow::{anyhow, bail, Result};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

extern "C" {
    fn kaic_llama_common_schema_to_gbnf(
        schema_json: *const c_char,
        out_gbnf: *mut *mut c_char,
    ) -> i32;
    fn kaic_llama_common_prepare_chat(
        chat_template: *const c_char,
        messages_json: *const c_char,
        tools_json: *const c_char,
        out_json: *mut *mut c_char,
    ) -> i32;
    fn kaic_llama_common_parse_chat(
        text: *const c_char,
        tools_json: *const c_char,
        out_json: *mut *mut c_char,
    ) -> i32;
    fn kaic_llama_common_parse_prepared_chat(
        chat_template: *const c_char,
        messages_json: *const c_char,
        tools_json: *const c_char,
        text: *const c_char,
        out_json: *mut *mut c_char,
    ) -> i32;
    fn kaic_llama_common_free_string(value: *mut c_char);
}

struct OutputGuard(*mut c_char);
impl Drop for OutputGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { kaic_llama_common_free_string(self.0) };
        }
    }
}

fn call_string(
    operation: &str,
    input: &str,
    call: unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> i32,
) -> Result<String> {
    let input = CString::new(input).map_err(|_| anyhow!("input contains NUL"))?;
    let mut output = std::ptr::null_mut();
    let status = unsafe { call(input.as_ptr(), &mut output) };
    if status != 0 {
        bail!("llama common operation={operation} status={status}");
    }
    if output.is_null() {
        bail!("llama common adapter returned no output");
    }
    let guard = OutputGuard(output);
    let result = unsafe { CStr::from_ptr(guard.0) }
        .to_str()
        .map_err(|error| anyhow!("llama common operation={operation} status=utf8: {error}"))?
        .to_owned();
    Ok(result)
}

pub fn schema_to_gbnf(schema_json: &str) -> Result<String> {
    call_string(
        "schema_to_gbnf",
        schema_json,
        kaic_llama_common_schema_to_gbnf,
    )
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GrammarTrigger {
    #[serde(rename = "type")]
    pub kind: i32,
    pub value: String,
    pub token: i32,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PreparedChat {
    pub prompt: String,
    pub grammar: String,
    pub grammar_lazy: bool,
    pub generation_prompt: String,
    pub grammar_triggers: Vec<GrammarTrigger>,
    pub preserved_tokens: Vec<String>,
    pub additional_stops: Vec<String>,
}

fn ffi_json_call(operation: &str, inputs: &[&str], prepared: bool) -> Result<String> {
    let inputs = inputs
        .iter()
        .map(|input| CString::new(*input).map_err(|_| anyhow!("{operation}: input contains NUL")))
        .collect::<Result<Vec<_>>>()?;
    let mut output = std::ptr::null_mut();
    let status = unsafe {
        if prepared {
            kaic_llama_common_prepare_chat(
                inputs[0].as_ptr(),
                inputs[1].as_ptr(),
                inputs[2].as_ptr(),
                &mut output,
            )
        } else {
            kaic_llama_common_parse_prepared_chat(
                inputs[0].as_ptr(),
                inputs[1].as_ptr(),
                inputs[2].as_ptr(),
                inputs[3].as_ptr(),
                &mut output,
            )
        }
    };
    if status != 0 {
        bail!("llama common operation={operation} status={status}");
    }
    if output.is_null() {
        bail!("llama common operation={operation} status=missing_output");
    }
    let guard = OutputGuard(output);
    Ok(unsafe { CStr::from_ptr(guard.0) }
        .to_str()
        .map_err(|error| anyhow!("llama common operation={operation} status=utf8: {error}"))?
        .to_owned())
}

pub fn prepare_chat(
    chat_template: &str,
    messages_json: &str,
    tools_json: &str,
) -> Result<PreparedChat> {
    let output = ffi_json_call(
        "prepare_chat",
        &[chat_template, messages_json, tools_json],
        true,
    )?;
    serde_json::from_str(&output)
        .map_err(|error| anyhow!("llama common operation=prepare_chat status=json: {error}"))
}

pub fn parse_chat(text: &str, tools_json: &str) -> Result<serde_json::Value> {
    let text = CString::new(text).map_err(|_| anyhow!("parse_chat: input contains NUL"))?;
    let tools_json =
        CString::new(tools_json).map_err(|_| anyhow!("parse_chat: input contains NUL"))?;
    let mut output = std::ptr::null_mut();
    let status =
        unsafe { kaic_llama_common_parse_chat(text.as_ptr(), tools_json.as_ptr(), &mut output) };
    if status != 0 {
        bail!("llama common operation=parse_chat status={status}");
    }
    if output.is_null() {
        bail!("llama common operation=parse_chat status=missing_output");
    }
    let guard = OutputGuard(output);
    let output = unsafe { CStr::from_ptr(guard.0) }
        .to_str()
        .map_err(|error| anyhow!("llama common operation=parse_chat status=utf8: {error}"))?;
    serde_json::from_str(output)
        .map_err(|error| anyhow!("llama common operation=parse_chat status=json: {error}"))
}

pub fn parse_prepared_chat(
    chat_template: &str,
    messages_json: &str,
    tools_json: &str,
    text: &str,
) -> Result<serde_json::Value> {
    let output = ffi_json_call(
        "parse_prepared_chat",
        &[chat_template, messages_json, tools_json, text],
        false,
    )?;
    serde_json::from_str(&output)
        .map_err(|error| anyhow!("llama common operation=parse_prepared_chat status=json: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_chat, schema_to_gbnf};

    #[test]
    fn native_parser_returns_gptoss_peg_native_envelope_as_content() {
        let raw = r#"<｜channel｜>commentary to=functions.workspace_write_new_file<｜channel｜>commentary <｜constrain｜>json<｜message｜>{"path":"/home/kirpich/Документы/Test/Test.md","content":""}"#;
        let parsed = parse_chat(
            raw,
            r#"[{"type":"function","function":{"name":"workspace_write_new_file","description":"create","parameters":{"type":"object"}}}]"#,
        )
        .expect("llama common must return the exact GPTOSS20 envelope as assistant content");
        assert_eq!(parsed["tool_calls"], serde_json::Value::Null);
        assert_eq!(parsed["content"], raw);
    }

    #[test]
    fn malformed_schema_is_rejected_with_stable_operation_and_status() {
        let error =
            schema_to_gbnf("{ definitely not json").expect_err("malformed schema must be rejected");
        let message = error.to_string();
        assert!(message.contains("operation=schema_to_gbnf"), "{message}");
        assert!(message.contains("status="), "{message}");
    }

    #[test]
    fn schema_to_gbnf_uses_llama_common() {
        let grammar = schema_to_gbnf(
            r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}"#,
        )
        .expect("valid schema should produce grammar");
        assert!(grammar.contains("answer"));
        assert!(grammar.contains("root"));
    }
}
