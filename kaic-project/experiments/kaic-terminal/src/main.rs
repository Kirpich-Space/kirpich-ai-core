mod agent;
mod contract;
mod core;
mod git_diff;
mod git_log;
mod git_show;
mod git_status;
#[cfg(test)]
mod git_test_support;
mod repl;
mod tools;

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::core::config::Config;
use crate::core::context;
use crate::core::engine::KaicEngine;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("kaic {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Первый позиционный аргумент — путь к проекту. "kaic ." и "kaic" без
    // аргументов оба означают текущую директорию — намеренное упрощение,
    // в исходной спецификации был явно описан только "kaic .".
    let project_path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| ".".to_string());

    let project_root: PathBuf = PathBuf::from(&project_path)
        .canonicalize()
        .with_context(|| format!("путь к проекту не найден: {project_path}"))?;

    println!("[kaic] проект: {}", project_root.display());

    let config = Config::load(&project_root)?;

    let project_info = context::detect(&project_root)?;

    // Роль REPL: диалоговая модель. Модель Planner'а (config.models.planner)
    // здесь намеренно не трогается — она поднимается агентным слоем отдельно.
    let engine = KaicEngine::new(&config.models.repl, config.n_gpu_layers)
        .context("не удалось инициализировать движок")?;

    repl::run(&engine, &config, &project_info)
}

fn print_help() {
    println!(
        "kaic — локальный AI-ассистент в терминале для работы с проектом.\n\n\
         Использование:\n  \
         kaic .                  открыть текущую директорию как проект\n  \
         kaic /путь/к/проекту    открыть проект по указанному пути\n  \
         kaic --help             показать эту справку\n  \
         kaic --version          показать версию\n\n\
         В корне открываемого проекта должен лежать файл kaic.toml\n\
         (см. kaic.toml.example)."
    );
}
