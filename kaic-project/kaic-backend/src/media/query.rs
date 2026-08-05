//! Извлечение поискового термина из свободного текста задачи.
//!
//! Зачем: в поиск уходил текст задачи дословно, и человеческая формулировка
//! ломала его наглухо. Замер по Openverse:
//!
//! ```text
//! «сделай видео про горное озеро» → 0 результатов
//! «горное озеро»                  → 14
//! «озеро»                         → 240
//! ```
//!
//! Дело не в языке: «горное озеро» ищется нормально. Дело в командных словах
//! («сделай», «видео», «про») — они не совпадают ни с одним тегом, а поиск
//! требует совпадения всех слов.
//!
//! Почему не стоп-лист: перечислить все формы вежливых просьб нельзя, а
//! промах стоп-листа не виден — он тихо выдаёт ноль результатов. Модель уже
//! есть в системе, и это ровно её работа.
//!
//! Почему не Planner из kaic-terminal: там контракт, GBNF-грамматика и
//! PassReport — вся эта машинерия существует ради гарантии структуры плана.
//! Здесь нужны два-три слова, и городить ради них агентный слой было бы
//! несоразмерно.
//!
//! Извлечение — улучшение, а не обязательный шаг: при любой неудаче
//! используется прежний дословный текст, то есть хуже, чем было, не станет.

use crate::model_backend::{GenerateRequest, Message, Role};

/// Модель для извлечения. Именно `Fable9B`: она резидентна (`always_loaded`),
/// поэтому шаг не платит за загрузку модели в память.
pub const EXTRACTION_MODEL_LABEL: &str = "Fable9B";

/// Потолок длины извлечённого термина. Всё длиннее — признак, что модель
/// пересказала задачу вместо того, чтобы выделить ключевые слова.
const MAX_TERM_CHARS: usize = 60;

/// Максимум слов в термине. Openverse требует совпадения всех слов, поэтому
/// длинная фраза сужает выдачу до нуля — ровно тот дефект, что мы чиним.
const MAX_TERM_WORDS: usize = 3;

/// Промпт намеренно минимальный: без грамматик и структурного форсинга.
/// Задача простая, а вся структура, которая нам нужна — одна строка.
pub fn build_messages(task_text: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: "Ты выделяешь поисковый запрос для банка фотографий. \
Ответь ТОЛЬКО названием предмета съёмки: одно-два слова, связная фраза \
(например «горное озеро», «зимний лес»). \
НЕ перечисляй синонимы через пробел. \
Без глаголов-команд, без слов «видео», «фото», «сделай». \
Без кавычек, без пояснений, без точки в конце."
                .to_string(),
        },
        Message {
            role: Role::User,
            content: format!("Задача: {task_text}\nКлючевые слова:"),
        },
    ]
}

pub fn build_request(task_text: &str) -> GenerateRequest {
    GenerateRequest {
        messages: build_messages(task_text),
        // Значение из реестра подставит Scheduler.
        temperature: None,
    }
}

/// Приводит ответ модели к поисковому термину.
///
/// `None` означает «извлечение не удалось» — вызывающий обязан взять
/// дословный текст задачи.
pub fn sanitize(raw: &str) -> Option<String> {
    // Модели любят добавлять преамбулу; берём последнюю непустую строку —
    // именно в ней обычно и лежит ответ.
    let line = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()?;

    // Снимаем оформление: кавычки, маркеры списка, финальную пунктуацию.
    let cleaned: String = line
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '«' | '»' | '-' | '*' | '.' | ':' | '•')
        })
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-')
        .collect();

    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    if cleaned.chars().count() > MAX_TERM_CHARS {
        // Модель пересказала задачу — доверять такому нельзя.
        return None;
    }

    let term = words
        .into_iter()
        .take(MAX_TERM_WORDS)
        .collect::<Vec<_>>()
        .join(" ");

    if term.trim().is_empty() {
        None
    } else {
        Some(term)
    }
}

