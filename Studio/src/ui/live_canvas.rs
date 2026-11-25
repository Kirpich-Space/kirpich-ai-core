//! Живой мультиагентный холст динамического редактирования (Pure Rust / egui):
//! - до 4 независимых курсоров (`VirtualPointer`) и виртуальных клавиатур (`VirtualKeyboard`),
//! - живое редактирование файлов workspace через kernel `openat(O_NOFOLLOW)`,
//! - аварийный стоп (`control-live` `#F2645F`) и честная матрица OS input-бэкендов.
use super::{Studio, icons, palette};
use crate::agent::workspace_live::{
    MAX_AGENT_SEATS, OsInputBackendCapabilities, SeatManager, probe_os_input_backends,
};
use eframe::egui::{self, RichText, Stroke};
use std::{io::Read, path::Path};

pub(super) struct LiveCanvas {
    pub seats: SeatManager,
    pub os_backends: Vec<OsInputBackendCapabilities>,
    pub new_seat_name: String,
    pub new_seat_paths: String,
    pub open_file: String,
    pub editor_buffer: String,
    pub loaded_sha256: Option<String>,
    pub workspace_files: Vec<String>,
    pub canvas_notice: Option<String>,
}

impl Default for LiveCanvas {
    fn default() -> Self {
        Self {
            seats: SeatManager::with_default_orchestrator(),
            os_backends: probe_os_input_backends(),
            new_seat_name: String::new(),
            new_seat_paths: String::new(),
            open_file: String::new(),
            editor_buffer: String::new(),
            loaded_sha256: None,
            workspace_files: Vec::new(),
            canvas_notice: None,
        }
    }
}

impl LiveCanvas {
    pub fn refresh_files(&mut self, workspace: &str) {
        self.workspace_files.clear();
        let trimmed = workspace.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(guard) = crate::agent::WorkspaceGuard::new(Path::new(trimmed)) else {
            return;
        };
        if let Ok(exec) =
            crate::agent::tools::exec_list_files(&guard, &serde_json::json!({"max_entries": 120}))
        {
            self.workspace_files = exec
                .output
                .lines()
                .filter(|l| !l.ends_with('/') && !l.starts_with('(') && !l.starts_with('…'))
                .map(str::to_owned)
                .collect();
        }
    }

    pub fn load_file(&mut self, workspace: &str, rel_path: &str) -> anyhow::Result<()> {
        let guard = crate::agent::WorkspaceGuard::new(Path::new(workspace.trim()))?;
        let (mut f, rel) = guard.open_read_no_follow(rel_path)?;
        let mut text = String::new();
        f.by_ref().take(256 * 1024).read_to_string(&mut text)?;
        drop(f);
        let (_, sha) = guard.hash_file_no_follow(&rel)?;
        self.open_file = rel;
        self.editor_buffer = text;
        self.loaded_sha256 = Some(sha);
        self.canvas_notice = None;
        Ok(())
    }

    pub fn save_current_file(&mut self, workspace: &str) -> anyhow::Result<()> {
        if self.open_file.trim().is_empty() {
            anyhow::bail!("Сначала выберите файл внутри workspace");
        }
        let guard = crate::agent::WorkspaceGuard::new(Path::new(workspace.trim()))?;
        let rel = guard.atomic_write_no_follow(&self.open_file, self.editor_buffer.as_bytes())?;
        let (_, sha) = guard.hash_file_no_follow(&rel)?;
        self.loaded_sha256 = Some(sha);
        self.canvas_notice = Some(format!("Сохранено через openat(O_NOFOLLOW): {rel}"));
        Ok(())
    }
}

