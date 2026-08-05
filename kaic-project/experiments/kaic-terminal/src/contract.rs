// Модуль опубликован как общий контракт Agent Layer: типы и разбор ответа
// планировщика. Пока Planner/run_pass (вторая, параллельная часть работы) не
// используют все элементы, dead_code глушится здесь, а не аннотациями на
// каждом типе.
#![allow(dead_code)]

/// Кто именно сделал проход: id роли, версия контракта роли и модель,
/// которой этот проход был исполнен. Нужна в отчёте, чтобы результат
/// прохода можно было соотнести с конкретной моделью Router'а.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub id: String,
    pub version: u32,
    pub model: String,
}

/// План намеренно минимален: плоский список шагов текстом.
/// Coder'а ещё не существует, поэтому подробная модель шага
/// (id, статус, зависимости) сейчас не нужна — YAGNI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<String>,
}

/// Гейт — это статус, а не число уверенности.
///
/// Решение зафиксировано ранее и здесь не пересматривается: продолжать ли
/// Coder — вопрос бинарный, а порог по confidence пришлось бы подбирать
/// эмпирически под каждую модель и заново после любой смены модели в Router'е.
/// NeedClarification несёт конкретные вопросы, то есть готовое действие
/// (спросить пользователя), а не сигнал, который ещё надо интерпретировать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    Ready,
    NeedClarification {
        clarification_questions: Vec<String>,
    },
}

/// Результат разбора сырого ответа модели-планировщика.
/// plan может быть Some и при NeedClarification: модель иногда присылает
/// черновик плана вместе с вопросами — терять его незачем.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlannerOutput {
    pub plan: Option<Plan>,
    pub gate: GateResult,
}

/// Отчёт об одном проходе агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub agent_identity: AgentIdentity,
    pub plan: Option<Plan>,
    pub gate_result: GateResult,
}

/// Вопрос, который подставляется, когда ответ модели не разобрался.
/// Fail-safe: непонятный вход никогда не становится молчаливым Ready.
pub const FALLBACK_CLARIFICATION_QUESTION: &str =
    "Ответ планировщика не удалось разобрать. Сформулируйте задачу заново, \
     подробнее и одним сообщением.";

/// Вопрос для случая, когда модель объявила READY, но плана не приложила.
pub const MISSING_PLAN_QUESTION: &str =
    "Планировщик объявил готовность, но не привёл ни одного шага плана. \
     Уточните задачу, чтобы план можно было построить.";

/// Разбирает сырой ответ планировщика. Чистая функция: без сети и IO.
///
/// Ожидаемый формат (регистр ключей и markdown-разметка вокруг них не важны):
///
/// ```text
/// STATUS: READY
/// PLAN:
/// 1. первый шаг
/// 2. второй шаг
/// ```
///
/// либо
///
/// ```text
/// STATUS: NEED_CLARIFICATION
/// QUESTIONS:
/// - первый вопрос
/// - второй вопрос
/// ```
///
/// Любая неоднозначность — отсутствующий или неизвестный статус, два разных
/// статуса в одном ответе, READY без шагов, NEED_CLARIFICATION без вопросов —
/// трактуется как NeedClarification. Молчаливый Ready на непонятном входе
/// невозможен по построению: Ready возвращается только при ровно одном
/// распознанном статусе READY и непустом плане.
pub fn parse_planner_response(raw: &str) -> ParsedPlannerOutput {
    let mut statuses: Vec<StatusToken> = Vec::new();
    let mut steps: Vec<String> = Vec::new();
    let mut questions: Vec<String> = Vec::new();
    let mut section = Section::Outside;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || is_fence(line) {
            continue;
        }

        if let Some(value) = header_value(line, "status") {
            statuses.push(parse_status(&value));
            section = Section::Outside;
            continue;
        }
        if let Some(value) = header_value(line, "plan") {
            section = Section::Plan;
            push_item(&mut steps, &value);
            continue;
        }
        if let Some(value) = header_value(line, "questions") {
            section = Section::Questions;
            push_item(&mut questions, &value);
            continue;
        }

        match section {
            Section::Plan => push_item(&mut steps, line),
            Section::Questions => push_item(&mut questions, line),
            // Свободный текст вне секций — комментарии модели, не данные.
            Section::Outside => {}
        }
    }

    let plan = if steps.is_empty() {
        None
    } else {
        Some(Plan { steps })
    };

    let gate = match single_status(&statuses) {
        Some(StatusToken::Ready) => {
            if plan.is_some() {
                GateResult::Ready
            } else {
                need_clarification(questions, MISSING_PLAN_QUESTION)
            }
        }
        Some(StatusToken::NeedClarification) => {
            need_clarification(questions, FALLBACK_CLARIFICATION_QUESTION)
        }
        // Статуса нет, он неизвестен, или их несколько разных.
        _ => need_clarification(questions, FALLBACK_CLARIFICATION_QUESTION),
    };

    ParsedPlannerOutput { plan, gate }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusToken {
    Ready,
    NeedClarification,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Outside,
    Plan,
    Questions,
}

/// Ровно один распознанный статус, иначе None (и, значит, NeedClarification).
fn single_status(statuses: &[StatusToken]) -> Option<StatusToken> {
    let first = *statuses.first()?;
    if first == StatusToken::Unknown {
        return None;
    }
    if statuses.iter().any(|s| *s != first) {
        return None;
    }
    Some(first)
}

fn need_clarification(questions: Vec<String>, fallback: &str) -> GateResult {
    let clarification_questions = if questions.is_empty() {
        vec![fallback.to_string()]
    } else {
        questions
    };
    GateResult::NeedClarification {
        clarification_questions,
    }
}

