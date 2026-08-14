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

/// Служебные слова, которые не должны оставаться ПОСЛЕДНИМИ в термине.
///
/// Обрезка до трёх слов рубит фразу посередине, и хвостом оказывается
/// предлог: «снежный лес на», «старый маяк на», «Old lighthouse on»,
/// «улица под». Openverse требует совпадения ВСЕХ слов, поэтому висящий
/// предлог не уточняет запрос, а обваливает выдачу — замер 2026-08-09:
/// «старый маяк на» дало 4 результата против 240 у «Old lighthouse on».
///
/// Оба языка намеренно. Язык вывода мы пиним промптом, но модель может
/// сорваться на русский, и санитайзер обязан пережить это сам.
///
/// Список только для ХВОСТА. Внутри фразы те же слова осмысленны:
/// «bridge over river» — нормальный термин, и `over` там трогать нельзя.
const TRAILING_STOPWORDS: &[&str] = &[
    // английский
    "a", "an", "the", "of", "on", "in", "at", "to", "for", "with", "by",
    "from", "over", "under", "into", "near", "and", "or",
    // русский
    "в", "во", "на", "над", "под", "у", "к", "ко", "с", "со", "из", "от",
    "до", "по", "за", "при", "про", "для", "и", "или", "около", "возле",
];

/// Снимает служебные слова с конца термина.
///
/// Возвращает исходную строку, если после снятия ничего не осталось: пустой
/// термин хуже неточного — он отправил бы контур на дословный текст задачи,
/// то есть ровно к тому дефекту, ради которого извлечение и делалось.
fn strip_trailing_stopwords(term: &str) -> String {
    let mut words: Vec<&str> = term.split_whitespace().collect();
    while words.len() > 1 {
        let last = words[words.len() - 1].to_lowercase();
        if TRAILING_STOPWORDS.contains(&last.as_str()) {
            words.pop();
        } else {
            break;
        }
    }
    // `len() > 1` в условии уже гарантирует непустоту, но если термин
    // состоял ровно из одного служебного слова — возвращаем его как есть.
    if words.is_empty() {
        term.to_string()
    } else {
        words.join(" ")
    }
}