impl Studio {
    pub(super) fn live_canvas_ui(&mut self, ui: &mut egui::Ui) {
        let pal = palette::p();
        let lang = self.lang();
        let ws_path = self.state.chats[self.active].options.workspace_path.clone();
        let control_live = self.live_canvas.seats.is_control_live();

        // Красная рамка и панель аварийного стопа (`control-live` #F2645F из kaic-palette.json)
        if control_live {
            egui::Frame::new()
                .fill(pal.surface_1)
                .stroke(Stroke::new(2.0, pal.err))
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        icons::dot(ui, pal.err);
                        ui.label(
                            RichText::new(lang.t(
                                "LIVE CONTROL ACTIVE — agent seats are editing workspace",
                                "АКТИВНОЕ УПРАВЛЕНИЕ — агентные курсоры/клавиатуры правят workspace",
                            ))
                            .strong()
                            .size(12.5)
                            .color(pal.err),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let stop_btn = egui::Button::new(
                                RichText::new(lang.t("STOP ALL SEATS", "СТОП ВСЕ КУРСОРЫ"))
                                    .strong()
                                    .color(pal.on_brand),
                            )
                            .fill(pal.err)
                            .stroke(Stroke::NONE)
                            .corner_radius(6);
                            if ui.add(stop_btn).clicked() {
                                self.live_canvas.seats.emergency_stop();
                                self.worker.cancel.cancel();
                                self.status = lang
                                    .t(
                                        "Emergency stop: all agent cursors & keyboards halted",
                                        "Аварийный стоп: все курсоры и клавиатуры агентов остановлены",
                                    )
                                    .into();
                            }
                        });
                    });
                });
            ui.add_space(8.0);
        }

        // Шапка вкладки «Холст / Workspace»
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(lang.t("Multi-Agent Live Workspace", "Живой мультиагентный холст"))
                    .strong()
                    .size(16.0)
                    .color(pal.ink),
            );
            ui.label(
                RichText::new(format!(
                    "· {}/{} {}",
                    self.live_canvas.seats.seats().len(),
                    MAX_AGENT_SEATS,
                    lang.t("cursors & keyboards", "курсоров и клавиатур")
                ))
                .monospace()
                .size(11.5)
                .color(pal.ink_muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(lang.t("Refresh files", "Обновить файлы"))
                    .clicked()
                {
                    self.live_canvas.refresh_files(&ws_path);
                }
                if !self.live_canvas.open_file.is_empty() {
                    let save_btn = egui::Button::new(
                        RichText::new(lang.t("Save file", "Сохранить файл"))
                            .strong()
                            .color(pal.on_brand),
                    )
                    .fill(pal.brand)
                    .stroke(Stroke::NONE)
                    .corner_radius(6);
                    if ui.add(save_btn).clicked() {
                        match self.live_canvas.save_current_file(&ws_path) {
                            Ok(()) => {}
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                }
            });
        });
        ui.add_space(6.0);

        // Явный переключатель динамического взаимодействия (отключено по умолчанию)
        let dyn_enabled = self.live_canvas.seats.is_dynamic_interaction_enabled();
        egui::Frame::new()
            .fill(pal.surface_1)
            .stroke(Stroke::new(
                1.0,
                if dyn_enabled { pal.brand } else { pal.line },
            ))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    icons::dot(ui, if dyn_enabled { pal.ok } else { pal.line_strong });
                    ui.label(
                        RichText::new(if dyn_enabled {
                            lang.t(
                                "Dynamic interaction ACTIVE (explicitly enabled)",
                                "Динамическое взаимодействие ВКЛЮЧЕНО пользователем",
                            )
                        } else {
                            lang.t(
                                "Dynamic interaction DISABLED by default",
                                "Динамическое взаимодействие отключено по умолчанию",
                            )
                        })
                        .size(12.0)
                        .strong()
                        .color(if dyn_enabled {
                            pal.ink
                        } else {
                            pal.ink_muted
                        }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let toggle_lbl = if dyn_enabled {
                            lang.t("Disable dynamic mode", "Отключить динамический режим")
                        } else {
                            lang.t("Activate dynamic mode", "Активировать динамический режим")
                        };
                        let btn = egui::Button::new(
                            RichText::new(toggle_lbl)
                                .size(11.5)
                                .strong()
                                .color(if dyn_enabled { pal.ink } else { pal.on_brand }),
                        )
                        .fill(if dyn_enabled {
                            pal.surface_2
                        } else {
                            pal.brand
                        })
                        .stroke(Stroke::new(
                            1.0,
                            if dyn_enabled {
                                pal.line_strong
                            } else {
                                pal.brand
                            },
                        ))
                        .corner_radius(6);
                        if ui.add(btn).clicked() {
                            let next = !dyn_enabled;
                            self.live_canvas.seats.set_dynamic_interaction(next);
                            self.state.chats[self.active]
                                .options
                                .dynamic_interaction_enabled = next;
                            self.mark_dirty();
                        }
                    });
                });
            });
        ui.add_space(6.0);

        // Поле пути workspace прямо на холсте (синхронизировано с текущим чатом)
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Workspace:")
                    .monospace()
                    .size(11.5)
                    .color(pal.ink_muted),
            );
            let mut ws_edit = ws_path.clone();
            if ui
                .add_enabled(
                    !self.busy,
                    egui::TextEdit::singleline(&mut ws_edit)
                        .hint_text("/abs/path/to/workspace")
                        .desired_width((ui.available_width() - 120.0).max(160.0))
                        .font(egui::TextStyle::Monospace),
                )
                .changed()
            {
                self.state.chats[self.active].options.workspace_path = ws_edit.clone();
                self.mark_dirty();
            }
            if ui.button(lang.t("Load dir", "Открыть")).clicked() {
                self.live_canvas.refresh_files(&ws_edit);
            }
        });
        ui.add_space(8.0);

        let narrow = ui.available_width() < 860.0;
        if narrow {
            self.render_seats_rack(ui, &ws_path);
            ui.add_space(8.0);
            self.render_live_editor(ui, &ws_path);
        } else {
            ui.columns(2, |cols| {
                cols[0].set_max_width(340.0);
                self.render_seats_rack(&mut cols[0], &ws_path);
                self.render_live_editor(&mut cols[1], &ws_path);
            });
        }
    }

    fn render_seats_rack(&mut self, ui: &mut egui::Ui, ws_path: &str) {
        let pal = palette::p();
        let lang = self.lang();
        egui::ScrollArea::vertical()
            .id_salt("live-canvas-seats-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(pal.surface_1)
                    .stroke(Stroke::new(1.0, pal.line))
                    .corner_radius(10)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(lang.t(
                                "Agent Cursors & Virtual Keyboards",
                                "Курсоры и виртуальные клавиатуры агентов",
                            ))
                            .strong()
                            .size(13.0)
                            .color(pal.ink),
                        );
                        ui.add_space(6.0);

                        let mut remove_id: Option<String> = None;
                        let mut toggle_ptr: Option<(String, bool)> = None;
                        let mut toggle_kbd: Option<(String, bool)> = None;
                        let mut jump_file: Option<String> = None;

                        for seat in self.live_canvas.seats.seats() {
                            let col = pal.agent_color(seat.color_slot);
                            let border = if seat.control_live { pal.err } else { col };
                            egui::Frame::new()
                                .fill(pal.surface_0)
                                .stroke(Stroke::new(1.2, border))
                                .corner_radius(8)
                                .inner_margin(8)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        icons::dot(ui, col);
                                        ui.label(
                                            RichText::new(&seat.agent_name)
                                                .strong()
                                                .size(12.0)
                                                .color(pal.ink),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .small_button(RichText::new("×").color(pal.err))
                                                    .on_hover_text(lang.t(
                                                        "Remove seat",
                                                        "Удалить курсор и клавиатуру",
                                                    ))
                                                    .clicked()
                                                {
                                                    remove_id = Some(seat.seat_id.clone());
                                                }
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · {}",
                                                        seat.seat_id, seat.keyboard.keyboard_id
                                                    ))
                                                    .monospace()
                                                    .size(10.0)
                                                    .color(col),
                                                );
                                            },
                                        );
                                    });
                                    ui.horizontal_wrapped(|ui| {
                                        let mut ptr_on = seat.pointer.enabled;
                                        if ui
                                            .checkbox(
                                                &mut ptr_on,
                                                RichText::new(lang.t("Cursor", "Курсор"))
                                                    .size(11.0),
                                            )
                                            .changed()
                                        {
                                            toggle_ptr = Some((seat.seat_id.clone(), ptr_on));
                                        }
                                        let mut kbd_on = seat.keyboard.enabled;
                                        if ui
                                            .checkbox(
                                                &mut kbd_on,
                                                RichText::new(lang.t("Keyboard", "Клавиатура"))
                                                    .size(11.0),
                                            )
                                            .changed()
                                        {
                                            toggle_kbd = Some((seat.seat_id.clone(), kbd_on));
                                        }
                                    });
                                    if let Some(tf) = &seat.pointer.target_file {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{tf}:L{}:C{}",
                                                    seat.pointer.line, seat.pointer.col
                                                ))
                                                .monospace()
                                                .size(10.5)
                                                .color(col),
                                            );
                                            if ui.small_button(lang.t("View", "Открыть")).clicked()
                                            {
                                                jump_file = Some(tf.clone());
                                            }
                                        });
                                    }
                                    if !seat.owned_paths.is_empty() {
                                        ui.label(
                                            RichText::new(format!(
                                                "owned: {}",
                                                seat.owned_paths.join(", ")
                                            ))
                                            .monospace()
                                            .size(10.0)
                                            .color(pal.ink_muted),
                                        );
                                    }
                                    if !seat.keyboard.last_keys.is_empty() {
                                        ui.label(
                                            RichText::new(format!(
                                                "⌨ {} (#{} ops)",
                                                seat.keyboard.last_keys, seat.keyboard.ops_count
                                            ))
                                            .monospace()
                                            .size(10.0)
                                            .color(pal.ceramic_200),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new(&seat.status)
                                                .size(10.5)
                                                .color(pal.ink_muted),
                                        );
                                    }
                                });
                            ui.add_space(4.0);
                        }

                        if let Some(id) = remove_id {
                            self.live_canvas.seats.remove_seat(&id);
                        }
                        if let Some((id, on)) = toggle_ptr {
                            let _ = self.live_canvas.seats.toggle_pointer(&id, on);
                        }
                        if let Some((id, on)) = toggle_kbd {
                            let _ = self.live_canvas.seats.toggle_keyboard(&id, on);
                        }
                        if let Some(f) = jump_file
                            && let Err(e) = self.live_canvas.load_file(ws_path, &f)
                        {
                            self.error = Some(format!("{e:#}"));
                        }

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(lang.t(
                                "+ Add agent cursor & keyboard",
                                "+ Добавить курсор и клавиатуру агента",
                            ))
                            .strong()
                            .size(11.5)
                            .color(pal.ink),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.live_canvas.new_seat_name)
                                .hint_text(
                                    lang.t(
                                        "Agent name (e.g. agent-2)",
                                        "Имя агента (напр. agent-2)",
                                    ),
                                )
                                .desired_width(f32::INFINITY),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.live_canvas.new_seat_paths)
                                .hint_text(lang.t(
                                    "Owned paths, comma-separated (e.g. src/a.rs)",
                                    "owned_paths через запятую (напр. src/a.rs)",
                                ))
                                .desired_width(f32::INFINITY)
                                .font(egui::TextStyle::Monospace),
                        );
                        let can_add = self.live_canvas.seats.seats().len() < MAX_AGENT_SEATS;
                        if ui
                            .add_enabled(
                                can_add,
                                egui::Button::new(
                                    RichText::new(lang.t("+ Add Seat", "+ Добавить место"))
                                        .color(pal.on_brand)
                                        .strong(),
                                )
                                .fill(pal.brand)
                                .stroke(Stroke::NONE)
                                .corner_radius(6),
                            )
                            .clicked()
                        {
                            let next_num = self.live_canvas.seats.seats().len() + 1;
                            let name = if self.live_canvas.new_seat_name.trim().is_empty() {
                                format!("agent-{next_num}")
                            } else {
                                self.live_canvas.new_seat_name.trim().to_string()
                            };
                            let owned: Vec<String> = self
                                .live_canvas
                                .new_seat_paths
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            match self.live_canvas.seats.add_seat(name, owned) {
                                Ok(_) => {
                                    self.live_canvas.new_seat_name.clear();
                                    self.live_canvas.new_seat_paths.clear();
                                }
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }

                        ui.add_space(8.0);
                        egui::CollapsingHeader::new(
                            RichText::new(lang.t(
                                "OS Input Backends (honest status)",
                                "Системные input-бэкенды (честный статус)",
                            ))
                            .size(11.0)
                            .color(pal.ink_muted),
                        )
                        .id_salt("live-canvas-os-backends")
                        .default_open(false)
                        .show(ui, |ui| {
                            let gate_stopped = self.live_canvas.seats.is_emergency_stopped();
                            ui.label(
                                RichText::new(format!(
                                    "• Engine: {} (gate={})",
                                    crate::agent::accel_backend_label(),
                                    if gate_stopped { "STOPPED" } else { "armed" }
                                ))
                                .monospace()
                                .size(10.0)
                                .color(if gate_stopped {
                                    pal.err
                                } else {
                                    pal.ok
                                }),
                            );
                            for b in &self.live_canvas.os_backends {
                                let col = if b.status == crate::agent::FeatureStatus::Tested {
                                    pal.ok
                                } else {
                                    pal.ink_muted
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "• {} [{}]",
                                        b.label,
                                        b.status.label_ru()
                                    ))
                                    .size(10.5)
                                    .color(col),
                                );
                                ui.label(RichText::new(&b.notes).size(10.0).color(pal.ink_muted));
                            }
                        });
                    });
            });
    }

    fn render_live_editor(&mut self, ui: &mut egui::Ui, ws_path: &str) {
        let pal = palette::p();
        let lang = self.lang();
        egui::Frame::new()
            .fill(pal.surface_1)
            .stroke(Stroke::new(1.0, pal.line))
            .corner_radius(10)
            .inner_margin(10)
            .show(ui, |ui| {
                // Полоса файлов workspace
                if !self.live_canvas.workspace_files.is_empty() {
                    let mut pick: Option<String> = None;
                    egui::ScrollArea::horizontal()
                        .id_salt("live-canvas-file-tabs")
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for f in self.live_canvas.workspace_files.iter().take(24) {
                                    let active = self.live_canvas.open_file == *f;
                                    let btn = egui::Button::new(
                                        RichText::new(f).monospace().size(11.0).color(if active {
                                            pal.ink
                                        } else {
                                            pal.ink_muted
                                        }),
                                    )
                                    .fill(if active { pal.surface_2 } else { pal.surface_0 })
                                    .stroke(Stroke::new(
                                        1.0,
                                        if active { pal.brand } else { pal.line },
                                    ))
                                    .corner_radius(5);
                                    if ui.add(btn).clicked() {
                                        pick = Some(f.clone());
                                    }
                                }
                            });
                        });
                    if let Some(f) = pick
                        && let Err(e) = self.live_canvas.load_file(ws_path, &f)
                    {
                        self.error = Some(format!("{e:#}"));
                    }
                    ui.add_space(6.0);
                }

                // Полоса активных агентных курсоров на текущем файле
                let active_file = self.live_canvas.open_file.clone();
                let cursors_here: Vec<(String, String, u8, usize, usize)> = self
                    .live_canvas
                    .seats
                    .seats()
                    .iter()
                    .filter(|s| {
                        s.pointer.enabled
                            && s.pointer.target_file.as_deref() == Some(active_file.as_str())
                    })
                    .map(|s| {
                        (
                            s.agent_name.clone(),
                            s.keyboard.keyboard_id.clone(),
                            s.color_slot,
                            s.pointer.line,
                            s.pointer.col,
                        )
                    })
                    .collect();

                if !cursors_here.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(lang.t("Active cursors:", "Курсоры в файле:"))
                                .size(11.0)
                                .color(pal.ink_muted),
                        );
                        for (name, kbd, slot, line, col_pos) in &cursors_here {
                            let c = pal.agent_color(*slot);
                            egui::Frame::new()
                                .fill(pal.surface_0)
                                .stroke(Stroke::new(1.0, c))
                                .corner_radius(6)
                                .inner_margin(egui::Margin::symmetric(6, 2))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "▍ {name} ({kbd}) · L{line}:C{col_pos}"
                                        ))
                                        .monospace()
                                        .size(10.5)
                                        .color(c),
                                    );
                                });
                        }
                    });
                    ui.add_space(6.0);
                }

                if let Some(note) = &self.live_canvas.canvas_notice {
                    ui.label(RichText::new(note).size(11.0).color(pal.ok));
                    ui.add_space(4.0);
                }

                if self.live_canvas.open_file.is_empty() {
                    ui.label(
                        RichText::new(lang.t(
                            "Enter an absolute workspace path above and click Load dir to open files for live multi-agent editing.",
                            "Укажите абсолютный путь workspace выше и нажмите «Открыть», чтобы редактировать файлы вместе с агентными курсорами.",
                        ))
                        .size(12.0)
                        .color(pal.ink_muted),
                    );
                } else {
                    if let Some(sha) = &self.live_canvas.loaded_sha256 {
                        let short: String = sha.chars().take(12).collect();
                        ui.label(
                            RichText::new(format!("{} · sha256:{short}", self.live_canvas.open_file))
                                .monospace()
                                .size(10.5)
                                .color(pal.ink_muted),
                        );
                    }
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("live-canvas-code-editor")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.live_canvas.editor_buffer)
                                    .code_editor()
                                    .desired_rows(20)
                                    .desired_width(f32::INFINITY),
                            );
                        });
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_canvas_renders_multi_cursors_keyboards_and_control_live_banner() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.state.chats[0].options.workspace_path = ws.display().to_string();
        app.tab = super::super::Tab::Workspace;
        app.live_canvas.refresh_files(&ws.display().to_string());
        app.live_canvas
            .load_file(&ws.display().to_string(), "main.rs")
            .unwrap();
        let s2 = app
            .live_canvas
            .seats
            .add_seat("agent-2", vec!["main.rs".into()])
            .unwrap()
            .seat_id
            .clone();
        let guard = crate::agent::WorkspaceGuard::new(&ws).unwrap();
        assert!(!app.live_canvas.seats.is_dynamic_interaction_enabled());
        assert!(
            app.live_canvas
                .seats
                .apply_live_edit(
                    &guard,
                    &s2,
                    crate::agent::workspace_live::LiveEditOp::MoveCursor {
                        file: "main.rs".into(),
                        line: 2,
                        col: 5,
                    },
                )
                .is_err()
        );
        app.live_canvas.seats.set_dynamic_interaction(true);
        app.live_canvas
            .seats
            .apply_live_edit(
                &guard,
                &s2,
                crate::agent::workspace_live::LiveEditOp::MoveCursor {
                    file: "main.rs".into(),
                    line: 2,
                    col: 5,
                },
            )
            .unwrap();
        // Активируем control_live для проверки красной панели аварийного стопа
        app.live_canvas
            .seats
            .apply_live_edit(
                &guard,
                &s2,
                crate::agent::workspace_live::LiveEditOp::ReplaceText {
                    file: "main.rs".into(),
                    old_text: "hi".into(),
                    new_text: "hello".into(),
                },
            )
            .unwrap();
        assert!(app.live_canvas.seats.is_control_live());

        for size in [egui::vec2(1280.0, 800.0), egui::vec2(680.0, 720.0)] {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| app.render(ui),
            );
            out.textures_delta.clear();
        }
    }
}