/// Варианты запроса от самого узкого к самому широкому.
///
/// Openverse требует совпадения ВСЕХ слов запроса, поэтому лишнее слово не
/// уточняет выдачу, а обнуляет её. Наблюдалось вживую: модель вернула
/// «Озеро горы вода» — правдоподобный набор слов, дающий 0 результатов,
/// тогда как «озеро» даёт 240.
///
/// Отбрасывать слова с конца, а не с начала: в русской именной группе
/// главное слово обычно первое («горное озеро» → «горное»… — нет, наоборот),
/// поэтому порядок сохраняем как есть и просто укорачиваем хвост — это
/// сохраняет «горное озеро» на втором шаге вместо распада на части.
pub fn narrowing_variants(term: &str) -> Vec<String> {
    let words: Vec<&str> = term.split_whitespace().collect();
    let mut variants = Vec::new();
    for take in (1..=words.len()).rev() {
        let variant = words[..take].join(" ");
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_goes_from_specific_to_broad() {
        // Ровно тот случай, что упал вживую: три слова → 0 результатов,
        // одно слово → сотни.
        assert_eq!(
            narrowing_variants("Озеро горы вода"),
            vec!["Озеро горы вода", "Озеро горы", "Озеро"]
        );
    }

    #[test]
    fn single_word_has_nothing_to_narrow() {
        assert_eq!(narrowing_variants("озеро"), vec!["озеро"]);
        assert!(narrowing_variants("").is_empty());
    }

    #[test]
    fn extracts_plain_answer() {
        assert_eq!(sanitize("горное озеро"), Some("горное озеро".to_string()));
    }

    #[test]
    fn strips_decoration_models_like_to_add() {
        assert_eq!(sanitize("\"горное озеро\""), Some("горное озеро".to_string()));
        assert_eq!(sanitize("«горное озеро»"), Some("горное озеро".to_string()));
        assert_eq!(sanitize("- горное озеро"), Some("горное озеро".to_string()));
        assert_eq!(sanitize("горное озеро."), Some("горное озеро".to_string()));
    }

    #[test]
    fn takes_the_last_line_when_model_adds_preamble() {
        let raw = "Конечно! Вот ключевые слова:\n\nгорное озеро";
        assert_eq!(sanitize(raw), Some("горное озеро".to_string()));
    }

    #[test]
    fn caps_word_count_because_openverse_requires_all_words_to_match() {
        // Длинная фраза сужает выдачу до нуля — именно тот дефект, что чиним.
        let raw = "горное озеро закат туман отражение красота";
        let term = sanitize(raw).expect("термин есть");
        assert_eq!(term.split_whitespace().count(), MAX_TERM_WORDS);
        assert_eq!(term, "горное озеро закат");
    }

    #[test]
    fn rejects_retelling_instead_of_keywords() {
        // Модель пересказала задачу вместо выделения слов — доверять нельзя,
        // вызывающий возьмёт дословный текст.
        let raw = "Пользователь просит подготовить видеоролик про красивое \
                   горное озеро в предгорьях с закатом и туманом";
        assert_eq!(sanitize(raw), None);
    }

    #[test]
    fn rejects_empty_and_punctuation_only() {
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("   \n  "), None);
        assert_eq!(sanitize("...."), None);
        assert_eq!(sanitize("\"\""), None);
    }

    #[test]
    fn keeps_latin_terms_intact() {
        assert_eq!(sanitize("mountain lake"), Some("mountain lake".to_string()));
    }

    #[test]
    fn prompt_names_no_command_words() {
        let messages = build_messages("сделай видео про горное озеро");
        assert_eq!(messages.len(), 2);
        // Текст задачи попадает в user-сообщение как есть.
        assert!(messages[1].content.contains("сделай видео про горное озеро"));
        // Система просит связную фразу и прямо запрещает перечисление
        // синонимов — именно оно давало 0 результатов на живом прогоне
        // («Озеро горы вода»).
        assert!(messages[0].content.contains("связная фраза"));
        assert!(messages[0].content.contains("НЕ перечисляй синонимы"));
    }
}
