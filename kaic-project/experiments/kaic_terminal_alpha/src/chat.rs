//! История диалога. Простая структура, без абстракций.

use anyhow::Result;
use llama_cpp_4::prelude::*;

pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

pub struct ChatHistory {
    pub messages: Vec<ChatMessage>,
}

impl ChatHistory {
    pub fn new() -> Self {
        ChatHistory {
            messages: Vec::new(),
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            role: "user".to_string(),
            content: text.to_string(),
        });
    }

    pub fn push_assistant(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: text.to_string(),
        });
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Конвертирует в формат, который понимает apply_chat_template().
    pub fn to_llama_messages(&self) -> Result<Vec<LlamaChatMessage>> {
        let mut out = Vec::with_capacity(self.messages.len());
        for m in &self.messages {
            out.push(LlamaChatMessage::new(m.role.clone(), m.content.clone())?);
        }
        Ok(out)
    }
}
