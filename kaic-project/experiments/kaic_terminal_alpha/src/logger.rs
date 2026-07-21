//! Минимальный логгер.
//!
//! Сознательно без структуры/состояния — просто функция, которая на
//! каждый вызов дописывает строку в лог-файл (создавая его и папку logs/
//! при необходимости) и одновременно печатает в stdout с явным flush.
//! Для терминальной альфы этого достаточно; открытие файла на каждую
//! запись чуть менее эффективно, зато исключает вопросы владения/borrow
//! логгера между модулями.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(log_path: &str, msg: &str) {
    println!("{msg}");
    io::stdout().flush().ok();

    if let Some(parent) = Path::new(log_path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

pub fn log_error(log_path: &str, msg: &str) {
    log(log_path, &format!("[ERROR] {msg}"));
}
