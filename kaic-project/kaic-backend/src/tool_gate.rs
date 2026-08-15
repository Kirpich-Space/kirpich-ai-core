//! Гейт вызова инструментов: без разрешения исполнить нельзя.
//!
//! Зачем отдельный модуль, а не проверка перед вызовом. Проверку можно
//! забыть в новой точке, и забудут — через полгода, когда появится второй
//! путь исполнения. Здесь работает тот же приём, что у `AdmittedAsset` в
//! медиа-гейте и у `textfile=` вместо экранирования в ffmpeg: обойти гейт
//! нельзя не потому, что все дисциплинированы, а потому, что в обход нечего
//! положить. Точка исполнения принимает [`ApprovedToolCall`], а
//! сконструировать его вне этого модуля невозможно — поля приватные, а
//! единственные конструкторы приватны или требуют явного решения.
//!
//! Почему это не паранойя: в наборе blender-mcp есть `execute_blender_code` —
//! выполнение произвольного Python внутри Blender, то есть внутри проекта
//! пользователя и с доступом к его файловой системе. Сегодня все вызовы
//! упираются в отказ сокета, потому что аддон не установлен; в день,
//! когда он появится, KAIC станет исполнителем произвольного кода по
//! решению модели.
//!
//! Чего здесь НЕТ и не должно быть:
//!
//! * **аннотаций сервера.** MCP отдаёт `annotations` с подсказками вида
//!   `readOnlyHint`, и спецификация прямо называет их недоверенными: сервер,
//!   пометивший себя безопасным, не является основанием. Мы их даже не
//!   читаем — поле не доходит до `ToolSpec`;
//! * **эвристики по именам.** Префикс `get_` не означает безопасность:
//!   `get_viewport_screenshot` читает экран пользователя. Разрешение даёт
//!   только явный список, составленный человеком.

use crate::model_backend::ToolCall;

/// Кто разрешил вызов. Идёт в аудит-журнал: «разрешено» без «кем» не
/// отвечает на вопрос, почему задача сделала то, что сделала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Имя нашлось в списке автоматически разрешённых.
    Allowlist,
    /// Человек подтвердил этот конкретный вызов.
    Human,
}

impl Approval {
    pub fn as_str(self) -> &'static str {
        match self {
            Approval::Allowlist => "allowlist",
            Approval::Human => "human",
        }
    }
}

/// Вызов, который разрешено исполнить.
///
/// Поля приватны, публичного конструктора нет. Единственные способы
/// получить значение — [`decide`] (нашла имя в allowlist) и
/// [`approve_by_human`] (человек ответил «да»). Всё остальное
/// невозможно выразить: сырой [`ToolCall`] в точку исполнения не подходит
/// по типу, а собрать структуру снаружи не даёт приватность полей.
pub struct ApprovedToolCall {
    call: ToolCall,
    approved_by: Approval,
}

impl ApprovedToolCall {
    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    pub fn approved_by(&self) -> Approval {
        self.approved_by
    }
}

/// Что делать с вызовом, который запросила модель.
pub enum Decision {
    /// Исполнять можно.
    Approved(ApprovedToolCall),
    /// Нужно спросить человека. Вызов возвращается как есть — исполнить его
    /// нельзя, пока он не пройдёт через [`approve_by_human`].
    NeedsHuman(ToolCall),
}

/// Решает судьбу вызова по списку автоматически разрешённых имён.
///
/// Умолчание — спрашивать. Пустой список означает «подтверждать всё», и
/// это правильное умолчание: система, которая по умолчанию исполняет,
/// однажды исполнит не то.
///
/// Сравнение имён точное. Ни префиксов, ни масок, ни регистронезависимости:
/// маска `get_*` разрешила бы `get_viewport_screenshot`, а разница между
/// `execute_blender_code` и `Execute_Blender_Code` — ровно тот зазор, в
/// который проходит то, чего не разрешали.
pub fn decide(auto_approve: &[String], call: ToolCall) -> Decision {
    if auto_approve.iter().any(|name| name == &call.name) {
        Decision::Approved(ApprovedToolCall {
            call,
            approved_by: Approval::Allowlist,
        })
    } else {
        Decision::NeedsHuman(call)
    }
}

