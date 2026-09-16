//! Минималистичный terminal-клиент штатного KAIC pipeline.

use std::{
    future::Future,
    io::{self, IsTerminal, Read, Write},
    process::{Command, Stdio},
};

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::control_center_api::{
    run_terminal_turn_interruptible_with_model, AppState, TerminalTurnOutcome,
};
use crate::task_store::TaskStatus;

const KAIC_RED: &str = "1;38;5;196";
const KAIC_ORANGE: &str = "38;5;202";
const ACTIVITY_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACTIVITY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);
const TERMINAL_COMMANDS: &[&str] = &["/clear", "/exit", "/help", "/models", "/quit", "/status"];
const BANNER: &str = r#"██╗  ██╗ █████╗ ██╗ ██████╗
██║ ██╔╝██╔══██╗██║██╔════╝
█████╔╝ ███████║██║██║
██╔═██╗ ██╔══██║██║██║
██║  ██╗██║  ██║██║╚██████╗
╚═╝  ╚═╝╚═╝  ╚═╝╚═╝ ╚═════╝"#;

#[derive(Debug, PartialEq, Eq)]
enum TerminalInput<'a> {
    Empty,
    Exit,
    Help,
    Status,
    Models,
    RemovedModelCommand,
    Clear,
    User(&'a str),
}

fn parse_input(line: &str) -> TerminalInput<'_> {
    let input = line.trim();
    if input.is_empty() {
        TerminalInput::Empty
    } else if input.eq_ignore_ascii_case("exit")
        || input.eq_ignore_ascii_case("quit")
        || input.eq_ignore_ascii_case("/exit")
        || input.eq_ignore_ascii_case("/quit")
    {
        TerminalInput::Exit
    } else if input.eq_ignore_ascii_case("/help") {
        TerminalInput::Help
    } else if input.eq_ignore_ascii_case("/status") {
        TerminalInput::Status
    } else if input.eq_ignore_ascii_case("/models") {
        TerminalInput::Models
    } else if input.eq_ignore_ascii_case("/model") {
        TerminalInput::RemovedModelCommand
    } else if let Some((command, _)) = input.split_once(char::is_whitespace) {
        if command.eq_ignore_ascii_case("/model") {
            TerminalInput::RemovedModelCommand
        } else {
            TerminalInput::User(input)
        }
    } else if input.eq_ignore_ascii_case("/clear") {
        TerminalInput::Clear
    } else {
        TerminalInput::User(input)
    }
}

fn styled(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn is_terminal_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

fn is_safe_input_character(character: char) -> bool {
    !character.is_control()
        && character != '\u{007f}'
        && !('\u{0080}'..='\u{009f}').contains(&character)
        && !is_terminal_format_control(character)
}

fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| {
            *character == '\n' || *character == '\t' || is_safe_input_character(*character)
        })
        .collect()
}

fn sanitize_terminal_line(text: &str) -> String {
    text.chars()
        .filter(|character| is_safe_input_character(*character))
        .collect()
}

fn should_use_interactive_editor(stdin_terminal: bool, stdout_terminal: bool) -> bool {
    stdin_terminal && stdout_terminal
}

fn fixed_width_field(text: &str, width: usize) -> String {
    let safe = sanitize_terminal_line(text);
    let mut field: String = safe.chars().take(width).collect();
    let padding = width.saturating_sub(field.chars().count());
    field.extend(std::iter::repeat_n(' ', padding));
    field
}

fn completion_candidates(line: &str, cursor: usize, _models: &[String]) -> (usize, Vec<String>) {
    let Some(before_cursor) = line.get(..cursor) else {
        return (0, Vec::new());
    };
    if before_cursor.starts_with('/') && !before_cursor.chars().any(char::is_whitespace) {
        let prefix = before_cursor.to_lowercase();
        return (
            0,
            TERMINAL_COMMANDS
                .iter()
                .filter(|command| command.starts_with(&prefix))
                .map(|command| (*command).to_string())
                .collect(),
        );
    }
    (0, Vec::new())
}

fn apply_completion(
    line: &mut String,
    cursor: usize,
    models: &[String],
    selected: usize,
) -> Option<usize> {
    let (start, candidates) = completion_candidates(line, cursor, models);
    let replacement = candidates.get(selected.min(candidates.len().checked_sub(1)?))?;
    line.replace_range(start..cursor, replacement);
    Some(start + replacement.len())
}

fn render_activity_frame(index: usize, color: bool) -> String {
    let frame = ACTIVITY_FRAMES[index % ACTIVITY_FRAMES.len()];
    styled(color, KAIC_ORANGE, &format!("{frame} Выполняю… "))
}

struct ActivityIndicator {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    clear_width: usize,
}

impl ActivityIndicator {
    fn start() -> io::Result<Self> {
        let first = render_activity_frame(0, true);
        let clear_width = visible_width(&first);
        print!("\n{first}");
        io::stdout().flush()?;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut frame = 1;
            while !worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(ACTIVITY_INTERVAL);
                if worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                print!("\r{}", render_activity_frame(frame, true));
                let _ = io::stdout().flush();
                frame = (frame + 1) % ACTIVITY_FRAMES.len();
            }
        });

        Ok(Self {
            stop,
            worker: Some(worker),
            clear_width,
        })
    }
}

impl Drop for ActivityIndicator {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        print!("\r{}\r", " ".repeat(self.clear_width));
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
fn render_activity(color: bool) -> String {
    format!("\n{}\n", render_activity_frame(0, color))
}

fn task_id_after_interruption(_task_id: Option<uuid::Uuid>) -> Option<uuid::Uuid> {
    None
}

fn selected_model_name(selected_model: Option<&str>) -> &str {
    selected_model.unwrap_or("auto (Scheduler)")
}

fn render_startup(backend: &str, selected_model: Option<&str>, color: bool) -> String {
    let title = styled(color, KAIC_RED, BANNER);
    let heading = styled(
        color,
        "1;37",
        &format!("KAIC {}", env!("CARGO_PKG_VERSION")),
    );
    let backend = fixed_width_field(backend, 43);
    let model = fixed_width_field(selected_model_name(selected_model), 43);
    let session = fixed_width_field("new", 43);
    let accent = |text: &str| styled(color, KAIC_ORANGE, text);
    format!(
        "{title}\n\n\
         ╭─ {heading} ─────────────────────────────────────────╮\n\
         │  {}  {backend}│\n\
         │  {}  {model}│\n\
         │  {}  {session}│\n\
         ╰──────────────────────────────────────────────────────╯\n\n\
         Добро пожаловать в KAIC. Напишите задачу или {}.\n\
         ✦ Tip: подсказки появляются при вводе; ↑/↓ выбирают, Tab вставляет; Ctrl+C отменяет ход.\n\n",
        accent("Backend"),
        accent("Model  "),
        accent("Session"),
        accent("/help")
    )
}

fn render_status(
    backend: &str,
    selected_model: Option<&str>,
    task_id: Option<uuid::Uuid>,
    color: bool,
) -> String {
    let backend = styled(color, KAIC_ORANGE, &sanitize_terminal_line(backend));
    let model = sanitize_terminal_line(selected_model_name(selected_model));
    let session = task_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "new".to_string());
    format!(
        " {} │ model {} │ session {} │ /help\n{}\n{} ",
        backend,
        model,
        session,
        styled(
            color,
            "90",
            "────────────────────────────────────────────────────────────",
        ),
        styled(color, KAIC_RED, "❯")
    )
}

