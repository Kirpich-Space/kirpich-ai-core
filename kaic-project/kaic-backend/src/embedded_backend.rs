//! EmbeddedBackend — ЗАПАРКОВАННЫЙ каркас будущей реализации `ModelBackend`
//! поверх `llama-cpp-4`.
//!
//! Рабочая логика сюда пока сознательно не переносится — она живёт
//! отдельно, в `experiments/llama_cpp_smoke/` (полностью автономный
//! Cargo-проект, не зависящий от KAIC). Пока технология не доказана там
//! своими руками (CPU и CUDA сборка, реальный GGUF, реальный
//! apply_chat_template на конкретных моделях), здесь намеренно нет
//! рабочего кода — чтобы не поддерживать одновременно два источника
//! правды, которые будут расходиться при каждой правке одного из них.
//!
//! Когда `experiments/llama_cpp_smoke` подтвердит себя — сюда переносится
//! уже проверенный, а не гипотетический код, тонкой адаптацией под
//! сигнатуры `ModelBackend` ниже.
//!
//! Компилируется только при `--features embedded-backend` (см. Cargo.toml).

use anyhow::Result;
use async_trait::async_trait;

use crate::model_backend::{GenerateRequest, GenerateResponse, ModelBackend};

/// Целевая форма backend'а — поля появятся вместе с реальной реализацией,
/// перенесённой из experiments/llama_cpp_smoke.
pub struct EmbeddedBackend;

#[async_trait]
impl ModelBackend for EmbeddedBackend {
    async fn is_loaded(&self, _model: &str) -> Result<bool> {
        todo!("перенести из experiments/llama_cpp_smoke после проверки технологии")
    }

    async fn load(&self, _model: &str) -> Result<()> {
        todo!("перенести из experiments/llama_cpp_smoke после проверки технологии")
    }

    async fn unload(&self, _model: &str) -> Result<()> {
        todo!("перенести из experiments/llama_cpp_smoke после проверки технологии")
    }

    async fn generate(&self, _model: &str, _request: GenerateRequest) -> Result<GenerateResponse> {
        todo!("перенести из experiments/llama_cpp_smoke после проверки технологии")
    }
}
