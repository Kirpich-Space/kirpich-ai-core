use anyhow::Result;

use crate::contract::{parse_planner_response, ParsedPlannerOutput};
use crate::core::engine::{ChatMessage, ChatParams, KaicEngine};

/// GBNF-грамматика сэмплинга для Planner'а.
///
/// Гарантирует ровно одно: вывод обязан начаться с `STATUS: READY` либо
/// `STATUS: NEED_CLARIFICATION`. Дальше (`rest`) разрешено что угодно, кроме
/// NUL — разбор остального текста остаётся за
/// `contract::parse_planner_response`, и дублировать его формат грамматикой
/// незачем: два описания одного формата разъезжаются.
///
/// Почему грамматика вообще понадобилась: две реальные попытки промпт-only
/// показали, что модель инструкцию понимает (пересказывает её дословно) и всё
/// равно открывает `<think>` нулевым токеном, съедая весь бюджет токенов.
/// Грамматика делает такое начало не «нежелательным», а невыразимым.
///
/// `rest` допускает пустоту, поэтому грамматика выполнима сразу после
/// префикса — EOG разрешён, модель вправе закончить когда сочтёт нужным, и
/// зависнуть на исчерпании max_tokens грамматика не заставляет.
pub const PLANNER_GRAMMAR: &str = r#"root ::= status rest
status ::= "STATUS: READY" | "STATUS: NEED_CLARIFICATION"
rest ::= [^\x00]*
"#;

/// Подставляет грамматику Planner'а в параметры вызова, не трогая остальное.
///
/// Грамматика живёт здесь, а не у вызывающего: она — часть контракта Planner'а
/// с моделью, ровно как system-промпт. Вызывающий (smoke, будущая обвязка
/// REPL) задаёт temperature/max_tokens/context и не обязан знать про GBNF.
fn planner_params(base: &ChatParams) -> ChatParams {
    ChatParams {
        grammar: Some(PLANNER_GRAMMAR.to_string()),
        ..base.clone()
    }
}

/// Builds a minimal planning prompt, calls `KaicEngine::chat`, parses via contract.
///
/// Промпт задаёт ровно тот формат, который разбирает
/// `contract::parse_planner_response` (STATUS/PLAN/QUESTIONS), и явно
/// подавляет рассуждение вслух. Текст промпта остаётся при включённой
/// грамматике: грамматика гарантирует только префикс, а осмысленность шагов
/// после него — по-прежнему задача инструкции.
///
/// Намеренное свойство промпта: он показывает **оба** значения STATUS. Если
/// маленькая модель начнёт цитировать инструкцию вместо ответа (наблюдалось на
/// реальном прогоне), в тексте окажутся два разных статуса, и правило
/// конфликта статусов в `parse_planner_response` закроет гейт. То есть
/// дословный пересказ промпта не может превратиться в ложный `Ready` с планом
/// из плейсхолдеров.
pub fn plan(engine: &KaicEngine, task: &str, params: &ChatParams) -> Result<ParsedPlannerOutput> {
    let system = "You are the KAIC Planner. You never write or modify files, and you never \
think out loud.\n\n\
Answer with ONLY this exact format, starting at the very first character:\n\n\
STATUS: READY\n\
PLAN:\n\
1. <first step>\n\
2. <second step>\n\n\
If, and only if, the task is too ambiguous to plan, answer instead with:\n\n\
STATUS: NEED_CLARIFICATION\n\
QUESTIONS:\n\
- <question>\n\n\
Rules:\n\
- Your first characters must be \"STATUS: \". Never open a <think> block. \
No preamble, no reasoning, no explanation, no markdown fences.\n\
- One short imperative inspect-only step per line, at most five, as many as the task needs.\n\
- Stop immediately after the last line. Write nothing else.";
    // Формат повторён в user-сообщении: оно идёт последним в ChatML, а у
    // маленьких моделей ближайшая к точке генерации инструкция весит больше.
    let user = format!(
        "Task:\n{task}\n\n\
Reply now in the required format. First characters: \"STATUS: \"."
    );

    let history = [
        ChatMessage::new("system", system),
        ChatMessage::new("user", user),
    ];

    let raw = engine.chat(&history, &planner_params(params))?;
    eprintln!("[planner raw]\n{raw}\n[/planner raw]");
    Ok(parse_planner_response(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::engine::GRAMMAR_ROOT_RULE;

    fn base_params() -> ChatParams {
        ChatParams {
            temperature: 0.2,
            max_tokens: 256,
            context: 2048,
            grammar: None,
        }
    }

    #[test]
    fn planner_injects_grammar_and_preserves_other_params() {
        let base = base_params();
        let params = planner_params(&base);

        assert_eq!(params.grammar.as_deref(), Some(PLANNER_GRAMMAR));
        assert_eq!(params.temperature, base.temperature);
        assert_eq!(params.max_tokens, base.max_tokens);
        assert_eq!(params.context, base.context);
    }

    #[test]
    fn caller_params_stay_unconstrained() {
        // Ветка grammar: None должна оставаться достижимой: её передаёт REPL,
        // и planner_params() не имеет права мутировать чужие параметры.
        let base = base_params();
        let _ = planner_params(&base);

        assert!(base.grammar.is_none());
    }

    #[test]
    fn grammar_declares_the_root_rule_engine_asks_for() {
        // Движок зовёт грамматику со стартовым правилом GRAMMAR_ROOT_RULE;
        // если его не окажется в тексте, llama.cpp вернёт null и
        // LlamaSampler::grammar() запаникует.
        assert!(PLANNER_GRAMMAR.contains(&format!("{GRAMMAR_ROOT_RULE} ::=")));
    }

    #[test]
    fn grammar_allows_exactly_the_two_contract_statuses() {
        assert!(PLANNER_GRAMMAR.contains("\"STATUS: READY\""));
        assert!(PLANNER_GRAMMAR.contains("\"STATUS: NEED_CLARIFICATION\""));
    }

    #[test]
    fn grammar_has_no_interior_nul() {
        // LlamaSampler::grammar() строит CString и паникует на NUL-байте.
        assert!(!PLANNER_GRAMMAR.contains('\0'));
    }
}
