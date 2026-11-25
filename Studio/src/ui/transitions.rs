//! Состояния нативного переноса ChatSidebar / ModelPicker (Ollama, MIT).
use super::Studio;

impl Studio {
    pub(super) fn chat_matches(&self, index: usize, query: &str) -> bool {
        let query = query.to_lowercase();
        let c = &self.state.chats[index];
        c.title.to_lowercase().contains(&query)
            || c.messages
                .iter()
                .any(|m| m.content.to_lowercase().contains(&query))
    }
    pub(super) fn rename_chat(&mut self, index: usize, title: &str) -> bool {
        if self.busy || title.trim().is_empty() {
            return false;
        }
        let Some(chat) = self.state.chats.get_mut(index) else {
            return false;
        };
        let old = chat.clone();
        chat.title = title.trim().to_owned();
        chat.updated_at = crate::storage::epoch_now();
        if self.persist().is_err() {
            self.state.chats[index] = old;
            return false;
        }
        true
    }
    // Invoked only by the explicit confirmation button, never by startup or polling.
    pub(super) fn confirm_delete(&mut self) -> bool {
        if self.busy {
            return false;
        }
        let Some(id) = self.delete_confirmation else {
            return false;
        };
        let Some(index) = self.state.chats.iter().position(|c| c.id == id) else {
            return false;
        };
        let old = self.state.chats.clone();
        let active_id = self.state.chats[self.active].id;
        self.state.chats.remove(index);
        if self.state.chats.is_empty() {
            self.state.chats.push(crate::storage::Chat::new());
        }
        if self.persist().is_err() {
            self.state.chats = old;
            return false;
        }
        self.active = self
            .state
            .chats
            .iter()
            .position(|c| c.id == active_id)
            .unwrap_or(0);
        if active_id == id {
            self.composer.clear();
            self.attachments.clear();
        }
        self.delete_confirmation = None;
        self.selected_chats.remove(&id);
        self.rename = None;
        true
    }
    pub(super) fn confirm_batch_delete(&mut self) -> bool {
        if self.busy || self.selected_chats.is_empty() {
            return false;
        }
        let old = self.state.chats.clone();
        let active_id = self.state.chats[self.active].id;
        let ids = self.selected_chats.clone();
        self.state.chats.retain(|c| !ids.contains(&c.id));
        if self.state.chats.is_empty() {
            self.state.chats.push(crate::storage::Chat::new());
        }
        if self.persist().is_err() {
            self.state.chats = old;
            return false;
        }
        self.active = self
            .state
            .chats
            .iter()
            .position(|c| c.id == active_id)
            .unwrap_or(0);
        if ids.contains(&active_id) {
            self.composer.clear();
            self.attachments.clear();
        }
        self.selected_chats.clear();
        self.select_mode = false;
        self.batch_delete_confirmation = false;
        self.rename = None;
        true
    }
    pub(super) fn delete_dialog(&mut self, ctx: &eframe::egui::Context) {
        let lang = self.lang();
        if self.batch_delete_confirmation {
            let count = self.selected_chats.len();
            if count == 0 {
                self.batch_delete_confirmation = false;
                return;
            }
            let mut confirm = false;
            let mut cancel = false;
            let pal = super::palette::p();
            eframe::egui::Modal::new(eframe::egui::Id::new("удалить-пачку-разговоров")).show(
                ctx,
                |ui| {
                    ui.set_max_width(360.0);
                    ui.heading(
                        eframe::egui::RichText::new(format!(
                            "{} ({count})?",
                            lang.t("Delete chats", "Удалить разговоры")
                        ))
                        .color(pal.ink)
                        .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        eframe::egui::RichText::new(lang.t(
                            "Selected chats will be permanently removed from local history.",
                            "Выбранные разговоры будут удалены из сохранённой истории. Отменить действие нельзя.",
                        ))
                        .size(12.5)
                        .color(pal.ink_muted),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(lang.t("Cancel", "Отмена")).clicked() {
                            cancel = true;
                        }
                        let delete_btn = eframe::egui::Button::new(
                            eframe::egui::RichText::new(format!(
                                "{} ({count})",
                                lang.t("Delete", "Удалить")
                            ))
                            .color(pal.on_brand)
                            .strong(),
                        )
                        .fill(pal.err)
                        .stroke(eframe::egui::Stroke::NONE);
                        if ui.add_enabled(!self.busy, delete_btn).clicked() {
                            confirm = true;
                        }
                    });
                    if ui.input_mut(|i| {
                        i.consume_key(eframe::egui::Modifiers::NONE, eframe::egui::Key::Escape)
                    }) {
                        cancel = true;
                    }
                },
            );
            if cancel {
                self.batch_delete_confirmation = false;
            } else if confirm {
                self.confirm_batch_delete();
            }
            return;
        }
        let Some(id) = self.delete_confirmation else {
            return;
        };
        let Some(chat) = self.state.chats.iter().find(|c| c.id == id) else {
            self.delete_confirmation = None;
            return;
        };
        let title = lang.chat_title(&chat.title).to_string();
        let mut confirm = false;
        let mut cancel = false;
        let pal = super::palette::p();
        eframe::egui::Modal::new(eframe::egui::Id::new("удалить-разговор")).show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.heading(
                eframe::egui::RichText::new(lang.t("Delete chat?", "Удалить разговор?"))
                    .color(pal.ink)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.add(
                eframe::egui::Label::new(eframe::egui::RichText::new(title).color(pal.ink)).wrap(),
            );
            ui.label(
                eframe::egui::RichText::new(lang.t(
                    "This chat will be permanently removed from local history.",
                    "Разговор будет удалён из сохранённой истории. Отменить действие нельзя.",
                ))
                .size(12.5)
                .color(pal.ink_muted),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(lang.t("Cancel", "Отмена")).clicked() {
                    cancel = true;
                }
                let delete_btn = eframe::egui::Button::new(
                    eframe::egui::RichText::new(lang.t("Delete chat", "Удалить разговор"))
                        .color(pal.on_brand)
                        .strong(),
                )
                .fill(pal.err)
                .stroke(eframe::egui::Stroke::NONE);
                if ui.add_enabled(!self.busy, delete_btn).clicked() {
                    confirm = true;
                }
            });
            if ui.input_mut(|i| {
                i.consume_key(eframe::egui::Modifiers::NONE, eframe::egui::Key::Escape)
            }) {
                cancel = true;
            }
        });
        if cancel {
            self.delete_confirmation = None;
        } else if confirm {
            self.confirm_delete();
        }
    }
    pub(super) fn branch_message(&mut self, index: usize) {
        if self.busy {
            return;
        }
        let source = &self.state.chats[self.active];
        let Some(message) = source.messages.get(index) else {
            return;
        };
        if message.role != "user" {
            return;
        }
        let mut branch = crate::storage::Chat::new();
        branch.messages = source.messages[..index].to_vec();
        branch.model = source.model.clone();
        self.composer = message.content.clone();
        self.attachments.clear();
        self.state.chats.insert(0, branch);
        self.active = 0;
        self.mark_dirty();
    }
    pub(super) fn close_model_picker(&mut self) {
        self.model_picker_open = false;
        self.top_model_picker_open = false;
        self.model_search.clear();
    }
    pub(super) fn select_model(&mut self, repo: &str) -> bool {
        if self.busy
            || !self
                .models
                .iter()
                .any(|m| (m.selection_id() == repo || m.repo == repo) && m.loadable())
        {
            return false;
        }
        self.state.settings.selected_repo = repo.into();
        self.close_model_picker();
        self.mark_dirty();
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_delete_requires_confirmation_and_rolls_back_failed_save() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        app.state.chats[0].messages = vec![crate::storage::Message::user("Первый".into())];
        app.new_chat();
        let id = app.state.chats[0].id;
        assert!(!app.confirm_delete());
        assert_eq!(app.state.chats.len(), 2);
        app.delete_confirmation = Some(id);
        let blocker = d.path().join("blocked");
        std::fs::write(&blocker, b"fixture").unwrap();
        app.state_path = blocker.join("state.json");
        assert!(!app.confirm_delete());
        assert_eq!(app.state.chats[0].id, id);
        app.state_path = d.path().join("state.json");
        assert!(app.confirm_delete());
        assert_eq!(app.state.chats.len(), 1);
        let saved: crate::storage::SavedState = crate::storage::load_json(&app.state_path).unwrap();
        assert_eq!(saved.chats.len(), 1);
        assert_ne!(saved.chats[0].id, id);
    }
    #[test]
    fn single_empty_chat_limit_prevents_infinite_empty_chats() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        assert_eq!(app.state.chats.len(), 1);
        app.new_chat();
        app.new_chat();
        assert_eq!(
            app.state.chats.len(),
            1,
            "cannot spawn multiple empty chats"
        );
        app.state.chats[0].messages = vec![crate::storage::Message::user("Привет".into())];
        app.new_chat();
        assert_eq!(app.state.chats.len(), 2);
        app.new_chat();
        assert_eq!(app.state.chats.len(), 2);
    }
    #[test]
    fn batch_delete_removes_selected_chats_and_rolls_back_on_error() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        for text in ["Первый", "Второй", "Третий"] {
            app.state.chats[0].messages = vec![crate::storage::Message::user(text.into())];
            app.new_chat();
        }
        assert_eq!(app.state.chats.len(), 4);
        let id_a = app.state.chats[1].id;
        let id_b = app.state.chats[2].id;
        app.selected_chats.insert(id_a);
        app.selected_chats.insert(id_b);

        let blocker = d.path().join("blocked");
        std::fs::write(&blocker, b"fixture").unwrap();
        app.state_path = blocker.join("state.json");
        assert!(!app.confirm_batch_delete());
        assert_eq!(app.state.chats.len(), 4);

        app.state_path = d.path().join("state.json");
        assert!(app.confirm_batch_delete());
        assert_eq!(app.state.chats.len(), 2);
        assert!(!app.state.chats.iter().any(|c| c.id == id_a || c.id == id_b));
        assert!(app.selected_chats.is_empty());
        assert!(!app.select_mode);
    }
    #[test]
    fn finish_rename_save_failure_rolls_back() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        let old = app.state.chats[0].title.clone();
        let blocker = d.path().join("blocked");
        std::fs::write(&blocker, b"fixture").unwrap();
        app.state_path = blocker.join("state.json");
        assert!(!app.rename_chat(0, "Changed"));
        assert_eq!(app.state.chats[0].title, old);
    }
    #[test]
    fn rename_search_and_branch_preserve_source_history() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        app.state.chats[0].messages = vec![crate::storage::Message::user("Исходный запрос".into())];
        let original = app.state.chats[0].id;
        assert!(!app.rename_chat(0, "  "));
        assert!(app.rename_chat(0, "  Новый заголовок  "));
        assert_eq!(app.state.chats[0].title, "Новый заголовок");
        assert!(app.chat_matches(0, "ЗАГОЛОВОК"));
        assert!(app.chat_matches(0, "исходный"));
        assert!(!app.chat_matches(0, "нет совпадений"));
        app.branch_message(0);
        assert_ne!(app.state.chats[0].id, original);
        assert_eq!(app.composer, "Исходный запрос");
        assert!(app.state.chats[0].messages.is_empty());
        assert_eq!(app.state.chats[1].messages[0].content, "Исходный запрос");
        app.busy = true;
        assert!(!app.rename_chat(0, "Занято"));
        let count = app.state.chats.len();
        app.branch_message(0);
        assert_eq!(app.state.chats.len(), count);
    }
    #[test]
    fn picker_close_clears_search_and_busy_selection_is_guarded() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = Studio::from_context(&eframe::egui::Context::default(), d.path().into());
        app.model_search = "gemma".into();
        app.close_model_picker();
        assert!(app.model_search.is_empty());
        assert!(!app.select_model("missing"));
        app.busy = true;
        assert!(!app.select_model(crate::catalog::GEMMA));
    }
}
