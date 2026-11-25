//! Управление контекстным окном и детерминированная компакция длинной истории агента.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMetrics {
    pub used_tokens: usize,
    pub context_limit: usize,
    pub utilization_pct: u8,
    pub compactions: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub dropped_messages: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub summary: String,
}

/// Консервативная UTF-8-совместимая оценка токенов (учитывает кириллицу и JSON-инструменты).
pub fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            let s = m.to_string();
            // Для кириллицы и JSON-структур 1 токен ~ 3 байта/символа в консервативную сторону
            s.chars().count() / 3 + 12
        })
        .sum()
}

pub fn context_metrics(
    messages: &[Value],
    context_limit: usize,
    compactions: u32,
) -> ContextMetrics {
    let used_tokens = estimate_messages_tokens(messages);
    let limit = context_limit.max(512);
    let pct = ((used_tokens.saturating_mul(100)) / limit).min(100) as u8;
    ContextMetrics {
        used_tokens,
        context_limit: limit,
        utilization_pct: pct,
        compactions,
    }
}

/// Автоматическая или ручная (`/compact`) компакция истории:
/// - Сохраняет `messages[0]` (system prompt), первое сообщение пользователя (цель задачи) и
///   последние `keep_tail` сообщений без изменений;
/// - Промежуточные шаги сворачивает в одно системное сообщение `[COMPACTED CONTEXT]` с
///   перечнем затронутых файлов, ошибок и результата последней проверки.
pub fn compact_wire_messages(
    messages: &mut Vec<Value>,
    context_limit: usize,
    changed_files: &[String],
    last_check_summary: Option<&str>,
    force: bool,
) -> Option<CompactionSummary> {
    let limit = context_limit.max(512);
    let tokens_before = estimate_messages_tokens(messages);
    let threshold = (limit * 78) / 100;
    if !force && tokens_before < threshold {
        return None;
    }

    // Определяем префикс: system[0] + первый user message с постановкой задачи
    let first_user_idx = messages
        .iter()
        .position(|m| m["role"].as_str() == Some("user"));
    let prefix_end = first_user_idx
        .map(|i| i + 1)
        .unwrap_or(1)
        .min(messages.len());
    let keep_tail = 6usize;

    if messages.len() <= prefix_end + keep_tail + 1 {
        return None;
    }

    let middle_end = messages.len() - keep_tail;
    let middle = &messages[prefix_end..middle_end];
    let dropped_messages = middle.len();

    let mut tool_calls_seen = 0usize;
    let mut errors_noted: Vec<String> = Vec::new();
    for msg in middle {
        if let Some(arr) = msg["tool_calls"].as_array() {
            tool_calls_seen += arr.len();
        }
        if msg["role"].as_str() == Some("tool")
            && let Some(c) = msg["content"].as_str()
            && (c.contains("ошибка")
                || c.contains("error")
                || c.contains("FAIL")
                || c.contains("exit_code=") && !c.contains("exit_code=0"))
            && errors_noted.len() < 4
        {
            let first_line = c.lines().next().unwrap_or(c);
            let clipped: String = first_line.chars().take(140).collect();
            errors_noted.push(clipped);
        }
    }

    let mut summary = format!(
        "[COMPACTED CONTEXT] Сжато сообщений: {dropped_messages}; вызовов инструментов в сжатом отрезке: {tool_calls_seen}."
    );
    if !changed_files.is_empty() {
        summary.push_str(&format!(
            "\nИзменённые файлы: {}.",
            changed_files.join(", ")
        ));
    }
    if let Some(chk) = last_check_summary {
        summary.push_str(&format!("\nПоследняя проверка: {chk}."));
    }
    if !errors_noted.is_empty() {
        summary.push_str("\nЗафиксированные ошибки:\n- ");
        summary.push_str(&errors_noted.join("\n- "));
    }

    let compacted_msg = json!({
        "role": "system",
        "content": summary,
    });
    messages.splice(prefix_end..middle_end, std::iter::once(compacted_msg));
    let tokens_after = estimate_messages_tokens(messages);
    Some(CompactionSummary {
        dropped_messages,
        tokens_before,
        tokens_after,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_preserves_system_task_tail_and_reduces_tokens() {
        let mut msgs = vec![
            json!({"role": "system", "content": "Системные правила"}),
            json!({"role": "user", "content": "Исправь баг в calc.py"}),
        ];
        for i in 0..14 {
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("c{i}"),
                "content": format!("exit_code=1 ошибка на шаге {i}: AssertionError {}", "x".repeat(300))
            }));
        }
        let before = estimate_messages_tokens(&msgs);
        let summary = compact_wire_messages(
            &mut msgs,
            before, // при >78% компакт запустится
            &["calc.py".into()],
            Some("run_check(pytest) → ошибка"),
            false,
        )
        .expect("compaction should trigger");
        assert!(summary.tokens_after < summary.tokens_before);
        assert!(summary.dropped_messages >= 6);
        assert_eq!(msgs[0]["content"], "Системные правила");
        assert_eq!(msgs[1]["content"], "Исправь баг в calc.py");
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .contains("[COMPACTED CONTEXT]")
        );
        assert!(msgs[2]["content"].as_str().unwrap().contains("calc.py"));
    }
}
