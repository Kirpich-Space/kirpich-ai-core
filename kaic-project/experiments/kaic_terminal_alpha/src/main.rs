//! KAIC Terminal Alpha — интерактивный консольный чат поверх llama-cpp-4.
//!
//! Запуск:
//!   cargo run                      (читает ./kaic.toml, создаёт с
//!                                    значениями по умолчанию, если нет)
//!   cargo run --features cuda      (GPU offload, см. n_gpu_layers в kaic.toml)

mod chat;
mod config;
mod engine;
mod logger;

use std::io::{self, Write};

use anyhow::Result;

use chat::ChatHistory;
use config::Config;

fn print_help() {
    println!("Команды:");
    println!("  /help   — это сообщение");
    println!("  /clear  — очистить историю диалога");
    println!("  /model  — показать путь к текущей модели");
    println!("  /temp N — установить temperature (например: /temp 0.9)");
    println!("  /exit   — выход");
    println!("Всё остальное — обычное сообщение модели.");
}

fn main() -> Result<()> {
    let cfg = Config::load("kaic.toml")?;
    logger::log(&cfg.log_path, "=== KAIC Terminal Alpha: старт ===");

    let backend = engine::init_backend()?;
    let model = match engine::load_model(&backend, &cfg) {
        Ok(m) => m,
        Err(e) => {
            logger::log_error(&cfg.log_path, &format!("не удалось загрузить модель: {e}"));
            return Err(e);
        }
    };

    let mut eng = match engine::Engine::new(&backend, &model, cfg.clone()) {
        Ok(e) => e,
        Err(e) => {
            logger::log_error(&cfg.log_path, &format!("не удалось создать контекст: {e}"));
            return Err(e);
        }
    };

    let mut history = ChatHistory::new();

    println!("KAIC Terminal Alpha. Модель: {}", eng.model_path());
    print_help();

    loop {
        print!("\n> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            logger::log_error(&cfg.log_path, "ошибка чтения ввода, выхожу");
            break;
        }
        let input = line.trim();

        if input.is_empty() {
            continue;
        }

        match input {
            "/exit" => {
                logger::log(&cfg.log_path, "выход по команде /exit");
                break;
            }
            "/help" => {
                print_help();
                continue;
            }
            "/clear" => {
                history.clear();
                println!("история очищена");
                continue;
            }
            "/model" => {
                println!("текущая модель: {}", eng.model_path());
                continue;
            }
            _ if input.starts_with("/temp") => {
                let arg = input.trim_start_matches("/temp").trim();
                match arg.parse::<f32>() {
                    Ok(t) => {
                        eng.set_temperature(t);
                        println!("temperature установлена: {t}");
                    }
                    Err(_) => {
                        println!(
                            "не понял значение, текущая temperature = {}",
                            eng.temperature()
                        );
                    }
                }
                continue;
            }
            _ => {}
        }

        history.push_user(input);
        logger::log(
            &cfg.log_path,
            &format!("сообщений в истории: {}", history.len()),
        );

        match eng.generate(&history) {
            Ok(reply) => {
                history.push_assistant(&reply);
            }
            Err(e) => {
                logger::log_error(&cfg.log_path, &format!("ошибка генерации: {e}"));
                println!("[ошибка генерации, см. лог: {}]", cfg.log_path);
            }
        }
    }

    logger::log(&cfg.log_path, "=== KAIC Terminal Alpha: остановлен ===");
    Ok(())
}