fn render_help(color: bool) -> String {
    format!(
        "\n{}\n  /models          открыть список и выбор модели\n  /status          backend, модель и сессия\n  /clear           очистить экран\n  /exit            завершить KAIC\n  ↑/↓              выбрать подсказку\n  Tab              вставить подсказку\n  Ctrl+C           отменить ход или выйти\n",
        styled(color, KAIC_RED, "Команды KAIC")
    )
}

fn terminal_model_root() -> std::path::PathBuf {
    std::env::var_os("KAIC_MODEL_DIR")
        .or_else(|| std::env::var_os("KAIC_EMBEDDED_MODEL_PATH"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/mnt/storage/Models"))
}

fn local_model_labels(
    registry: &crate::resource_registry::ResourceRegistry,
    backend: &str,
    model_root: &std::path::Path,
) -> Vec<String> {
    let canonical_root = model_root.canonicalize().ok();
    let mut models = registry
        .all()
        .filter_map(|(label, resource)| {
            let usable = if backend == "embedded" {
                let root = canonical_root.as_ref()?;
                let path = std::path::Path::new(resource.path.as_deref()?);
                let metadata = path.symlink_metadata().ok()?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return None;
                }
                let canonical_path = path.canonicalize().ok()?;
                canonical_path.starts_with(root)
                    && canonical_path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
            } else {
                resource
                    .lm_studio_key
                    .as_deref()
                    .is_some_and(|key| !key.trim().is_empty())
            };
            usable.then(|| label.clone())
        })
        .collect::<Vec<_>>();
    models.sort_by_key(|model| model.to_lowercase());
    models
}

fn render_completion_popup(candidates: &[String], selected: usize, color: bool) -> Vec<String> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let candidate = sanitize_terminal_line(candidate);
            if index == selected {
                styled(color, KAIC_ORANGE, &format!("  › {candidate}"))
            } else {
                format!("    {candidate}")
            }
        })
        .collect()
}

fn render_models(models: &[String], selected_model: Option<&str>, color: bool) -> String {
    let mut output = format!("\n{}\n", styled(color, KAIC_RED, "Модели KAIC"));
    let auto_marker = if selected_model.is_none() {
        "●"
    } else {
        "·"
    };
    output.push_str(&format!("  {auto_marker} auto (Scheduler)\n"));
    for model in models {
        let marker = if selected_model == Some(model.as_str()) {
            "●"
        } else {
            "·"
        };
        output.push_str(&format!("  {marker} {}\n", sanitize_terminal_line(model)));
    }
    output.push_str(
        "\n  Настроены в Resource Registry; доступность проверяется при запуске.\n  Выбор: /models\n",
    );
    output
}

fn model_picker_choices(models: &[String]) -> Vec<Option<&str>> {
    std::iter::once(None)
        .chain(models.iter().map(|model| Some(model.as_str())))
        .collect()
}

fn move_picker_selection(selected: usize, action: EscapeAction, choice_count: usize) -> usize {
    if choice_count == 0 {
        return 0;
    }
    match action {
        EscapeAction::Up => (selected + choice_count - 1) % choice_count,
        EscapeAction::Down => (selected + 1) % choice_count,
        _ => selected.min(choice_count - 1),
    }
}

fn render_model_picker(models: &[String], selected: usize, color: bool) -> String {
    let mut output = format!("{}\n", styled(color, KAIC_RED, "Выберите модель"));
    for (index, choice) in model_picker_choices(models).into_iter().enumerate() {
        let marker = if index == selected { "●" } else { "·" };
        let label = choice
            .map(sanitize_terminal_line)
            .unwrap_or_else(|| "auto (Scheduler)".to_string());
        output.push_str(&format!("  {marker} {label}\n"));
    }
    output.push_str("  ↑/↓ выбрать · Enter/Tab подтвердить · Esc отменить");
    output
}

fn model_picker_physical_rows(rendered: &str, terminal_width: usize) -> usize {
    rendered
        .lines()
        .map(|line| occupied_rows(visible_width(line), terminal_width))
        .sum::<usize>()
        .max(1)
}

fn redraw_model_picker(rendered: &str, previous_rows: &mut usize) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if *previous_rows > 0 {
        stdout.write_all(b"\r")?;
        if *previous_rows > 1 {
            write!(stdout, "\x1b[{}A", *previous_rows - 1)?;
        }
        for row in 0..*previous_rows {
            stdout.write_all(b"\x1b[2K")?;
            if row + 1 < *previous_rows {
                stdout.write_all(b"\x1b[1B\r")?;
            }
        }
        if *previous_rows > 1 {
            write!(stdout, "\x1b[{}A", *previous_rows - 1)?;
        }
    }

    let lines = rendered.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        write!(stdout, "\r{line}")?;
        if index + 1 < lines.len() {
            stdout.write_all(b"\r\n")?;
        }
    }
    *previous_rows = model_picker_physical_rows(rendered, terminal_width());
    stdout.flush()
}

#[derive(Debug, PartialEq, Eq)]
enum PickerOutcome {
    Selected(Option<String>),
    Cancelled,
    Interrupted,
    Terminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawRead {
    Byte,
    Retry,
    Terminated,
}

fn read_byte_or_termination<R: Read>(
    reader: &mut R,
    byte: &mut [u8; 1],
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<RawRead> {
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(RawRead::Terminated);
    }
    if reader.read(byte)? == 0 {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(RawRead::Terminated)
        } else {
            Ok(RawRead::Retry)
        }
    } else {
        Ok(RawRead::Byte)
    }
}