/// Человек подтвердил конкретный вызов.
///
/// Отдельная функция, а не флаг в [`decide`]: подтверждение человека — это
/// событие, а не настройка, и путать их нельзя. Вызывается только из
/// маршрута подтверждения, куда человек пришёл своими руками.
pub fn approve_by_human(call: ToolCall) -> ApprovedToolCall {
    ApprovedToolCall {
        call,
        approved_by: Approval::Human,
    }
}

/// Текст, который показывается человеку перед решением.
///
/// Аргументы приводятся ЦЕЛИКОМ. Спецификация MCP требует показать их до
/// отправки, а молчаливое обрезание — худший из возможных компромиссов:
/// решение принимается по тому, что видно, и вредное отличие может лежать
/// ровно в отрезанном хвосте. Если аргументы длинные — они длинные, и
/// человек увидит их длину.
pub fn describe(server: &str, call: &ToolCall) -> String {
    format!(
        "Требуется подтверждение вызова инструмента.\n\
         Сервер: {server}\n\
         Инструмент: {}\n\
         Аргументы ({} символов):\n{}\n\n\
         Подтвердить: POST /tasks/<id>/tool-decision {{\"approve\": true}}\n\
         Отказать:    POST /tasks/<id>/tool-decision {{\"approve\": false}}",
        call.name,
        call.arguments.chars().count(),
        call.arguments
    )
}

/// Куда пишется журнал вызовов.
///
/// Рядом с `.kaic/telegram-audit.log`, но отдельным файлом: предмет другой и
/// поля другие, а смешивать в один журнал две разные сущности — значит
/// сделать его нечитаемым для обоих вопросов. `.kaic/` не под версионным
/// контролем (см. .gitignore).
pub const AUDIT_LOG_PATH: &str = ".kaic/tool-audit.log";

/// Чем закончился вызов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Executed,
    Failed,
    /// Человек отказал — вызова не было вовсе.
    DeniedByHuman,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Executed => "executed",
            Outcome::Failed => "failed",
            Outcome::DeniedByHuman => "denied_by_human",
        }
    }
}