/// Промпт намеренно минимальный: без грамматик и структурного форсинга.
/// Задача простая, а вся структура, которая нам нужна — одна строка.
///
/// Термин ВСЕГДА на английском, независимо от языка задачи. Причина не
/// стилистическая, а размер корпуса: Openverse индексирован преимущественно
/// по-английски, и на одном и том же предмете съёмки разница кратная
/// (замер 2026-08-09: «mountain lake» — 240 результатов против «горное
/// озеро» — 14; «snowy forest» — 240 против «снежный лес» — 1).
///
/// До этой правки промпт о языке не говорил вовсе, а примеры в нём были
/// русские — модель выбирала язык произвольно, и выдача прыгала на порядок
/// от прогона к прогону: английский вход `mountain lake` возвращал русский
/// термин, русский вход про маяк — английский. Примеры заменены на
/// английские вместе с требованием: требовать английский вывод, показывая
/// русские образцы, значило бы давать противоречивую инструкцию.
///
/// Цена принята сознательно: «горное озеро» и «mountain lake» — не одно
/// множество фотографий, локальная специфика теряется. Бедная выдача хуже,
/// чем выдача без локальной специфики.
pub fn build_messages(task_text: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: "Ты выделяешь поисковый запрос для банка фотографий. \
Ответь ТОЛЬКО названием предмета съёмки: одно-два слова, связная фраза. \
ВСЕГДА НА АНГЛИЙСКОМ, даже если задача на другом языке \
(например «mountain lake», «winter forest»). \
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

/// Разделители строк, по которым режется ответ модели.
///
/// `str::lines()` знает только `\n` (и снимает `\r` перед ним) — этого мало.
/// Правило «взять последнюю непустую строку» существует ради ОДНОГО: выкинуть
/// преамбулу модели. Преамбула, отделённая не переводом строки, а другим
/// разделителем, это правило обходила молча: слова преамбулы оставались в
/// термине, а сам ответ выбрасывался обрезкой до трёх слов.
///
/// Набор — тот же, что у `str.splitlines()` в Python: именно на нём считался
/// замер temperature (`experiments/temperature-probe`), и расхождение между
/// замеренным поведением и рабочим — это ровно тот класс дефекта, который
/// здесь уже стоил одного замера.
const LINE_SEPARATORS: &[char] = &[
    '\n',       // LF
    '\r',       // CR
    '\u{0b}',   // вертикальная табуляция
    '\u{0c}',   // перевод страницы
    '\u{85}',   // NEL
    '\u{2028}', // разделитель строк
    '\u{2029}', // разделитель абзацев
];

/// Приводит ответ модели к поисковому термину.
///
/// `None` означает «извлечение не удалось» — вызывающий обязан взять
/// дословный текст задачи.
pub fn sanitize(raw: &str) -> Option<String> {
    // Модели любят добавлять преамбулу; берём последнюю непустую строку —
    // именно в ней обычно и лежит ответ.
    let line = raw
        .split(LINE_SEPARATORS)
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

    // Снимаем хвост ПОСЛЕ обрезки: именно обрезка его и создаёт.
    let term = strip_trailing_stopwords(&term);

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

    // --- Хвостовые служебные слова ---
    // Все четыре случая ниже взяты из замера 2026-08-09, а не придуманы:
    // именно они уронили выдачу с 240 до 4 и до 1.

    #[test]
    fn trailing_preposition_is_stripped_in_both_languages() {
        assert_eq!(strip_trailing_stopwords("снежный лес на"), "снежный лес");
        assert_eq!(strip_trailing_stopwords("старый маяк на"), "старый маяк");
        assert_eq!(strip_trailing_stopwords("Old lighthouse on"), "Old lighthouse");
        assert_eq!(strip_trailing_stopwords("улица под"), "улица");
    }

    #[test]
    fn meaningful_last_word_is_kept() {
        assert_eq!(strip_trailing_stopwords("mountain lake"), "mountain lake");
        assert_eq!(strip_trailing_stopwords("горное озеро"), "горное озеро");
        assert_eq!(strip_trailing_stopwords("busy city street"), "busy city street");
    }

    #[test]
    fn stopword_inside_the_phrase_is_untouched() {
        // «over» в середине — часть смысла, а не мусор обрезки.
        assert_eq!(strip_trailing_stopwords("bridge over river"), "bridge over river");
        assert_eq!(strip_trailing_stopwords("дом у моря"), "дом у моря");
    }

    #[test]
    fn stripping_never_yields_an_empty_term() {
        // Термин целиком из служебных слов: снимать до пустоты нельзя —
        // пустой термин отправил бы контур на дословный текст задачи.
        assert_eq!(strip_trailing_stopwords("на"), "на");
        assert_eq!(strip_trailing_stopwords("of the"), "of");
        assert!(!strip_trailing_stopwords("под над").is_empty());
    }

    #[test]
    fn sanitize_applies_stripping_after_the_three_word_limit() {
        // Предел в 3 слова соблюдён, и хвост, созданный самой обрезкой, снят.
        assert_eq!(
            sanitize("старый маяк на скалистом берегу"),
            Some("старый маяк".to_string())
        );
        // Три осмысленных слова остаются тремя.
        assert_eq!(
            sanitize("busy city street in the rain"),
            Some("busy city street".to_string())
        );
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
    fn preamble_is_dropped_whatever_separates_it_from_the_answer() {
        // Все четыре входа найдены сравнением с портом санитайзера из
        // experiments/temperature-probe: на них Rust и порт расходились,
        // причём расходились МОЛЧА — оба возвращали правдоподобный термин.
        //
        // До правки `str::lines()` не резал ни один из этих разделителей,
        // преамбула оставалась первой строкой, и до настоящего ответа
        // обрезка до трёх слов уже не доходила.
        for sep in ['\u{2028}', '\u{85}', '\u{0b}', '\u{0c}', '\u{2029}'] {
            let raw = format!("mountain lake{sep}city bridge");
            assert_eq!(
                sanitize(&raw),
                Some("city bridge".to_string()),
                "разделитель U+{:04X} не отделил преамбулу",
                sep as u32
            );
        }
    }

    #[test]
    fn a_separator_inside_the_answer_does_not_split_words_together() {
        // Обратная сторона той же правки: разделитель — граница строк, а не
        // невидимый склеиватель. «mountain lake\u{2028}горное озеро» до
        // правки давало «mountain lake горное» — смесь преамбулы и ответа.
        assert_eq!(
            sanitize("mountain lake\u{2028}горное озеро"),
            Some("горное озеро".to_string())
        );
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
