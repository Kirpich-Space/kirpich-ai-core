use std::io::{self, Write};

use anyhow::Result;

use crate::core::context::ProjectInfo;
use crate::core::engine::{ChatMessage, ChatParams, KaicEngine};
use crate::core::config::Config;
use crate::{git_diff, git_log, git_show, git_status};
use crate::tools;

const SYSTEM_PROMPT: &str = "Ты — KAIC, локальный AI-ассистент, работающий в терминале внутри \
конкретного проекта на диске пользователя. Ты можешь объяснять код, искать ошибки, отвечать по \
структуре проекта и предлагать изменения в виде текста. Ты НЕ редактируешь файлы и НЕ выполняешь \
команды напрямую — только предлагаешь их пользователю.";

/// Состояние сессии живёт здесь, а не в KaicEngine — см. пояснение в
/// core/engine.rs про минимальный публичный интерфейс движка.
struct Session {
    history: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: usize,
    context: u32,
    model_path: String,
}

impl Session {
    fn new(config: &Config) -> Self {
        Self {
            history: vec![ChatMessage::new("system", SYSTEM_PROMPT)],
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            context: config.context,
            // Сессия REPL показывает и использует свою, диалоговую модель:
            // модель Planner'а к терминалу отношения не имеет.
            model_path: config.models.repl.clone(),
        }
    }

    fn clear(&mut self) {
        self.history.truncate(1); // системный промпт остаётся
    }