/// Дописывает строку в append-only журнал вызовов инструментов.
///
/// **JSON Lines, а не TSV, как у Telegram-моста.** У того поля короткие и
/// свои: идентификатор задачи, хэш, флаг. Здесь в строку идут аргументы,
/// сочинённые моделью, — с переводами строк, табуляциями и кавычками внутри.
/// TSV потребовал бы экранирования, а ручное экранирование в этом проекте
/// уже дважды оказывалось дефектом (`textfile=` вместо экранирования в
/// ffmpeg). Сериализатор делает это правильно и без нашего участия.
///
/// Только дозапись: `append(true)`, никогда не перезапись и не усечение.
/// Отказы человека пишутся наравне с исполнениями — журнал без них не
/// отвечает на вопрос «почему задача не сделала то, что просили», а это
/// ровно тот вопрос, ради которого его читают.
pub fn append_audit(
    path: impl AsRef<std::path::Path>,
    task_id: &str,
    server: &str,
    call: &ToolCall,
    approved_by: Option<Approval>,
    outcome: Outcome,
    result: &str,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;

    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать {}", parent.display()))?;
    }

    let record = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "task": task_id,
        "server": server,
        "tool": call.name,
        "tool_call_id": call.id,
        // Целиком: журнал, в котором аргументы обрезаны, не годится для
        // разбора инцидента — а именно для него он и существует.
        "arguments": call.arguments,
        "approved_by": approved_by.map(|a| a.as_str()),
        "outcome": outcome.as_str(),
        "result": result,
    });

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("не удалось открыть журнал {}", path.display()))?;
    writeln!(file, "{record}").context("не удалось записать строку журнала")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    #[test]
    fn an_empty_allowlist_asks_about_everything() {
        // Умолчание конфига — пустой список. Система, которая по умолчанию
        // исполняет, однажды исполнит не то.
        assert!(matches!(
            decide(&[], call("get_scene_info")),
            Decision::NeedsHuman(_)
        ));
    }

    #[test]
    fn only_an_exact_name_passes() {
        let allow = vec!["get_scene_info".to_string()];
        assert!(matches!(
            decide(&allow, call("get_scene_info")),
            Decision::Approved(_)
        ));
        // Другой инструмент того же сервера — не разрешён.
        assert!(matches!(
            decide(&allow, call("execute_blender_code")),
            Decision::NeedsHuman(_)
        ));
    }

    #[test]
    fn no_name_heuristics_whatsoever() {
        // Ни префикса, ни маски, ни регистра. `get_` не означает
        // безопасность: get_viewport_screenshot читает экран пользователя.
        let allow = vec!["get_scene_info".to_string()];
        for probe in [
            "get_viewport_screenshot",
            "get_",
            "Get_Scene_Info",
            "GET_SCENE_INFO",
            "get_scene_info_2",
            " get_scene_info",
        ] {
            assert!(
                matches!(decide(&allow, call(probe)), Decision::NeedsHuman(_)),
                "'{probe}' прошёл без подтверждения"
            );
        }
    }

    #[test]
    fn who_approved_is_recorded() {
        let allow = vec!["get_scene_info".to_string()];
        let Decision::Approved(approved) = decide(&allow, call("get_scene_info")) else {
            panic!("должно было разрешиться");
        };
        assert_eq!(approved.approved_by(), Approval::Allowlist);
        assert_eq!(approve_by_human(call("что_угодно")).approved_by(), Approval::Human);
    }

    #[test]
    fn a_human_refusal_is_written_to_the_journal_too() {
        // Журнал без отказов не отвечает на вопрос «почему задача не сделала
        // то, что просили» — а читают его ровно за этим.
        let dir = std::env::temp_dir().join(format!("kaic-audit-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("tool-audit.log");

        let mut c = call("execute_blender_code");
        c.arguments = "{\"code\":\"import os\\nos.remove('x')\"}".to_string();

        append_audit(&path, "task-1", "BlenderMCP", &c, None, Outcome::DeniedByHuman, "отказ")
            .unwrap();
        append_audit(
            &path,
            "task-1",
            "BlenderMCP",
            &call("get_scene_info"),
            Some(Approval::Allowlist),
            Outcome::Executed,
            "ok",
        )
        .unwrap();

        let lines: Vec<&str> = {
            let raw = std::fs::read_to_string(&path).unwrap();
            Box::leak(raw.into_boxed_str()).lines().collect()
        };
        assert_eq!(lines.len(), 2, "журнал только дозаписывается");

        let denied: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(denied["outcome"], "denied_by_human");
        assert_eq!(denied["approved_by"], serde_json::Value::Null);
        assert_eq!(denied["tool"], "execute_blender_code");
        // Аргументы целиком, включая переводы строк — то, ради чего JSON, а
        // не TSV: экранировать руками здесь было бы дефектом.
        assert_eq!(denied["arguments"], c.arguments);

        let executed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(executed["outcome"], "executed");
        assert_eq!(executed["approved_by"], "allowlist");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_human_sees_the_arguments_whole() {
        // Обрезание аргументов — худший компромисс: решение принимается по
        // тому, что видно, а вредное отличие может лежать в хвосте.
        let long = format!("{{\"code\":\"{}\"}}", "x".repeat(4000));
        let mut c = call("execute_blender_code");
        c.arguments = long.clone();

        let text = describe("BlenderMCP", &c);
        assert!(text.contains(&long), "аргументы обрезаны");
        assert!(text.contains("execute_blender_code"));
        assert!(text.contains("BlenderMCP"));
        // Длина названа явно, чтобы человек видел масштаб до чтения.
        assert!(text.contains(&long.chars().count().to_string()));
    }
}