fn read_interactive_model_picker_blocking(
    models: &[String],
    current: Option<&str>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<PickerOutcome> {
    let _raw = RawTerminalGuard::enter()?;
    let choices = model_picker_choices(models);
    let mut selected = current
        .and_then(|current| {
            choices
                .iter()
                .position(|choice| choice.is_some_and(|model| model.eq_ignore_ascii_case(current)))
        })
        .unwrap_or(0);
    let mut displayed_rows = 0usize;
    redraw_model_picker(
        &render_model_picker(models, selected, true),
        &mut displayed_rows,
    )?;
    let mut stdin = io::stdin().lock();
    let mut byte = [0_u8; 1];
    loop {
        match read_byte_or_termination(&mut stdin, &mut byte, cancelled)? {
            RawRead::Retry => continue,
            RawRead::Terminated => return Ok(PickerOutcome::Terminated),
            RawRead::Byte => {}
        }
        match byte[0] {
            b'\r' | b'\n' | b'\t' => {
                return Ok(PickerOutcome::Selected(
                    choices[selected].map(str::to_string),
                ));
            }
            3 => return Ok(PickerOutcome::Interrupted),
            27 => {
                let action = classify_escape_sequence(&read_escape_sequence(&mut stdin)?);
                match action {
                    EscapeAction::Dismiss => return Ok(PickerOutcome::Cancelled),
                    EscapeAction::Up | EscapeAction::Down => {
                        selected = move_picker_selection(selected, action, choices.len());
                        redraw_model_picker(
                            &render_model_picker(models, selected, true),
                            &mut displayed_rows,
                        )?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

async fn read_interactive_model_picker(
    models: Vec<String>,
    current: Option<String>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<PickerOutcome> {
    let worker_cancelled = cancelled.clone();
    tokio::task::spawn_blocking(move || {
        read_interactive_model_picker_blocking(
            &models,
            current.as_deref(),
            worker_cancelled.as_ref(),
        )
    })
    .await
    .map_err(io::Error::other)?
}

#[derive(Debug, PartialEq, Eq)]
enum ReadOutcome {
    Line,
    Eof,
    Interrupted,
    Terminated,
}

struct RawTerminalGuard {
    original_state: String,
    bracketed_paste_enabled: bool,
}

fn enable_bracketed_paste<W: Write>(writer: &mut W, enabled: &mut bool) -> io::Result<()> {
    // Arm cleanup before the first fallible write: partial output may already enable the mode.
    *enabled = true;
    writer.write_all(b"\x1b[?2004h")?;
    writer.flush()
}

impl RawTerminalGuard {
    fn enter() -> io::Result<Self> {
        let state = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()?;
        if !state.status.success() {
            return Err(io::Error::other("stty -g завершился с ошибкой"));
        }
        let original_state = String::from_utf8(state.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .trim()
            .to_string();
        let status = Command::new("stty")
            .args(["raw", "-echo", "min", "0", "time", "1"])
            .stdin(Stdio::inherit())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("не удалось включить raw terminal mode"));
        }
        let mut guard = Self {
            original_state,
            bracketed_paste_enabled: false,
        };
        {
            let mut stdout = io::stdout().lock();
            enable_bracketed_paste(&mut stdout, &mut guard.bracketed_paste_enabled)?;
        }
        Ok(guard)
    }
}

fn cleanup_raw_terminal<W: Write, F: FnOnce()>(
    writer: &mut W,
    bracketed_paste_enabled: bool,
    restore: F,
) {
    if bracketed_paste_enabled {
        let _ = writer.write_all(b"\x1b[?2004l");
        let _ = writer.flush();
    }
    restore();
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout().lock();
        cleanup_raw_terminal(&mut stdout, self.bracketed_paste_enabled, || {
            let _ = Command::new("stty")
                .arg(&self.original_state)
                .stdin(Stdio::inherit())
                .status();
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeAction {
    PasteStart,
    PasteEnd,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Dismiss,
    Ignore,
}

fn classify_escape_sequence(sequence: &[u8]) -> EscapeAction {
    match sequence {
        b"[200~" => EscapeAction::PasteStart,
        b"[201~" => EscapeAction::PasteEnd,
        b"[A" | b"OA" => EscapeAction::Up,
        b"[B" | b"OB" => EscapeAction::Down,
        b"[D" | b"OD" => EscapeAction::Left,
        b"[C" | b"OC" => EscapeAction::Right,
        b"[H" | b"OH" | b"[1~" | b"[7~" => EscapeAction::Home,
        b"[F" | b"OF" | b"[4~" | b"[8~" => EscapeAction::End,
        b"" => EscapeAction::Dismiss,
        _ => EscapeAction::Ignore,
    }
}

fn read_escape_sequence<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut sequence = Vec::with_capacity(8);
    let mut byte = [0_u8; 1];
    if reader.read(&mut byte)? == 0 {
        return Ok(sequence);
    }
    sequence.push(byte[0]);
    match byte[0] {
        b'[' => {
            while sequence.len() < 32 {
                if reader.read(&mut byte)? == 0 {
                    break;
                }
                sequence.push(byte[0]);
                if (0x40..=0x7e).contains(&byte[0]) {
                    break;
                }
            }
        }
        b'O' => {
            if reader.read(&mut byte)? != 0 {
                sequence.push(byte[0]);
            }
        }
        b']' => {
            let mut escaped = false;
            while sequence.len() < 4096 {
                if reader.read(&mut byte)? == 0 {
                    break;
                }
                sequence.push(byte[0]);
                if byte[0] == 0x07 || (escaped && byte[0] == b'\\') {
                    break;
                }
                escaped = byte[0] == 27;
            }
        }
        _ => {}
    }
    Ok(sequence)
}

fn read_utf8_character<R: Read>(reader: &mut R, first: u8) -> io::Result<Option<char>> {
    let width = if first < 0x80 {
        1
    } else if first & 0b1110_0000 == 0b1100_0000 {
        2
    } else if first & 0b1111_0000 == 0b1110_0000 {
        3
    } else if first & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid UTF-8 input",
        ));
    };
    let mut bytes = vec![first];
    let mut byte = [0_u8; 1];
    while bytes.len() < width {
        if reader.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete UTF-8 input",
            ));
        }
        bytes.push(byte[0]);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(text.chars().next())
}

fn previous_char_boundary(line: &str, cursor: usize) -> usize {
    line.get(..cursor)
        .and_then(|prefix| prefix.char_indices().next_back().map(|(index, _)| index))
        .unwrap_or(0)
}

fn next_char_boundary(line: &str, cursor: usize) -> usize {
    line.get(cursor..)
        .and_then(|suffix| {
            suffix
                .char_indices()
                .nth(1)
                .map(|(index, _)| cursor + index)
        })
        .unwrap_or(line.len())
}

fn move_cursor(line: &str, cursor: usize, action: EscapeAction) -> usize {
    match action {
        EscapeAction::Left => previous_char_boundary(line, cursor),
        EscapeAction::Right => next_char_boundary(line, cursor),
        EscapeAction::Home => 0,
        EscapeAction::End => line.len(),
        _ => cursor,
    }
}

fn insert_at_cursor(line: &mut String, cursor: &mut usize, character: char) {
    line.insert(*cursor, character);
    *cursor += character.len_utf8();
}

fn backspace_at_cursor(line: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 || !line.is_char_boundary(*cursor) {
        return false;
    }
    let previous = previous_char_boundary(line, *cursor);
    line.replace_range(previous..*cursor, "");
    *cursor = previous;
    true
}

fn terminal_cell_width(character: char) -> usize {
    let code = character as u32;
    if matches!(
        code,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06ed
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0xe0100..=0xe01ef
    ) {
        0
    } else if matches!(
        code,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

fn visible_width(text: &str) -> usize {
    #[derive(Clone, Copy)]
    enum EscapeState {
        Text,
        Escape,
        Csi,
        Osc,
    }

    let mut state = EscapeState::Text;
    let mut width = 0;
    for character in text.chars() {
        state = match state {
            EscapeState::Text if character == '\u{1b}' => EscapeState::Escape,
            EscapeState::Text => {
                if !character.is_control() && !is_terminal_format_control(character) {
                    width += terminal_cell_width(character);
                }
                EscapeState::Text
            }
            EscapeState::Escape if character == '[' => EscapeState::Csi,
            EscapeState::Escape if character == ']' => EscapeState::Osc,
            EscapeState::Escape => EscapeState::Text,
            EscapeState::Csi if ('@'..='~').contains(&character) => EscapeState::Text,
            EscapeState::Csi => EscapeState::Csi,
            EscapeState::Osc if character == '\u{7}' => EscapeState::Text,
            EscapeState::Osc => EscapeState::Osc,
        };
    }
    width
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditorLayout {
    editor_rows: usize,
    popup_rows: usize,
    total_rows: usize,
    cursor_row: usize,
    cursor_column: usize,
}

fn occupied_rows(cells: usize, terminal_width: usize) -> usize {
    let width = terminal_width.max(1);
    cells.saturating_sub(1) / width + 1
}

fn editor_layout(
    prompt: &str,
    line: &str,
    cursor: usize,
    candidates: &[String],
    terminal_width: usize,
) -> EditorLayout {
    let width = terminal_width.max(1);
    let prompt_width = visible_width(prompt);
    let line_width = visible_width(line);
    let cursor_cells = prompt_width + visible_width(line.get(..cursor).unwrap_or(line));
    let editor_rows = occupied_rows(prompt_width + line_width, width);
    let popup_rows = render_completion_popup(candidates, 0, true)
        .iter()
        .map(|row| occupied_rows(visible_width(row), width))
        .sum();
    EditorLayout {
        editor_rows,
        popup_rows,
        total_rows: editor_rows + popup_rows,
        cursor_row: cursor_cells.saturating_sub(1) / width,
        cursor_column: if cursor_cells > 0 && cursor_cells % width == 0 {
            width - 1
        } else {
            cursor_cells % width
        },
    }
}

fn terminal_width() -> usize {
    Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|size| size.split_whitespace().nth(1)?.parse().ok())
        .filter(|width| *width > 0)
        .unwrap_or(80)
}

fn redraw_editor(
    prompt: &str,
    line: &str,
    cursor: usize,
    candidates: &[String],
    selected: usize,
    previous: &mut EditorLayout,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "\r")?;
    if previous.cursor_row > 0 {
        write!(stdout, "\x1b[{}A", previous.cursor_row)?;
    }
    for row in 0..previous.total_rows {
        stdout.write_all(b"\x1b[2K")?;
        if row + 1 < previous.total_rows {
            stdout.write_all(b"\x1b[1B\r")?;
        }
    }
    if previous.total_rows > 1 {
        write!(stdout, "\x1b[{}A", previous.total_rows - 1)?;
    }
    write!(stdout, "\r{prompt}{line}")?;

    let rows = render_completion_popup(candidates, selected, true);
    for row in &rows {
        write!(stdout, "\n\r\x1b[2K{row}")?;
    }
    let layout = editor_layout(prompt, line, cursor, candidates, terminal_width());
    let rows_below_cursor = layout.total_rows - 1 - layout.cursor_row;
    if rows_below_cursor > 0 {
        write!(stdout, "\x1b[{}A", rows_below_cursor)?;
    }
    write!(stdout, "\r")?;
    if layout.cursor_column > 0 {
        write!(stdout, "\x1b[{}C", layout.cursor_column)?;
    }
    *previous = layout;
    stdout.flush()
}

fn read_interactive_line_blocking(
    models: &[String],
    prompt: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> io::Result<(ReadOutcome, String)> {
    let _raw = RawTerminalGuard::enter()?;
    let mut stdin = io::stdin().lock();
    let mut line = String::new();
    let mut byte = [0_u8; 1];
    let mut bracketed_paste = false;
    let mut selected = 0usize;
    let mut cursor = 0usize;
    let mut displayed = editor_layout(prompt, &line, cursor, &[], terminal_width());

    loop {
        match read_byte_or_termination(&mut stdin, &mut byte, cancelled)? {
            RawRead::Retry => continue,
            RawRead::Terminated => return Ok((ReadOutcome::Terminated, String::new())),
            RawRead::Byte => {}
        }
        match byte[0] {
            b'\r' | b'\n' if bracketed_paste => {
                if !line
                    .get(..cursor)
                    .is_some_and(|prefix| prefix.ends_with(' '))
                {
                    insert_at_cursor(&mut line, &mut cursor, ' ');
                    selected = 0;
                    let candidates = completion_candidates(&line, cursor, models).1;
                    redraw_editor(prompt, &line, cursor, &candidates, selected, &mut displayed)?;
                }
            }
            b'\r' | b'\n' => {
                redraw_editor(prompt, &line, cursor, &[], 0, &mut displayed)?;
                print!("\r\n");
                io::stdout().flush()?;
                return Ok((ReadOutcome::Line, line));
            }
            3 if !bracketed_paste => {
                redraw_editor(prompt, &line, cursor, &[], 0, &mut displayed)?;
                print!("^C\r\n");
                io::stdout().flush()?;
                return Ok((ReadOutcome::Interrupted, String::new()));
            }
            4 if !bracketed_paste && line.is_empty() => {
                redraw_editor(prompt, &line, cursor, &[], 0, &mut displayed)?;
                return Ok((ReadOutcome::Eof, line));
            }
            8 | 127 if !bracketed_paste => {
                if backspace_at_cursor(&mut line, &mut cursor) {
                    selected = 0;
                    let candidates = completion_candidates(&line, cursor, models).1;
                    redraw_editor(prompt, &line, cursor, &candidates, selected, &mut displayed)?;
                }
            }
            b'\t' if !bracketed_paste => {
                if let Some(new_cursor) = apply_completion(&mut line, cursor, models, selected) {
                    cursor = new_cursor;
                    selected = 0;
                    redraw_editor(prompt, &line, cursor, &[], selected, &mut displayed)?;
                } else {
                    print!("\x07");
                    io::stdout().flush()?;
                }
            }
            27 => {
                let action = classify_escape_sequence(&read_escape_sequence(&mut stdin)?);
                match action {
                    EscapeAction::PasteStart if !bracketed_paste => bracketed_paste = true,
                    EscapeAction::PasteEnd if bracketed_paste => bracketed_paste = false,
                    EscapeAction::Up | EscapeAction::Down if !bracketed_paste => {
                        let candidates = completion_candidates(&line, cursor, models).1;
                        if !candidates.is_empty() {
                            selected = match action {
                                EscapeAction::Up => {
                                    if selected == 0 {
                                        candidates.len() - 1
                                    } else {
                                        selected - 1
                                    }
                                }
                                EscapeAction::Down => (selected + 1) % candidates.len(),
                                _ => unreachable!(),
                            };
                            redraw_editor(
                                prompt,
                                &line,
                                cursor,
                                &candidates,
                                selected,
                                &mut displayed,
                            )?;
                        }
                    }
                    EscapeAction::Left
                    | EscapeAction::Right
                    | EscapeAction::Home
                    | EscapeAction::End
                        if !bracketed_paste =>
                    {
                        cursor = move_cursor(&line, cursor, action);
                        let candidates = completion_candidates(&line, cursor, models).1;
                        redraw_editor(
                            prompt,
                            &line,
                            cursor,
                            &candidates,
                            selected,
                            &mut displayed,
                        )?;
                    }
                    EscapeAction::Dismiss if !bracketed_paste => {
                        redraw_editor(prompt, &line, cursor, &[], selected, &mut displayed)?;
                    }
                    _ => {}
                }
            }
            first => {
                let Some(character) = read_utf8_character(&mut stdin, first)? else {
                    continue;
                };
                if is_safe_input_character(character) {
                    insert_at_cursor(&mut line, &mut cursor, character);
                    selected = 0;
                    let candidates = completion_candidates(&line, cursor, models).1;
                    redraw_editor(prompt, &line, cursor, &candidates, selected, &mut displayed)?;
                }
            }
        }
    }
}

async fn read_interactive_line(
    models: Vec<String>,
    prompt: String,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<(ReadOutcome, String)> {
    let worker_cancelled = cancelled.clone();
    tokio::task::spawn_blocking(move || {
        read_interactive_line_blocking(&models, &prompt, worker_cancelled.as_ref())
    })
    .await
    .map_err(io::Error::other)?
}

async fn animate_while<F>(future: F) -> io::Result<F::Output>
where
    F: Future,
{
    let activity = ActivityIndicator::start()?;
    let output = future.await;
    drop(activity);
    Ok(output)
}

async fn read_line_or_interrupt<R, F>(
    reader: &mut R,
    line: &mut String,
    interrupt: F,
) -> io::Result<ReadOutcome>
where
    R: AsyncBufRead + Unpin,
    F: Future<Output = io::Result<()>>,
{
    tokio::pin!(interrupt);
    tokio::select! {
        read = reader.read_line(line) => match read? {
            0 => Ok(ReadOutcome::Eof),
            _ => Ok(ReadOutcome::Line),
        },
        signal = &mut interrupt => {
            signal?;
            Ok(ReadOutcome::Interrupted)
        }
    }
}

fn install_termination_listener() -> io::Result<(
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    tokio::task::JoinHandle<()>,
)> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener_cancelled = cancelled.clone();
    let listener = tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
        listener_cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Ok((cancelled, listener))
}

async fn ctrl_c_or_termination(
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<()> {
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = async {
            while !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => Ok(()),
    }
}

pub async fn run(state: &AppState) -> anyhow::Result<()> {
    let interactive =
        should_use_interactive_editor(io::stdin().is_terminal(), io::stdout().is_terminal());
    let backend = state.scheduler.backend_name();
    let model_root = terminal_model_root();
    let (termination, termination_listener) = install_termination_listener()?;
    let mut selected_model: Option<String> = None;
    print!(
        "{}",
        render_startup(backend, selected_model.as_deref(), interactive)
    );

    let mut task_id = None;
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        if termination.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        // Availability can change while the REPL is running (for example a GGUF
        // is added or removed), so completion and commands use a fresh view.
        let models = local_model_labels(state.resource_registry.as_ref(), backend, &model_root);
        print!(
            "{}",
            render_status(backend, selected_model.as_deref(), task_id, interactive)
        );
        io::stdout().flush()?;

        let (read_outcome, line) = if interactive {
            let prompt = format!("{} ", styled(true, KAIC_RED, "❯"));
            read_interactive_line(models.clone(), prompt, termination.clone()).await?
        } else {
            let mut line = String::new();
            let outcome = read_line_or_interrupt(
                &mut stdin,
                &mut line,
                ctrl_c_or_termination(termination.clone()),
            )
            .await?;
            (outcome, line)
        };
        match read_outcome {
            ReadOutcome::Line => {}
            ReadOutcome::Eof | ReadOutcome::Interrupted | ReadOutcome::Terminated => {
                println!();
                break;
            }
        }
        let models = local_model_labels(state.resource_registry.as_ref(), backend, &model_root);

        let input = match parse_input(&line) {
            TerminalInput::Empty => continue,
            TerminalInput::Exit => break,
            TerminalInput::Help => {
                println!("{}", render_help(interactive));
                continue;
            }
            TerminalInput::Status => {
                let session = task_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "new".to_string());
                println!(
                    "\n{}  {}\n{}  {}\n{}  {}\n",
                    styled(interactive, KAIC_RED, "Backend"),
                    sanitize_terminal_line(backend),
                    styled(interactive, KAIC_RED, "Model"),
                    sanitize_terminal_line(selected_model_name(selected_model.as_deref())),
                    styled(interactive, KAIC_RED, "Session"),
                    session
                );
                continue;
            }
            TerminalInput::Models => {
                if interactive {
                    match read_interactive_model_picker(
                        models.clone(),
                        selected_model.clone(),
                        termination.clone(),
                    )
                    .await?
                    {
                        PickerOutcome::Selected(model) => {
                            let latest_models = local_model_labels(
                                state.resource_registry.as_ref(),
                                backend,
                                &model_root,
                            );
                            selected_model = match model {
                                Some(requested) => {
                                    let Some(available) = latest_models
                                        .iter()
                                        .find(|candidate| {
                                            candidate.eq_ignore_ascii_case(&requested)
                                        })
                                        .cloned()
                                    else {
                                        println!(
                                            "\nKAIC> Модель '{}' больше недоступна.{}",
                                            sanitize_terminal_line(&requested),
                                            render_models(
                                                &latest_models,
                                                selected_model.as_deref(),
                                                true
                                            )
                                        );
                                        continue;
                                    };
                                    Some(available)
                                }
                                None => None,
                            };
                            task_id = None;
                            println!(
                                "\nKAIC> Model: {}; начата новая сессия.\n",
                                sanitize_terminal_line(selected_model_name(
                                    selected_model.as_deref()
                                ))
                            );
                        }
                        PickerOutcome::Cancelled => println!("\nKAIC> Выбор модели отменён.\n"),
                        PickerOutcome::Interrupted => {
                            println!("\n^C\n");
                        }
                        PickerOutcome::Terminated => break,
                    }
                } else {
                    println!(
                        "{}",
                        render_models(&models, selected_model.as_deref(), false)
                    );
                }
                continue;
            }
            TerminalInput::RemovedModelCommand => {
                println!("\nKAIC> Команда /model удалена. Используйте /models для списка и выбора модели.\n");
                continue;
            }
            TerminalInput::Clear => {
                if interactive {
                    print!("\x1b[2J\x1b[H");
                    print!(
                        "{}",
                        render_startup(backend, selected_model.as_deref(), interactive)
                    );
                    io::stdout().flush()?;
                }
                continue;
            }
            TerminalInput::User(input) => input,
        };

        let turn = run_terminal_turn_interruptible_with_model(
            state,
            task_id,
            input,
            selected_model.as_deref(),
            false,
            ctrl_c_or_termination(termination.clone()),
        );
        let turn_result = if interactive {
            animate_while(turn).await?
        } else {
            turn.await
        };

        match turn_result {
            Ok(TerminalTurnOutcome::Completed(turn)) => {
                task_id = Some(turn.task_id);
                match turn.status {
                    TaskStatus::Done => match turn.reply {
                        Some(reply) => println!(
                            "\n{} {}\n",
                            styled(interactive, KAIC_RED, "◆ KAIC"),
                            sanitize_terminal_text(&reply)
                        ),
                        None => println!(
                            "\n{} Ответ модели отсутствует.\n",
                            styled(interactive, "1;33", "◇ KAIC")
                        ),
                    },
                    TaskStatus::WaitingForHuman => println!(
                        "\nKAIC> {}\n\
                         KAIC> Этот terminal-диалог заблокирован до решения pending-вызова. \
                         Terminal mode не запускает Control Center API; завершите REPL и \
                         отдельно запустите server mode с Control Center-клиентом.\n",
                        sanitize_terminal_text(&turn.detail.unwrap_or_else(|| format!(
                            "Инструмент ожидает явного разрешения; задача сохранена как {}.",
                            turn.task_id
                        )))
                    ),
                    TaskStatus::Failed => println!(
                        "\nKAIC> {}\n",
                        sanitize_terminal_text(&turn.detail.unwrap_or_else(|| {
                            "Задача завершилась без описания ошибки.".to_string()
                        }))
                    ),
                    status => println!("\nKAIC> Состояние задачи: {status:?}.\n"),
                }
            }
            Ok(TerminalTurnOutcome::Interrupted {
                task_id: interrupted_task_id,
            }) => {
                println!("\nKAIC> Задача {interrupted_task_id} отменена; начата новая сессия.\n");
                task_id = task_id_after_interruption(task_id);
                continue;
            }
            Err(err) => {
                eprintln!(
                    "\nKAIC error: {}\n",
                    sanitize_terminal_text(&format!("{err:#}"))
                );
            }
        }
    }

    termination_listener.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_suggests_commands_and_registry_models() {
        let models = vec!["Fable9B".to_string(), "Gemma27".to_string()];

        assert_eq!(
            completion_candidates("/sta", 4, &models),
            (0, vec!["/status".to_string()])
        );
        assert_eq!(
            completion_candidates("/mod", 4, &models),
            (0, vec!["/models".to_string()])
        );
        assert_eq!(
            completion_candidates("обычный текст", "обычный текст".len(), &models),
            (0, vec![])
        );
    }

    #[test]
    fn completion_replaces_only_the_token_before_the_cursor() {
        let models = vec!["Fable9B".to_string()];
        let mut line = "/mo suffix".to_string();
        let cursor = "/mo".len();

        let new_cursor = apply_completion(&mut line, cursor, &models, 0).unwrap();

        assert_eq!(line, "/models suffix");
        assert_eq!(new_cursor, "/models".len());
    }

    #[test]
    fn cursor_editing_supports_left_right_home_end_and_backspace() {
        let mut line = "a🙂c".to_string();
        let mut cursor = line.len();

        cursor = move_cursor(&line, cursor, EscapeAction::Left);
        insert_at_cursor(&mut line, &mut cursor, 'b');
        assert_eq!(line, "a🙂bc");
        assert!(backspace_at_cursor(&mut line, &mut cursor));
        assert_eq!(line, "a🙂c");
        cursor = move_cursor(&line, cursor, EscapeAction::Home);
        cursor = move_cursor(&line, cursor, EscapeAction::Right);
        assert_eq!(cursor, 1);
        cursor = move_cursor(&line, cursor, EscapeAction::End);
        assert_eq!(cursor, line.len());
    }

    #[test]
    fn redraw_layout_counts_wrapped_editor_and_popup_rows() {
        let prompt = "\x1b[31m❯\x1b[0m ";
        let candidates = vec!["abcdefghij".to_string()];

        let layout = editor_layout(prompt, "123456789", 5, &candidates, 8);

        assert_eq!(layout.editor_rows, 2);
        assert_eq!(layout.popup_rows, 2);
        assert_eq!(layout.total_rows, 4);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_column, 7);
    }

    #[test]
    fn redraw_layout_uses_terminal_cell_width_for_wide_and_combining_text() {
        let layout = editor_layout("❯ ", "e\u{301}🙂界", "e\u{301}🙂".len(), &[], 6);

        assert_eq!(visible_width("e\u{301}🙂界"), 5);
        assert_eq!(layout.editor_rows, 2);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_column, 5);
    }

    #[test]
    fn model_picker_contains_auto_and_tracks_navigation() {
        let models = vec!["Fable9B".to_string(), "Gemma27".to_string()];
        let choices = model_picker_choices(&models);

        assert_eq!(choices, vec![None, Some("Fable9B"), Some("Gemma27")]);
        assert_eq!(move_picker_selection(0, EscapeAction::Up, choices.len()), 2);
        assert_eq!(
            move_picker_selection(2, EscapeAction::Down, choices.len()),
            0
        );
        assert_eq!(
            move_picker_selection(1, EscapeAction::Down, choices.len()),
            2
        );
    }

    #[test]
    fn model_picker_physical_rows_include_wrapped_labels() {
        let rendered = "Выберите модель\n  ● auto (Scheduler)\n  · VeryLongModel";

        assert_eq!(model_picker_physical_rows(rendered, 10), 6);
    }

    #[test]
    fn model_picker_marks_current_choice_without_terminal_controls_in_labels() {
        let models = vec!["Fable\nforged".to_string(), "Gemma27".to_string()];

        let rendered = render_model_picker(&models, 2, false);

        assert!(rendered.contains("· auto (Scheduler)"));
        assert!(rendered.contains("· Fableforged"));
        assert!(rendered.contains("● Gemma27"));
        assert!(!rendered.contains("Fable\nforged"));
    }

    #[test]
    fn completion_popup_marks_the_selected_candidate() {
        let candidates = vec!["Fable9B".to_string(), "GPTOSS20".to_string()];

        let popup = render_completion_popup(&candidates, 1, false);

        assert!(popup[0].contains("  Fable9B"));
        assert!(popup[1].contains("› GPTOSS20"));
        assert!(!popup.join("\n").contains("\u{1b}["));
    }

    #[test]
    fn navigation_keys_are_distinguished_from_paste_sequences() {
        assert_eq!(classify_escape_sequence(b"[A"), EscapeAction::Up);
        assert_eq!(classify_escape_sequence(b"[B"), EscapeAction::Down);
        assert_eq!(classify_escape_sequence(b"[D"), EscapeAction::Left);
        assert_eq!(classify_escape_sequence(b"[C"), EscapeAction::Right);
        assert_eq!(classify_escape_sequence(b"[H"), EscapeAction::Home);
        assert_eq!(classify_escape_sequence(b"[F"), EscapeAction::End);
        assert_eq!(classify_escape_sequence(b"[200~"), EscapeAction::PasteStart);
    }

    #[test]
    fn embedded_model_list_only_contains_safe_gguf_files_under_model_root() {
        let root =
            std::env::temp_dir().join(format!("kaic-terminal-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let present = root.join("present.gguf");
        std::fs::write(&present, b"fixture").unwrap();
        let uppercase = root.join("uppercase.GGUF");
        std::fs::write(&uppercase, b"fixture").unwrap();
        let non_gguf = root.join("notes.txt");
        std::fs::write(&non_gguf, b"fixture").unwrap();
        let outside = root.with_extension("outside.gguf");
        std::fs::write(&outside, b"fixture").unwrap();
        let symlink = root.join("link.gguf");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&present, &symlink).unwrap();
        let registry_path = root.join("models.yaml");
        std::fs::write(
            &registry_path,
            format!(
                "Present:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nUppercase:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nNonGguf:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nOutside:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nSymlink:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nMissing:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nLegacy:\n  vram_mb: 1\n  load_seconds: 1\n",
                present.display(),
                uppercase.display(),
                non_gguf.display(),
                outside.display(),
                symlink.display(),
                root.join("missing.gguf").display()
            ),
        )
        .unwrap();
        let registry =
            crate::resource_registry::ResourceRegistry::from_file(&registry_path).unwrap();

        assert_eq!(
            local_model_labels(&registry, "embedded", &root),
            vec!["Present".to_string(), "Uppercase".to_string()]
        );
        std::fs::remove_file(&uppercase).unwrap();
        assert_eq!(
            local_model_labels(&registry, "embedded", &root),
            vec!["Present".to_string()]
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn external_model_list_requires_a_nonempty_provider_key() {
        let root =
            std::env::temp_dir().join(format!("kaic-terminal-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let model = root.join("model.gguf");
        std::fs::write(&model, b"fixture").unwrap();
        let registry_path = root.join("models.yaml");
        std::fs::write(
            &registry_path,
            format!(
                "PathOnly:\n  path: {}\n  vram_mb: 1\n  load_seconds: 1\nEmptyKey:\n  lm_studio_key: '  '\n  vram_mb: 1\n  load_seconds: 1\nProvider:\n  lm_studio_key: provider/model\n  vram_mb: 1\n  load_seconds: 1\n",
                model.display()
            ),
        )
        .unwrap();
        let registry =
            crate::resource_registry::ResourceRegistry::from_file(&registry_path).unwrap();

        assert_eq!(
            local_model_labels(&registry, "lmstudio", &root),
            vec!["Provider".to_string()]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activity_animation_is_smooth_fixed_width_and_non_destructive() {
        let frames: Vec<_> = (0..ACTIVITY_FRAMES.len())
            .map(|index| render_activity_frame(index, false))
            .collect();

        assert!(ACTIVITY_INTERVAL <= std::time::Duration::from_millis(100));
        assert!(frames.iter().all(|frame| frame.chars().count() == 12));
        assert!(frames.iter().all(|frame| !frame.contains("\x1b[2K")));
        assert_eq!(
            render_activity_frame(ACTIVITY_FRAMES.len(), false),
            frames[0]
        );
    }

    #[test]
    fn terminal_input_distinguishes_exit_empty_and_user_text() {
        assert_eq!(parse_input("  "), TerminalInput::Empty);
        assert_eq!(parse_input("exit"), TerminalInput::Exit);
        assert_eq!(parse_input("QUIT"), TerminalInput::Exit);
        assert_eq!(
            parse_input("  покажи статус  "),
            TerminalInput::User("покажи статус")
        );
    }

    #[test]
    fn model_selection_uses_only_models_command() {
        assert_eq!(parse_input("/models"), TerminalInput::Models);
        assert_eq!(parse_input("/model"), TerminalInput::RemovedModelCommand);
        assert_eq!(
            parse_input("/model Fable9B"),
            TerminalInput::RemovedModelCommand
        );
    }

    #[test]
    fn interactive_theme_is_red_orange_not_cyan() {
        let panel = render_startup("lmstudio", None, true);
        let status = render_status("lmstudio", None, None, true);

        assert!(panel.contains("\x1b[1;38;5;196m"));
        assert!(status.contains("\x1b[38;5;202m"));
        assert!(!panel.contains("\x1b[1;36m"));
        assert!(!status.contains("\x1b[36m"));
    }

    #[test]
    fn slash_commands_are_handled_by_the_repl() {
        assert_eq!(parse_input("/help"), TerminalInput::Help);
        assert_eq!(parse_input("/status"), TerminalInput::Status);
        assert_eq!(parse_input("/clear"), TerminalInput::Clear);
        assert_eq!(parse_input("/exit"), TerminalInput::Exit);
    }

    #[test]
    fn startup_panel_shows_identity_backend_and_commands() {
        let panel = render_startup("lmstudio", None, false);

        assert!(panel.contains(BANNER));
        assert!(panel.contains("KAIC 0.1.0"));
        assert!(panel.contains("Backend  lmstudio"));
        assert!(panel.contains("Session  new"));
        assert!(panel.contains("/help"));
        assert!(panel.contains("Tab"));
        assert!(!panel.contains("\u{1b}["));
    }

    #[test]
    fn startup_frame_has_equal_border_widths() {
        let panel = render_startup("lmstudio", None, false);
        let widths: Vec<_> = panel
            .lines()
            .filter(|line| line.starts_with('╭') || line.starts_with('╰'))
            .map(|line| line.chars().count())
            .collect();

        assert_eq!(widths.len(), 2);
        assert_eq!(widths[0], widths[1]);
    }

    #[test]
    fn untrusted_terminal_text_removes_ansi_osc_and_c1_controls() {
        let hostile = "ok\x1b[31mred\x1b[0m\x1b]52;c;Y2xpcA==\x07!\u{009b}2J\rforge\nnext\tcell";
        let safe = sanitize_terminal_text(hostile);

        assert_eq!(safe, "ok[31mred[0m]52;c;Y2xpcA==!2Jforge\nnext\tcell");
        assert!(!safe.chars().any(|character| {
            character == '\u{001b}'
                || character == '\u{007f}'
                || ('\u{0080}'..='\u{009f}').contains(&character)
                || (character.is_control() && character != '\n' && character != '\t')
        }));
    }

    #[test]
    fn activity_status_uses_its_own_line_without_cursor_erasure() {
        let activity = render_activity(false);

        assert!(activity.ends_with('\n'));
        assert!(!activity.contains('\r'));
        assert!(!activity.contains("\x1b[2K"));
    }

    #[test]
    fn interactive_editor_requires_both_terminal_streams() {
        assert!(should_use_interactive_editor(true, true));
        assert!(!should_use_interactive_editor(false, true));
        assert!(!should_use_interactive_editor(true, false));
    }

    #[test]
    fn input_and_output_sanitizers_reject_terminal_spoofing_controls() {
        for character in [
            '\0', '\u{0085}', '\u{061c}', '\u{200e}', '\u{202e}', '\u{2066}', '\u{2069}',
            '\u{feff}',
        ] {
            assert!(!is_safe_input_character(character));
        }
        assert!(is_safe_input_character('Ж'));
        assert_eq!(
            sanitize_terminal_text("safe\u{202e}spoof\u{2066}text\u{2069}"),
            "safespooftext"
        );
        assert_eq!(
            sanitize_terminal_line("model\nforged\trow"),
            "modelforgedrow"
        );
    }

    #[test]
    fn bracketed_paste_cleanup_is_armed_before_enable_write_can_fail() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("expected failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut enabled = false;
        let error = enable_bracketed_paste(&mut FailingWriter, &mut enabled).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(enabled);
    }

    #[test]
    fn failed_cleanup_output_cannot_skip_terminal_restoration() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("expected failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("expected failure"))
            }
        }

        let mut restored = false;
        cleanup_raw_terminal(&mut FailingWriter, true, || restored = true);
        assert!(restored);
    }

    #[test]
    fn external_termination_stops_the_raw_read_loop_for_raii_cleanup() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        let mut byte = [0_u8; 1];

        assert_eq!(
            read_byte_or_termination(&mut reader, &mut byte, &cancelled).unwrap(),
            RawRead::Terminated
        );
    }

    #[test]
    fn escape_parser_distinguishes_bracketed_paste_and_navigation() {
        assert_eq!(classify_escape_sequence(b"[200~"), EscapeAction::PasteStart);
        assert_eq!(classify_escape_sequence(b"[201~"), EscapeAction::PasteEnd);
        assert_eq!(classify_escape_sequence(b"[A"), EscapeAction::Up);
        assert_eq!(classify_escape_sequence(b"[3~"), EscapeAction::Ignore);
        assert_eq!(classify_escape_sequence(b""), EscapeAction::Dismiss);

        let mut ss3 = std::io::Cursor::new(b"OPx".to_vec());
        assert_eq!(read_escape_sequence(&mut ss3).unwrap(), b"OP");
        let mut next = [0_u8; 1];
        ss3.read_exact(&mut next).unwrap();
        assert_eq!(next[0], b'x');
    }

    #[test]
    fn backspace_updates_unicode_text_before_a_full_redraw() {
        let mut input = "e\u{301}🙂".to_string();
        let mut cursor = input.len();
        assert!(backspace_at_cursor(&mut input, &mut cursor));
        assert_eq!(input, "e\u{301}");
        assert!(backspace_at_cursor(&mut input, &mut cursor));
        assert_eq!(input, "e");
    }

    #[test]
    fn interrupted_turn_starts_a_new_session_and_keeps_repl_running() {
        let old_task = Some(uuid::Uuid::new_v4());

        assert_eq!(task_id_after_interruption(old_task), None);
    }

    #[tokio::test]
    async fn ctrl_c_wins_over_a_pending_terminal_read() {
        let (_writer, reader) = tokio::io::duplex(8);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();

        let outcome = read_line_or_interrupt(&mut reader, &mut line, std::future::ready(Ok(())))
            .await
            .unwrap();

        assert_eq!(outcome, ReadOutcome::Interrupted);
    }
}
