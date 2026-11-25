//! Независимый терминальный клиент существующего KAIC HTTP API.
//!
//! CLI использует построчный ввод и polling. Исполнение инструментов и оркестрация
//! остаются в backend; полноэкранные UI-зависимости здесь не используются.

pub mod app;
pub mod cli;
pub mod client;
pub mod protocol;
pub mod text;
