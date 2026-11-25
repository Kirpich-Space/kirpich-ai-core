//! Настройки KAIC Studio (язык EN/RU, папка моделей, HF_TOKEN, параметры llama.cpp).
use super::{Studio, anim, palette};
use crate::{catalog, kaic::Secret, network::LocalEndpoint, service::Job};
use eframe::egui::{self, Color32, RichText, Stroke};

impl Studio {
    fn save_settings(&mut self) -> anyhow::Result<()> {
        let lang = self.lang();
        LocalEndpoint::parse(&self.state.settings.kaic_url)?;
        self.mark_dirty();
        self.persist()?;
        self.status = lang
            .t(
                "Settings saved; KAIC token stays in memory only",
                "Настройки сохранены; токен остаётся только в памяти",
            )
            .into();
        Ok(())
    }

    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let pal = palette::p();
        let lang = self.lang();
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.settings_open = false;
            return;
        }
        let token_present = self.hf.token_present();

        egui::ScrollArea::vertical()
            .id_salt("страница-настроек")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().min(760.0);
                let margin = ((ui.available_width() - width) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(margin);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        ui.add_space(12.0);

                        // ── Шапка страницы ──────────────────────────────────
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(
                                    RichText::new(lang.t("Settings", "Настройки"))
                                        .strong()
                                        .size(20.0)
                                        .color(pal.ink),
                                );
                                ui.label(
                                    RichText::new(lang.t(
                                        "Interface language, model folder, Hugging Face token, and inference defaults",
                                        "Язык интерфейса, папка моделей, токен Hugging Face и параметры инференса",
                                    ))
                                    .size(12.5)
                                    .color(pal.ink_muted),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let save_btn = egui::Button::new(
                                        RichText::new(lang.t("Save", "Сохранить"))
                                            .color(pal.on_brand)
                                            .strong(),
                                    )
                                    .fill(pal.brand)
                                    .stroke(Stroke::NONE)
                                    .corner_radius(7);
                                    if ui.add(save_btn).clicked()
                                        && let Err(e) = self.save_settings()
                                    {
                                        self.error = Some(format!("{e:#}"));
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(lang.t("Done", "Готово"))
                                                    .color(pal.ink),
                                            )
                                            .corner_radius(7),
                                        )
                                        .clicked()
                                    {
                                        self.settings_open = false;
                                    }
                                    anim::keycap(ui, "Esc");
                                },
                            );
                        });
                        ui.add_space(12.0);

                        // ── 0. Интерфейс (EN / RU) ──────────────────────────
                        settings_card(ui, lang.t("Interface", "Интерфейс"), |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(
                                        RichText::new(lang.t("Language", "Язык интерфейса"))
                                            .strong()
                                            .size(13.0)
                                            .color(pal.ink),
                                    );
                                    ui.label(
                                        RichText::new(lang.t(
                                            "Applies immediately across all screens",
                                            "Применяется сразу ко всем экранам",
                                        ))
                                        .size(12.0)
                                        .color(pal.ink_muted),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        for (code, label) in [
                                            ("ru", "Русский (RU)"),
                                            ("en", "English (EN)"),
                                        ] {
                                            let sel = self
                                                .state
                                                .settings
                                                .language
                                                .eq_ignore_ascii_case(code)
                                                || (code == "en"
                                                    && !self
                                                        .state
                                                        .settings
                                                        .language
                                                        .eq_ignore_ascii_case("ru"));
                                            let btn = egui::Button::new(
                                                RichText::new(label).size(12.5).color(if sel {
                                                    pal.ink
                                                } else {
                                                    pal.ink_muted
                                                }),
                                            )
                                            .fill(if sel {
                                                pal.surface_2
                                            } else {
                                                pal.surface_0
                                            })
                                            .stroke(Stroke::new(
                                                1.0,
                                                if sel { pal.brand } else { pal.line },
                                            ))
                                            .corner_radius(6);
                                            if ui.add(btn).clicked() && !sel {
                                                self.state.settings.language = code.into();
                                                self.mark_dirty();
                                            }
                                        }
                                    },
                                );
                            });
                        });

                        ui.add_space(12.0);

                        // ── 1. Модели и Hugging Face ────────────────────────
                        settings_card(
                            ui,
                            lang.t("Models & Hugging Face", "Модели и Hugging Face"),
                            |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(lang.t(
                                            "GGUF download folder",
                                            "Папка сохранения GGUF",
                                        ))
                                        .strong()
                                        .size(13.0)
                                        .color(pal.ink),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button(lang.t("Reset path", "Сбросить путь"))
                                                .on_hover_text(lang.t(
                                                    "Restore default models folder",
                                                    "Вернуть каталог по умолчанию",
                                                ))
                                                .clicked()
                                            {
                                                self.hf.directory = self.hf.default_directory();
                                            }
                                        },
                                    );
                                });
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.hf.directory)
                                        .hint_text("/home/user/models")
                                        .margin(egui::Margin::symmetric(10, 6))
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace),
                                );
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{}: {}",
                                            lang.t("System catalog", "Системный каталог"),
                                            catalog::MODEL_ROOT
                                        ))
                                        .monospace()
                                        .size(11.5)
                                        .color(pal.ink_muted),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button(lang.t(
                                                    "Open Models",
                                                    "Открыть модели",
                                                ))
                                                .clicked()
                                            {
                                                self.settings_open = false;
                                                self.tab = super::Tab::Models;
                                            }
                                        },
                                    );
                                });

                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(8.0);

                                // Токен Hugging Face (HF_TOKEN)
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(lang.t(
                                            "Hugging Face Token (HF_TOKEN)",
                                            "Токен Hugging Face (HF_TOKEN)",
                                        ))
                                        .strong()
                                        .size(13.0)
                                        .color(pal.ink),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let (lbl, col) = if token_present {
                                                (lang.t("Configured", "Задан"), pal.brand_text)
                                            } else {
                                                (lang.t("Not set", "Не задан"), pal.ink_muted)
                                            };
                                            egui::Frame::new()
                                                .fill(pal.surface_2)
                                                .stroke(Stroke::new(1.0, pal.line))
                                                .corner_radius(10)
                                                .inner_margin(egui::Margin::symmetric(8, 2))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        RichText::new(lbl).size(11.0).color(col),
                                                    );
                                                });
                                        },
                                    );
                                });
                                ui.label(
                                    RichText::new(lang.t(
                                        "Free read-only token from huggingface.co/settings/tokens. Lifts HTTP 429 limits; stored locally (0600).",
                                        "Бесплатный read-only токен с huggingface.co/settings/tokens. Снимает лимит HTTP 429 и хранится локально (0600).",
                                    ))
                                    .size(12.0)
                                    .color(pal.ink_muted),
                                );
                                ui.horizontal(|ui| {
                                    let reserved = if token_present { 216.0 } else { 124.0 };
                                    let input_w = (ui.available_width() - reserved).max(160.0);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.hf.token_draft)
                                            .password(true)
                                            .margin(egui::Margin::symmetric(10, 6))
                                            .hint_text(if token_present {
                                                lang.t(
                                                    "Paste new hf_… token to replace",
                                                    "Вставьте новый токен hf_… для замены",
                                                )
                                            } else {
                                                "hf_…"
                                            })
                                            .desired_width(input_w),
                                    );
                                    let has_draft = !self.hf.token_draft.trim().is_empty();
                                    let apply_lbl = lang.t("Apply", "Применить");
                                    let apply_btn = if has_draft {
                                        egui::Button::new(
                                            RichText::new(apply_lbl)
                                                .color(pal.on_brand)
                                                .strong(),
                                        )
                                        .fill(pal.brand)
                                        .stroke(Stroke::NONE)
                                    } else {
                                        egui::Button::new(apply_lbl)
                                    };
                                    if ui.add_enabled(has_draft, apply_btn).clicked() {
                                        let tok = self.hf.token_draft.trim().to_string();
                                        self.hf.token_draft.clear();
                                        self.hf.save_token(&ctx, tok);
                                        self.status = lang
                                            .t("HF token saved", "Токен Hugging Face сохранён")
                                            .into();
                                    }
                                    if token_present {
                                        let clear_btn = egui::Button::new(
                                            RichText::new(lang.t("Clear", "Сбросить"))
                                                .color(pal.err),
                                        );
                                        if ui.add(clear_btn).clicked() {
                                            self.hf.save_token(&ctx, String::new());
                                            self.status = lang
                                                .t("HF token removed", "Токен Hugging Face удалён")
                                                .into();
                                        }
                                    }
                                });
                            },
                        );

                        ui.add_space(12.0);

                        // ── 2. Режим движка и KAIC API ─────────────────────
                        settings_card(
                            ui,
                            lang.t("Inference Engine", "Движок инференса"),
                            |ui| {
                                ui.add_enabled_ui(!self.busy, |ui| {
                                    ui.horizontal(|ui| {
                                        for (use_kaic, label) in [
                                            (false, lang.t("Local llama.cpp", "Локальный llama.cpp")),
                                            (true, "KAIC API (127.0.0.1)"),
                                        ] {
                                            let sel = self.state.settings.use_kaic == use_kaic;
                                            let btn = egui::Button::new(
                                                RichText::new(label).size(12.5).color(if sel {
                                                    pal.ink
                                                } else {
                                                    pal.ink_muted
                                                }),
                                            )
                                            .fill(if sel {
                                                pal.surface_2
                                            } else {
                                                Color32::TRANSPARENT
                                            })
                                            .stroke(Stroke::new(
                                                1.0,
                                                if sel { pal.brand } else { pal.line },
                                            ))
                                            .corner_radius(6);
                                            if ui.add(btn).clicked() && !sel {
                                                self.state.settings.use_kaic = use_kaic;
                                                self.mark_dirty();
                                            }
                                        }
                                    });
                                    if self.state.settings.use_kaic {
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(lang.t(
                                                "KAIC endpoint (loopback only)",
                                                "Адрес KAIC (только loopback)",
                                            ))
                                            .size(12.0)
                                            .color(pal.ink_muted),
                                        );
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(
                                                    &mut self.state.settings.kaic_url,
                                                )
                                                .margin(egui::Margin::symmetric(10, 6))
                                                .desired_width(f32::INFINITY)
                                                .font(egui::TextStyle::Monospace),
                                            )
                                            .changed()
                                        {
                                            self.kaic_info = None;
                                            self.mark_dirty();
                                        }
                                        ui.label(
                                            RichText::new(lang.t(
                                                "KAIC token · memory only",
                                                "Токен KAIC · только в памяти",
                                            ))
                                            .size(12.0)
                                            .color(pal.ink_muted),
                                        );
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.token)
                                                .password(true)
                                                .margin(egui::Margin::symmetric(10, 6))
                                                .desired_width(f32::INFINITY),
                                        );
                                        ui.add_space(4.0);
                                        if ui
                                            .button(lang.t("Test KAIC", "Проверить KAIC"))
                                            .clicked()
                                        {
                                            self.start(Job::InspectKaic {
                                                url: self.state.settings.kaic_url.clone(),
                                                token: Secret::new(self.token.clone()),
                                            });
                                        }
                                        if let Some(v) = &self.kaic_info {
                                            egui::CollapsingHeader::new(lang.t(
                                                "KAIC response",
                                                "Ответ KAIC",
                                            ))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        serde_json::to_string_pretty(v)
                                                            .unwrap_or_default(),
                                                    )
                                                    .selectable(true)
                                                    .wrap(),
                                                );
                                            });
                                        }
                                    }
                                });
                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(10.0);
                                let sd_ok = crate::sd_cpp::binary_exists(
                                    &self.state.settings.sd_cli_path,
                                );
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(lang.t(
                                            "Image & Video Engine (stable-diffusion.cpp)",
                                            "Движок фото и видео (stable-diffusion.cpp)",
                                        ))
                                        .strong()
                                        .size(13.0)
                                        .color(pal.ink),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let (lbl, col) = if sd_ok {
                                                (
                                                    lang.t("Ready · CUDA", "Готов · CUDA"),
                                                    pal.brand_text,
                                                )
                                            } else {
                                                (
                                                    lang.t("Not found", "Не найден"),
                                                    pal.ink_muted,
                                                )
                                            };
                                            egui::Frame::new()
                                                .fill(pal.surface_2)
                                                .stroke(Stroke::new(1.0, pal.line))
                                                .corner_radius(10)
                                                .inner_margin(egui::Margin::symmetric(8, 2))
                                                .show(ui, |ui| {
                                                    ui.label(
                                                        RichText::new(lbl).size(11.0).color(col),
                                                    );
                                                });
                                        },
                                    );
                                });
                                ui.horizontal(|ui| {
                                    let input_w = (ui.available_width() - 108.0).max(160.0);
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(
                                                &mut self.state.settings.sd_cli_path,
                                            )
                                            .hint_text("~/.local/opt/kaic-studio/bin/sd-cli")
                                            .margin(egui::Margin::symmetric(10, 6))
                                            .desired_width(input_w)
                                            .font(egui::TextStyle::Monospace),
                                        )
                                        .changed()
                                    {
                                        self.mark_dirty();
                                    }
                                    if ui
                                        .small_button(lang.t("Reset path", "Сбросить"))
                                        .clicked()
                                    {
                                        self.state.settings.sd_cli_path =
                                            crate::storage::default_sd_cli_path();
                                        self.mark_dirty();
                                    }
                                });
                                ui.label(
                                    RichText::new(lang.t(
                                        "Runs GGUF/safetensors image (FLUX, SDXL, Qwen-Image) and video (Wan2.1) models locally. Automatically unloads llama.cpp to free VRAM.",
                                        "Запускает локальные GGUF/safetensors модели фото (FLUX, SDXL, Qwen-Image) и видео (Wan2.1). Автоматически выгружает llama.cpp для освобождения VRAM.",
                                    ))
                                    .size(11.5)
                                    .color(pal.ink_muted),
                                );
                            },
                        );

                        ui.add_space(12.0);

                        // ── 4. Хранилище данных ────────────────────────────
                        settings_card(
                            ui,
                            lang.t("App Data", "Данные приложения"),
                            |ui| {
                                ui.label(
                                    RichText::new(self.data.display().to_string())
                                        .monospace()
                                        .size(12.0)
                                        .color(pal.ink),
                                );
                                ui.label(
                                    RichText::new(lang.t(
                                        "Chats and attachments are stored locally. KAIC token is never written to disk.",
                                        "Разговоры и вложения хранятся локально. Токен KAIC на диск не записывается.",
                                    ))
                                    .size(12.0)
                                    .color(pal.ink_muted),
                                );
                            },
                        );
                        ui.add_space(20.0);
                    });
                });
            });
    }
}