fn parse_status(value: &str) -> StatusToken {
    let normalized: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();

    match normalized.as_str() {
        "ready" => StatusToken::Ready,
        "needclarification" => StatusToken::NeedClarification,
        _ => StatusToken::Unknown,
    }
}

/// Если строка — заголовок секции `key`, возвращает то, что стоит после
/// двоеточия (возможно, пустую строку). Разметка вокруг ключа (**, ##, `)
/// игнорируется, регистр не важен.
fn header_value(line: &str, key: &str) -> Option<String> {
    let colon = line.find(':')?;
    let (head, tail) = line.split_at(colon);
    let head: String = head
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase();
    if head != key {
        return None;
    }
    Some(tail[1..].trim().to_string())
}

fn is_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

/// Убирает маркер списка или нумерацию и добавляет шаг/вопрос, если после
/// очистки осталось непустое содержимое.
fn push_item(items: &mut Vec<String>, raw: &str) {
    let item = strip_emphasis(strip_bullet(raw.trim()));
    // Хвосты разметки вроде "**" от заголовка "**Plan:**" содержательными
    // шагами не являются.
    if item.chars().any(char::is_alphanumeric) {
        items.push(item.to_string());
    }
}

fn strip_emphasis(line: &str) -> &str {
    let trimmed = line.trim_matches('*').trim();
    if trimmed.chars().any(char::is_alphanumeric) {
        trimmed
    } else {
        line
    }
}

fn strip_bullet(line: &str) -> &str {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("• "))
    {
        return rest.trim();
    }

    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &line[digits.len()..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim();
        }
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_plan() {
        let raw = "STATUS: READY\n\
                   PLAN:\n\
                   1. Прочитать src/repl.rs\n\
                   2. Добавить команду /plan\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.gate, GateResult::Ready);
        assert_eq!(
            parsed.plan,
            Some(Plan {
                steps: vec![
                    "Прочитать src/repl.rs".to_string(),
                    "Добавить команду /plan".to_string(),
                ],
            })
        );
    }

    #[test]
    fn parses_explicit_need_clarification() {
        let raw = "STATUS: NEED_CLARIFICATION\n\
                   QUESTIONS:\n\
                   - Какой файл менять?\n\
                   - Нужны ли тесты?\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.plan, None);
        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec![
                    "Какой файл менять?".to_string(),
                    "Нужны ли тесты?".to_string(),
                ],
            }
        );
    }

    #[test]
    fn unparsable_response_falls_back_to_need_clarification() {
        let raw = "Конечно! Вот что я думаю по этому поводу…\n\n\
                   Возможно, стоит начать с рефакторинга.";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.plan, None);
        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec![FALLBACK_CLARIFICATION_QUESTION.to_string()],
            }
        );
    }

    #[test]
    fn empty_response_falls_back_to_need_clarification() {
        let parsed = parse_planner_response("");

        assert_eq!(parsed.plan, None);
        assert!(matches!(parsed.gate, GateResult::NeedClarification { .. }));
    }

    #[test]
    fn ready_without_steps_is_not_ready() {
        let raw = "STATUS: READY\nPLAN:\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.plan, None);
        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec![MISSING_PLAN_QUESTION.to_string()],
            }
        );
    }

    #[test]
    fn need_clarification_without_questions_gets_fallback() {
        let parsed = parse_planner_response("status: need clarification");

        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec![FALLBACK_CLARIFICATION_QUESTION.to_string()],
            }
        );
    }

    #[test]
    fn unknown_status_is_not_ready() {
        let raw = "STATUS: OK\nPLAN:\n- сделать всё хорошо\n";

        let parsed = parse_planner_response(raw);

        // План сохранён, но гейт закрыт: статус не распознан.
        assert_eq!(
            parsed.plan,
            Some(Plan {
                steps: vec!["сделать всё хорошо".to_string()],
            })
        );
        assert!(matches!(parsed.gate, GateResult::NeedClarification { .. }));
    }

    #[test]
    fn conflicting_statuses_are_not_ready() {
        let raw = "STATUS: READY\n\
                   PLAN:\n\
                   1. шаг\n\
                   STATUS: NEED_CLARIFICATION\n\
                   QUESTIONS:\n\
                   - точно ли так?\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec!["точно ли так?".to_string()],
            }
        );
    }

    #[test]
    fn tolerates_markdown_and_code_fences() {
        let raw = "```\n\
                   **Status:** ready\n\
                   **Plan:**\n\
                   1) первый шаг\n\
                   * второй шаг\n\
                   ```\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.gate, GateResult::Ready);
        assert_eq!(
            parsed.plan,
            Some(Plan {
                steps: vec!["первый шаг".to_string(), "второй шаг".to_string()],
            })
        );
    }

    #[test]
    fn keeps_draft_plan_alongside_questions() {
        let raw = "STATUS: NEED_CLARIFICATION\n\
                   PLAN:\n\
                   - черновой шаг\n\
                   QUESTIONS:\n\
                   - какой модуль трогать?\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(
            parsed.plan,
            Some(Plan {
                steps: vec!["черновой шаг".to_string()],
            })
        );
        assert_eq!(
            parsed.gate,
            GateResult::NeedClarification {
                clarification_questions: vec!["какой модуль трогать?".to_string()],
            }
        );
    }

    #[test]
    fn inline_header_value_is_a_step() {
        let raw = "STATUS: READY\nPLAN: единственный шаг\n";

        let parsed = parse_planner_response(raw);

        assert_eq!(parsed.gate, GateResult::Ready);
        assert_eq!(
            parsed.plan,
            Some(Plan {
                steps: vec!["единственный шаг".to_string()],
            })
        );
    }
}