    fn history_text(&self) -> String {
        if self.history.len() <= 1 {
            return "(история пуста)".to_string();
        }
        self.history[1..]
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn params(&self) -> ChatParams {
        ChatParams {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            context: self.context,
            // Свободный чат в REPL ничем не ограничивается: грамматика — это
            // инструмент агентного слоя, а не режим работы терминала.
            grammar: None,
        }
    }
}

pub fn run(engine: &KaicEngine, config: &Config, project: &ProjectInfo) -> Result<()> {
    let mut session = Session::new(config);

    println!("KAIC Terminal v0.2 — /help для списка команд, /exit для выхода.\n");
    println!("{}\n", project.summary());

    loop {
        print!("> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        let bytes_read = io::stdin().read_line(&mut line)?;
        if bytes_read == 0 {
            break; // EOF
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(rest) = input.strip_prefix("/read ") {
            handle_read(&mut session, project, rest.trim());
            continue;
        }
        if input == "/ls" || input.starts_with("/ls ") {
            let rel = input.strip_prefix("/ls").unwrap().trim();
            handle_ls(project, rel);
            continue;
        }
        if input == "/write" || input.starts_with("/write ") {
            let rel = input.strip_prefix("/write").unwrap().trim();
            handle_write(project, rel);
            continue;
        }
        if input == "/create-file" || input.starts_with("/create-file ") {
            let rel = input.strip_prefix("/create-file").unwrap().trim();
            handle_create_file(project, rel);
            continue;
        }
        if input == "/create-directory" || input.starts_with("/create-directory ") {
            let rel = input.strip_prefix("/create-directory").unwrap().trim();
            handle_create_directory(project, rel);
            continue;
        }
        if let Some(rest) = input.strip_prefix("/temp ") {
            handle_temp(&mut session, rest.trim());
            continue;
        }
        if input == "/git-show" || input.starts_with("/git-show ") {
            let revision = input.strip_prefix("/git-show").unwrap().trim();
            handle_git_show(project, revision);
            continue;
        }

        match input {
            "/exit" => break,
            "/help" => print_help(),
            "/clear" => {
                session.clear();
                println!("История очищена (системный промпт сохранён).\n");
            }
            "/history" => println!("{}\n", session.history_text()),
            "/model" => println!("Модель: {}\n", session.model_path),
            "/temp" => println!("Текущая temperature: {}\n", session.temperature),
            "/git-status" => handle_git_status(project),
            "/git-diff" => handle_git_diff(project),
            "/git-log" => handle_git_log(project),
            "project" => println!("{}\n", project.summary()),
            _ => {
                session.history.push(ChatMessage::new("user", input));
                let params = session.params();
                match engine.chat(&session.history, &params) {
                    Ok(reply) => {
                        session.history.push(ChatMessage::new("assistant", reply.clone()));
                        println!("{reply}\n");
                    }
                    Err(e) => {
                        // Реплика не удалась — не оставляем "висящее" сообщение
                        // пользователя без ответа в истории.
                        session.history.pop();
                        eprintln!("[ошибка] {e:#}\n");
                    }
                }
            }
        }
    }

    println!("Пока.");
    Ok(())
}

fn handle_read(session: &mut Session, project: &ProjectInfo, rel_path: &str) {
    if rel_path.is_empty() {
        eprintln!("[ошибка] использование: /read <относительный путь к файлу>\n");
        return;
    }
    match tools::read_file(&project.root, rel_path) {
        Ok(content) => {
            println!("--- {rel_path} ---\n{content}\n---\n");
            let text = format!("[контекст: {rel_path}]\n{content}");
            session.history.push(ChatMessage::new("user", text));
            println!("(содержимое добавлено в контекст сессии — модель увидит его в следующем ответе)\n");
        }
        Err(e) => eprintln!("[ошибка] {e:#}\n"),
    }
}

fn handle_ls(project: &ProjectInfo, rel_path: &str) {
    let rel_path = if rel_path.is_empty() { "." } else { rel_path };
    match tools::list_directory(&project.root, rel_path) {
        Ok(listing) => println!("{listing}\n"),
        Err(e) => eprintln!("[ошибка] {e:#}\n"),
    }
}

fn handle_write(project: &ProjectInfo, rel_path: &str) {
    if rel_path.is_empty() {
        eprintln!("[error] usage: /write <relative file path>\n");
        return;
    }

    println!(
        "Введите новое содержимое файла. Отдельная строка /end завершает ввод, \
         /cancel отменяет операцию."
    );
    let mut content = String::new();

    loop {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                println!("\nЗапись отменена: получен EOF.\n");
                return;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("[error] failed to read file content: {error}\n");
                return;
            }
        }

        let marker = line.trim_end_matches(['\r', '\n']);
        match marker {
            "/end" => break,
            "/cancel" => {
                println!("Запись отменена.\n");
                return;
            }
            _ => content.push_str(&line),
        }
    }

    let prepared = match tools::prepare_write_file(&project.root, rel_path, &content) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("[error] {error:#}\n");
            return;
        }
    };

    println!(
        "\n--- write_file diff ---\n{}--- end diff ---\n",
        prepared.diff()
    );
    if !prepared.has_changes() {
        println!("Изменений нет; запись не выполнялась.\n");
        return;
    }

    print!(
        "Применить показанный diff и записать {} байт в \"{rel_path}\"? [y/N] ",
        prepared.bytes_to_write()
    );
    if let Err(error) = io::stdout().flush() {
        eprintln!("[error] failed to flush stdout: {error}\n");
        return;
    }

    let mut confirmation = String::new();
    match io::stdin().read_line(&mut confirmation) {
        Ok(0) => {
            println!("\nЗапись отменена: получен EOF.\n");
            return;
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[error] failed to read confirmation: {error}\n");
            return;
        }
    }

    let confirmation = confirmation.trim().to_ascii_lowercase();
    if confirmation != "y" && confirmation != "yes" {
        println!("Запись отменена.\n");
        return;
    }

    match tools::write_file(prepared) {
        Ok(outcome) if !outcome.changed => {
            println!("Изменений нет; запись не выполнялась.\n");
        }
        Ok(outcome) => {
            println!("write_file completed:");
            let shown_path = outcome
                .path
                .strip_prefix(&project.root)
                .unwrap_or(&outcome.path);
            println!("  path: {}", shown_path.display());
            println!("  bytes written: {}", outcome.bytes_written);
            println!(
                "  verification: {}",
                if outcome.verified { "passed" } else { "failed" }
            );
            if let Some(backup_path) = outcome.backup_path {
                let shown_path = backup_path
                    .strip_prefix(&project.root)
                    .unwrap_or(&backup_path);
                println!("  backup: {}", shown_path.display());
            }
            println!("  audit log: .kaic/write-audit.log\n");
        }
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_create_file(project: &ProjectInfo, rel_path: &str) {
    if rel_path.is_empty() {
        eprintln!("[error] usage: /create-file <relative file path>\n");
        return;
    }

    println!(
        "Введите содержимое нового файла. Отдельная строка /end завершает ввод, \
         /cancel отменяет операцию."
    );
    let mut content = String::new();

    loop {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                println!("\nСоздание файла отменено: получен EOF.\n");
                return;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("[error] failed to read file content: {error}\n");
                return;
            }
        }

        let marker = line.trim_end_matches(['\r', '\n']);
        match marker {
            "/end" => break,
            "/cancel" => {
                println!("Создание файла отменено.\n");
                return;
            }
            _ => content.push_str(&line),
        }
    }

    let prepared = match tools::prepare_create_file(&project.root, rel_path, &content) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("[error] {error:#}\n");
            return;
        }
    };

    println!(
        "\n--- create_file diff ---\n{}--- end diff ---\n",
        prepared.diff()
    );
    print!(
        "Применить показанный diff и создать \"{rel_path}\" ({} байт)? [y/N] ",
        prepared.bytes_to_write()
    );
    if let Err(error) = io::stdout().flush() {
        eprintln!("[error] failed to flush stdout: {error}\n");
        return;
    }

    let mut confirmation = String::new();
    match io::stdin().read_line(&mut confirmation) {
        Ok(0) => {
            println!("\nСоздание файла отменено: получен EOF.\n");
            return;
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[error] failed to read confirmation: {error}\n");
            return;
        }
    }
    let confirmation = confirmation.trim().to_ascii_lowercase();
    if confirmation != "y" && confirmation != "yes" {
        println!("Создание файла отменено.\n");
        return;
    }

    match tools::create_file(prepared) {
        Ok(outcome) => {
            let shown_path = outcome
                .path
                .strip_prefix(&project.root)
                .unwrap_or(&outcome.path);
            println!("create_file completed:");
            println!("  path: {}", shown_path.display());
            println!("  bytes written: {}", outcome.bytes_written);
            println!(
                "  verification: {}",
                if outcome.verified { "passed" } else { "failed" }
            );
            println!("  backup: not required");
            println!("  audit log: .kaic/write-audit.log\n");
        }
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_create_directory(project: &ProjectInfo, rel_path: &str) {
    if rel_path.is_empty() {
        eprintln!("[error] usage: /create-directory <relative directory path>\n");
        return;
    }

    let prepared = match tools::prepare_create_directory(&project.root, rel_path) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("[error] {error:#}\n");
            return;
        }
    };

    println!(
        "\n--- create_directory preview ---\n{}--- end preview ---\n",
        prepared.preview()
    );
    print!("Создать показанную директорию? [y/N] ");
    if let Err(error) = io::stdout().flush() {
        eprintln!("[error] failed to flush stdout: {error}\n");
        return;
    }

    let mut confirmation = String::new();
    match io::stdin().read_line(&mut confirmation) {
        Ok(0) => {
            println!("\nСоздание директории отменено: получен EOF.\n");
            return;
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[error] failed to read confirmation: {error}\n");
            return;
        }
    }
    let confirmation = confirmation.trim().to_ascii_lowercase();
    if confirmation != "y" && confirmation != "yes" {
        println!("Создание директории отменено.\n");
        return;
    }

    match tools::create_directory(prepared) {
        Ok(outcome) => {
            let shown_path = outcome
                .path
                .strip_prefix(&project.root)
                .unwrap_or(&outcome.path);
            println!("create_directory completed:");
            println!("  path: {}", shown_path.display());
            println!(
                "  verification: {}",
                if outcome.verified { "passed" } else { "failed" }
            );
            println!("  backup: not required");
            println!("  audit log: .kaic/write-audit.log\n");
        }
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_git_status(project: &ProjectInfo) {
    match git_status::git_status(&project.root) {
        Ok(output) => println!("{output}\n"),
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_git_diff(project: &ProjectInfo) {
    match git_diff::git_diff(&project.root) {
        Ok(output) => println!("{output}\n"),
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_git_log(project: &ProjectInfo) {
    match git_log::git_log(&project.root) {
        Ok(output) => println!("{output}\n"),
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_git_show(project: &ProjectInfo, revision: &str) {
    if revision.is_empty() {
        eprintln!("[error] usage: /git-show <revision>\n");
        return;
    }

    match git_show::git_show(&project.root, revision) {
        Ok(output) => println!("{output}\n"),
        Err(error) => eprintln!("[error] {error:#}\n"),
    }
}

fn handle_temp(session: &mut Session, value: &str) {
    match value.parse::<f32>() {
        Ok(t) if (0.0..=2.0).contains(&t) => {
            session.temperature = t;
            println!("temperature установлена: {t}\n");
        }
        Ok(t) => eprintln!("[ошибка] temperature {t} вне разумного диапазона 0.0..=2.0\n"),
        Err(_) => eprintln!("[ошибка] использование: /temp <число, например 0.7>\n"),
    }
}

fn print_help() {
    println!(
        "\nКоманды:\n\
         /help     — эта справка\n\
         /clear    — очистить историю сессии (системный промпт сохраняется)\n\
         /model    — показать путь к текущей модели\n\
         /temp N   — установить temperature (например: /temp 0.7)\n\
         /history  — показать историю текущей сессии\n\
         /read F   — прочитать файл проекта и добавить его в контекст сессии\n\
         /ls [D]   — показать содержимое директории проекта (по умолчанию — корень)\n\
         /write F  — безопасно записать файл (ввод до /end, отмена через /cancel)\n\
         /create-file F      — создать новый файл после preview и подтверждения\n\
         /create-directory D — создать одну директорию после подтверждения\n\
         /git-status       — показать branch и состояние working tree\n\
         /git-diff         — показать unstaged и staged изменения\n\
         /git-log          — показать последние 20 коммитов\n\
         /git-show REV     — показать выбранный коммит и его patch\n\
         project   — показать сведения о проекте\n\
         /exit     — выйти\n\
         Любой другой ввод отправляется модели как сообщение чата.\n"
    );
}
