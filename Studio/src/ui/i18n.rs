//! Локализация интерфейса KAIC Studio: EN (по умолчанию) и RU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    pub fn from_code(code: &str) -> Self {
        if code.eq_ignore_ascii_case("ru") {
            Self::Ru
        } else {
            Self::En
        }
    }

    pub fn t<'a>(self, en: &'a str, ru: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::Ru => ru,
        }
    }

    pub fn format_bytes(self, bytes: u64) -> String {
        if bytes >= 1024 * 1024 * 1024 {
            format!(
                "{:.2} {}",
                bytes as f64 / 1_073_741_824.0,
                self.t("GB", "ГБ")
            )
        } else if bytes >= 1024 * 1024 {
            format!("{:.1} {}", bytes as f64 / 1_048_576.0, self.t("MB", "МБ"))
        } else if bytes >= 1024 {
            format!("{:.1} {}", bytes as f64 / 1024.0, self.t("KB", "КБ"))
        } else {
            format!("{bytes} {}", self.t("B", "Б"))
        }
    }

    pub fn format_gb(self, bytes: u64) -> String {
        format!(
            "{:.2} {}",
            bytes as f64 / 1_073_741_824.0,
            self.t("GB", "ГБ")
        )
    }

    pub fn format_kb(self, bytes: usize) -> String {
        if bytes >= 1024 {
            format!("{:.1} {}", bytes as f64 / 1024.0, self.t("KB", "КБ"))
        } else {
            format!("{bytes} {}", self.t("B", "Б"))
        }
    }

    pub fn chat_title<'a>(self, raw: &'a str) -> &'a str {
        if raw.is_empty() || raw == "Новый разговор" || raw == "New chat" {
            self.t("New chat", "Новый разговор")
        } else {
            raw
        }
    }
}