fn settings_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    let pal = palette::p();
    let inner_w = (ui.available_width() - 34.0).max(200.0);
    egui::Frame::new()
        .fill(pal.surface_1)
        .stroke(Stroke::new(1.0, pal.line))
        .corner_radius(10)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(inner_w);
            ui.label(RichText::new(title).strong().size(14.0).color(pal.ink));
            ui.add_space(10.0);
            body(ui);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_success_requires_persistence() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&egui::Context::default(), dir.path().to_path_buf());
        assert_eq!(app.lang(), super::super::i18n::Lang::En);
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        app.state_path = blocker.join("state.json");
        app.status = "Unsaved".into();
        assert!(app.save_settings().is_err());
        assert_eq!(app.status, "Unsaved");
        assert!(app.error.is_some());
        assert!(app.dirty.is_some());
        app.state_path = dir.path().join("state.json");
        app.state.settings.language = "ru".into();
        app.save_settings().unwrap();
        assert!(app.status.starts_with("Настройки сохранены"));
        assert!(app.dirty.is_none());
        let saved: crate::storage::SavedState = crate::storage::load_json(&app.state_path).unwrap();
        assert_eq!(saved.settings.kaic_url, app.state.settings.kaic_url);
        assert_eq!(saved.settings.language, "ru");
    }
}
