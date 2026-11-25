//! Непроверенные строки никогда не попадают в терминал как управляющие последовательности.
use crate::protocol::ToolApproval;
use std::fmt::Write;

/// Сохраняет видимые Unicode-символы; управляющие и невидимые форматы показывает явно.
/// Обратная косая черта удваивается: буквальное `\\u{001b}` отлично от настоящего ESC.
pub fn sanitize(input: &str) -> String {
    sanitize_mode(input, false)
}

fn sanitize_mode(input: &str, exact: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' if !exact => output.push('\n'),
            '\t' if !exact => output.push_str("    "),
            '\\' => output.push_str("\\\\"),
            ch if ch.is_control() || is_invisible_format(ch) => {
                let _ = write!(output, "\\u{{{:04x}}}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output
}

fn is_invisible_format(ch: char) -> bool {
    matches!(ch as u32,
        0x00ad | 0x034f | 0x061c | 0x115f..=0x1160 | 0x17b4..=0x17b5 |
        0x180b..=0x180f | 0x200b..=0x200f | 0x2028..=0x202e |
        0x2060..=0x206f | 0x3164 | 0xfe00..=0xfe0f | 0xfeff | 0xffa0 |
        0xfff9..=0xfffb | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a |
        0xe0000..=0xe0fff)
}

/// Полный снимок: никакого JSON pretty-print/усечения/нормализации исходных аргументов.
pub fn approval_text(snapshot: &ToolApproval) -> String {
    let mut output = format!(
        "Запрос: {}\nЗадача: {}\nИсполнение: {}\nВызов: {}\nСервер: {}\nКоманда: {}\n",
        snapshot.request_id,
        snapshot.task_id,
        snapshot.execution_id,
        exact(&snapshot.tool_call_id),
        exact(&snapshot.server.peer_name),
        exact(&snapshot.server.command),
    );
    for (index, argument) in snapshot.server.args.iter().enumerate() {
        let _ = writeln!(
            output,
            "Сервер arg[{index}] ({} байт): {}",
            argument.len(),
            exact(argument)
        );
    }
    let _ = write!(
        output,
        "Инструмент: {}\nАргументы ({} байт, исходная строка):\n{}",
        exact(&snapshot.tool_name),
        snapshot.arguments.len(),
        exact(&snapshot.arguments),
    );
    output
}

/// В подтверждении также раскрываем TAB/newline/CR: однозначное представление байтов строки.
fn exact(input: &str) -> String {
    sanitize_mode(input, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_sequences_and_unicode_formats() {
        assert_eq!(
            sanitize("Привет\x1b[2J\r\u{009b}\u{202e}\u{200d}🧱"),
            "Привет\\u{001b}[2J\\u{000d}\\u{009b}\\u{202e}\\u{200d}🧱"
        );
        assert_ne!(sanitize("\x1b"), sanitize("\\u{001b}"));
        assert_eq!(sanitize("a\nb\tc"), "a\nb    c");
    }

    #[test]
    fn approval_contains_entire_original_target_without_mutation_or_truncation() {
        let snapshot = ToolApproval {
            request_id: uuid::Uuid::from_u128(1),
            task_id: uuid::Uuid::from_u128(2),
            execution_id: uuid::Uuid::from_u128(3),
            tool_call_id: "call-4".into(),
            tool_name: "write\nforged-label".into(),
            arguments: format!("\t{}\n", "я".repeat(20_000)),
            server: crate::protocol::ToolServerIdentity {
                command: "local-mcp".into(),
                args: vec!["--first".into(), "two\twords".into()],
                peer_name: "files".into(),
            },
        };
        let original = snapshot.clone();
        let text = approval_text(&snapshot);
        assert!(text.contains(&snapshot.request_id.to_string()));
        assert!(text.contains(&snapshot.task_id.to_string()));
        assert!(text.contains(&snapshot.execution_id.to_string()));
        assert!(text.contains("call-4") && text.contains("local-mcp") && text.contains("files"));
        assert!(text.contains("arg[0]") && text.contains("arg[1]"));
        assert!(text.contains("two\\u{0009}words"));
        assert!(text.contains("write\\u{000a}forged-label"));
        assert!(text.contains("40002 байт"));
        assert!(text.ends_with(&format!("\\u{{0009}}{}\\u{{000a}}", "я".repeat(20_000))));
        assert_eq!(snapshot, original);
    }
}
