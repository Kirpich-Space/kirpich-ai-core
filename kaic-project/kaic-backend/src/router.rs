//! Router — определяет категорию задачи.
//!
//! Router полностью детерминирован: никаких вызовов LLM, никакого
//! embedding, никакой классификации через Nemotron. На RTX 4060 8GB
//! постоянная загрузка/выгрузка модели ради классификации убила бы
//! скорость системы — поэтому Router работает как обычный быстрый
//! Rust-код на regex и метаданных задачи.
//!
//! Router не знает ни одной модели. Он возвращает только категорию
//! (`Category`) — дальше решает Capability Registry.
//!
//! Порядок проверки:
//! 1. Явная категория от Agent'а — если указана, используется без анализа.
//! 2. Метаданные задачи (размер проекта) — большой проект классифицируется
//!    без анализа текста.
//! 3. Ключевые слова / regex — от самых специфичных категорий к общим.
//! 4. Default — `Simple`, если ничего не совпало.

/// Категория задачи. Единственное, что Router возвращает наружу.
///
/// Значения строго соответствуют ключам в Capability Registry YAML —
/// если добавляется новая категория, она должна появиться в обоих местах.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Simple,
    Programming,
    LargeProject,
    Engineering,
    DeepAnalysis,
    Video,
    Research,
}

impl Category {
    /// Имя категории в том виде, в котором оно записано в Capability Registry YAML.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Simple => "Simple",
            Category::Programming => "Programming",
            Category::LargeProject => "LargeProject",
            Category::Engineering => "Engineering",
            Category::DeepAnalysis => "DeepAnalysis",
            Category::Video => "Video",
            Category::Research => "Research",
        }
    }
}

/// Метаданные задачи, которых достаточно Router'у для классификации
/// без анализа текста. Agent заполняет то, что знает о задаче;
/// Router не запрашивает ничего сверх этого.
#[derive(Debug, Clone, Default)]
pub struct TaskMetadata {
    /// Явная категория от Agent'а. Если задана — Router её просто возвращает.
    pub agent_category: Option<Category>,

    /// Количество файлов, затронутых задачей (diff, проект и т.п.).
    pub file_count: usize,

    /// Суммарный размер изменяемого/анализируемого кода в строках.
    pub total_lines: usize,
}

/// Порог, после которого задача считается "большим проектом"
/// без анализа текста запроса.
const LARGE_PROJECT_FILE_THRESHOLD: usize = 10;
const LARGE_PROJECT_LINES_THRESHOLD: usize = 1000;

/// Router — определяет категорию задачи по метаданным и тексту запроса.
pub struct Router;

impl Router {
    /// Определяет категорию задачи.
    ///
    /// `text` — исходный текст запроса (для поиска ключевых слов).
    /// `metadata` — то, что Agent уже знает о задаче.
    pub fn classify(text: &str, metadata: &TaskMetadata) -> Category {
        // Шаг 1: Agent уже указал категорию — доверяем ему, дальше не идём.
        if let Some(category) = metadata.agent_category {
            return category;
        }

        // Шаг 2: метаданные говорят о большом проекте — не нужно читать текст.
        if metadata.file_count >= LARGE_PROJECT_FILE_THRESHOLD
            || metadata.total_lines >= LARGE_PROJECT_LINES_THRESHOLD
        {
            return Category::LargeProject;
        }

        // Шаг 3: ключевые слова, от специфичных категорий к общим.
        let lower = text.to_lowercase();

        if contains_any(&lower, ENGINEERING_KEYWORDS) {
            return Category::Engineering;
        }
        if contains_any(&lower, DEEP_ANALYSIS_KEYWORDS) {
            return Category::DeepAnalysis;
        }
        if contains_any(&lower, VIDEO_KEYWORDS) {
            return Category::Video;
        }
        if contains_any(&lower, RESEARCH_KEYWORDS) {
            return Category::Research;
        }
        if contains_any(&lower, PROGRAMMING_KEYWORDS) {
            return Category::Programming;
        }

        // Шаг 4: ничего не совпало — безопасный default.
        Category::Simple
    }
}

fn contains_any(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|kw| text.contains(kw))
}

const ENGINEERING_KEYWORDS: &[&str] = &[
    "расчёт",
    "расчет",
    "stm32",
    "esp32",
    "sfs",
    "двигатель",
    "тяга",
    "орбита",
    "cadquery",
    "прошивка",
    "микроконтроллер",
];

const DEEP_ANALYSIS_KEYWORDS: &[&str] = &[
    "архитектур",
    "проверь весь",
    "аудит",
    "рефактори",
    "проанализируй проект",
];

const VIDEO_KEYWORDS: &[&str] = &[
    "видео",
    "монтаж",
    "смонтируй",
    "субтитры",
    "таймкод",
    "ffmpeg",
];

const RESEARCH_KEYWORDS: &[&str] = &[
    "найди статью",
    "исследуй",
    "новост",
    "почитай про",
    "изучи вопрос",
];

const PROGRAMMING_KEYWORDS: &[&str] = &[
    "напиши функцию",
    "напиши код",
    "исправь баг",
    "исправь ошибку",
    "рефактор",
    "diff",
    "compile",
    "компилир",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_override_wins_regardless_of_text_or_metadata() {
        let metadata = TaskMetadata {
            agent_category: Some(Category::Video),
            file_count: 100, // выглядит как LargeProject, но override важнее
            total_lines: 5000,
        };
        assert_eq!(
            Router::classify("рассчитай тягу двигателя", &metadata),
            Category::Video
        );
    }

    #[test]
    fn large_metadata_wins_over_keywords() {
        let metadata = TaskMetadata {
            agent_category: None,
            file_count: 20,
            total_lines: 200,
        };
        // текст похож на Programming, но метаданные важнее по порядку проверки
        assert_eq!(
            Router::classify("напиши функцию сортировки", &metadata),
            Category::LargeProject
        );
    }

    #[test]
    fn keywords_classify_engineering() {
        let metadata = TaskMetadata::default();
        assert_eq!(
            Router::classify("посчитай расчёт тяги для двигателя Арматура", &metadata),
            Category::Engineering
        );
    }

    #[test]
    fn keywords_classify_programming() {
        let metadata = TaskMetadata::default();
        assert_eq!(
            Router::classify("напиши функцию сортировки на Rust", &metadata),
            Category::Programming
        );
    }

    #[test]
    fn unknown_text_defaults_to_simple() {
        let metadata = TaskMetadata::default();
        assert_eq!(
            Router::classify("привет, как дела?", &metadata),
            Category::Simple
        );
    }

    #[test]
    fn category_as_str_matches_registry_keys() {
        assert_eq!(Category::LargeProject.as_str(), "LargeProject");
        assert_eq!(Category::DeepAnalysis.as_str(), "DeepAnalysis");
    }
}
