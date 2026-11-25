//! Современный диалог выбора локального файла в фирменном стиле KAIC Studio (Carbon / Bone / Kiln).
use super::{anim, i18n::Lang, palette};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
};

struct CachedRow {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

pub(super) struct Browser {
    directory: PathBuf,
    back: Vec<PathBuf>,
    filename: String,
    filter: String,
    error: Option<String>,
    cached_key: Option<(PathBuf, String)>,
    cached_rows: Vec<CachedRow>,
    native_rx: Option<mpsc::Receiver<Option<PathBuf>>>,
}

fn allowed(path: &Path) -> bool {
    !path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".aws" | ".codex" | ".ssh" | ".gnupg")
        )
    })
}

fn entries(path: &Path, filter: &str) -> std::io::Result<Vec<(PathBuf, bool)>> {
    let mut result = Vec::new();
    let needle = filter.to_lowercase();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(canonical) = p.canonicalize() else {
            continue;
        };
        if !allowed(&canonical) {
            continue;
        }
        let directory = canonical.is_dir();
        if directory
            || needle.is_empty()
            || entry
                .file_name()
                .to_string_lossy()
                .to_lowercase()
                .contains(&needle)
        {
            result.push((p, directory));
        }
    }
    result.sort_by_key(|(p, d)| {
        (
            !*d,
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase(),
        )
    });
    Ok(result)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn quick_places(lang: Lang) -> Vec<(&'static str, PathBuf)> {
    let home = home_dir();
    let candidates = [
        (lang.t("Home", "Домой"), home.clone()),
        (lang.t("Documents", "Документы"), home.join("Documents")),
        (lang.t("Downloads", "Загрузки"), home.join("Downloads")),
        (lang.t("Desktop", "Рабочий стол"), home.join("Desktop")),
        (lang.t("Models", "Модели"), home.join("models")),
        (lang.t("Root /", "Корень /"), PathBuf::from("/")),
    ];
    candidates.into_iter().filter(|(_, p)| p.is_dir()).collect()
}

fn paint_folder(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
    let c = rect.center();
    let pt = |x, y| c + egui::vec2(x, y);
    let p = ui.painter();
    let stroke = Stroke::new(1.3, color);
    p.add(egui::Shape::closed_line(
        vec![
            pt(-7.0, -5.0),
            pt(-2.0, -5.0),
            pt(-0.5, -3.0),
            pt(7.0, -3.0),
            pt(7.0, 5.5),
            pt(-7.0, 5.5),
        ],
        stroke,
    ));
}

fn paint_file(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
    let c = rect.center();
    let pt = |x, y| c + egui::vec2(x, y);
    let p = ui.painter();
    let stroke = Stroke::new(1.3, color);
    p.add(egui::Shape::closed_line(
        vec![
            pt(-5.0, -6.5),
            pt(1.5, -6.5),
            pt(5.5, -2.5),
            pt(5.5, 6.5),
            pt(-5.0, 6.5),
        ],
        stroke,
    ));
    p.add(egui::Shape::line(
        vec![pt(1.5, -6.5), pt(1.5, -2.5), pt(5.5, -2.5)],
        stroke,
    ));
}

fn nav_icon_btn(ui: &mut egui::Ui, enabled: bool, up: bool, tooltip: &str) -> bool {
    let pal = palette::p();
    let resp = ui
        .add_enabled(
            enabled,
            egui::Button::new("")
                .fill(pal.surface_2)
                .stroke(Stroke::new(1.0, pal.line))
                .corner_radius(6)
                .min_size(Vec2::splat(28.0)),
        )
        .on_hover_text(tooltip);
    let c = resp.rect.center();
    let col = if enabled { pal.ink } else { pal.line_strong };
    let stroke = Stroke::new(1.4, col);
    let p = ui.painter();
    if up {
        p.line_segment(
            [c + egui::vec2(0.0, 5.0), c + egui::vec2(0.0, -5.0)],
            stroke,
        );
        p.line_segment(
            [c + egui::vec2(-4.0, -1.0), c + egui::vec2(0.0, -5.0)],
            stroke,
        );
        p.line_segment(
            [c + egui::vec2(4.0, -1.0), c + egui::vec2(0.0, -5.0)],
            stroke,
        );
    } else {
        p.line_segment(
            [c + egui::vec2(5.0, 0.0), c + egui::vec2(-5.0, 0.0)],
            stroke,
        );
        p.line_segment(
            [c + egui::vec2(-1.0, -4.0), c + egui::vec2(-5.0, 0.0)],
            stroke,
        );
        p.line_segment(
            [c + egui::vec2(-1.0, 4.0), c + egui::vec2(-5.0, 0.0)],
            stroke,
        );
    }
    resp.clicked()
}

fn spawn_native_picker(ctx: egui::Context, start_dir: PathBuf) -> mpsc::Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut dir_arg = start_dir.to_string_lossy().to_string();
        if !dir_arg.ends_with('/') {
            dir_arg.push('/');
        }
        let out = std::process::Command::new("zenity")
            .args([
                "--file-selection",
                "--title=Прикрепить текстовый файл — KAIC Studio",
                &format!("--filename={dir_arg}"),
            ])
            .output();
        let picked = out.ok().filter(|o| o.status.success()).and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        });
        let _ = tx.send(picked);
        ctx.request_repaint();
    });
    rx
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            directory: home_dir(),
            back: vec![],
            filename: String::new(),
            filter: String::new(),
            error: None,
            cached_key: None,
            cached_rows: Vec::new(),
            native_rx: None,
        }
    }
}

impl Browser {
    fn navigate(&mut self, path: PathBuf) {
        match path.canonicalize() {
            Ok(p) if allowed(&p) && p.is_dir() => {
                if p != self.directory {
                    self.back.push(self.directory.clone());
                    self.directory = p;
                }
                self.filename.clear();
                self.error = None;
                self.cached_key = None;
            }
            _ => self.error = Some("Каталог недоступен или содержит защищённые данные".into()),
        }
    }

    fn ensure_cache(&mut self) {
        let key = (self.directory.clone(), self.filter.clone());
        if self.cached_key.as_ref() == Some(&key) {
            return;
        }
        self.cached_rows.clear();
        match entries(&self.directory, &self.filter) {
            Ok(items) => {
                for (path, is_dir) in items {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let size = if is_dir {
                        None
                    } else {
                        path.metadata().ok().map(|m| m.len())
                    };
                    self.cached_rows.push(CachedRow {
                        path,
                        name,
                        is_dir,
                        size,
                    });
                }
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("Не удалось прочитать каталог: {e}"));
            }
        }
        self.cached_key = Some(key);
    }

    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool, lang: Lang) -> Option<PathBuf> {
        let pal = palette::p();
        let mut selected = None;
        let mut cancel = false;

        // Проверяем ответ нативного системного диалога (если был запущен)
        if let Some(rx) = &self.native_rx {
            match rx.try_recv() {
                Ok(Some(path)) => {
                    self.native_rx = None;
                    if let Some(parent) = path.parent() {
                        self.directory = parent.to_path_buf();
                        self.cached_key = None;
                    }
                    return Some(path);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.native_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        self.ensure_cache();

        let modal_w = (ctx.content_rect().width() - 48.0).clamp(440.0, 740.0);
        let list_h = (ctx.content_rect().height() - 260.0).clamp(200.0, 360.0);

        egui::Modal::new(egui::Id::new("каик-выбор-файла"))
            .frame(
                egui::Frame::new()
                    .fill(pal.surface_1)
                    .stroke(Stroke::new(1.0, pal.line_strong))
                    .corner_radius(12)
                    .inner_margin(16),
            )
            .show(ctx, |ui| {
                ui.set_width(modal_w);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                // ── Шапка модалки ──────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(lang.t("Attach file", "Прикрепить файл"))
                            .strong()
                            .size(16.0)
                            .color(pal.ink),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("×").size(15.0).color(pal.ink_muted),
                                )
                                .frame(false)
                                .min_size(Vec2::splat(24.0)),
                            )
                            .on_hover_text(lang.t("Close · Esc", "Закрыть · Esc"))
                            .clicked()
                        {
                            cancel = true;
                        }
                        let waiting_native = self.native_rx.is_some();
                        if ui
                            .add_enabled(
                                !waiting_native,
                                egui::Button::new(
                                    RichText::new(if waiting_native {
                                        lang.t("System…", "Системный…")
                                    } else {
                                        lang.t("System dialog", "Системный диалог")
                                    })
                                    .size(12.0)
                                    .color(pal.ink_muted),
                                )
                                .corner_radius(6),
                            )
                            .on_hover_text(lang.t(
                                "Open native OS file picker",
                                "Открыть системный выбор файла",
                            ))
                            .clicked()
                        {
                            self.native_rx =
                                Some(spawn_native_picker(ctx.clone(), self.directory.clone()));
                        }
                    });
                });

                // ── Навигация: Назад, Вверх, интерактивные хлебные крошки, поиск ──
                let mut navigate: Option<PathBuf> = None;
                ui.horizontal(|ui| {
                    if nav_icon_btn(ui, !self.back.is_empty(), false, lang.t("Back", "Назад"))
                        && let Some(p) = self.back.pop()
                    {
                        self.directory = p;
                        self.filename.clear();
                        self.cached_key = None;
                    }
                    if nav_icon_btn(
                        ui,
                        self.directory.parent().is_some(),
                        true,
                        lang.t("Up", "Вверх"),
                    ) && let Some(p) = self.directory.parent()
                    {
                        navigate = Some(p.to_path_buf());
                    }

                    // Хлебные крошки пути
                    let crumb_w = (ui.available_width() - 188.0).max(120.0);
                    egui::Frame::new()
                        .fill(pal.surface_0)
                        .stroke(Stroke::new(1.0, pal.line))
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.set_width(crumb_w);
                            egui::ScrollArea::horizontal()
                                .id_salt("файл-крошки")
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        let mut acc = PathBuf::new();
                                        let comps: Vec<_> = self.directory.components().collect();
                                        for (idx, comp) in comps.iter().enumerate() {
                                            acc.push(comp);
                                            let target = acc.clone();
                                            let is_last = idx + 1 == comps.len();
                                            let raw = comp.as_os_str().to_string_lossy();
                                            let label = if raw == "/" { "/" } else { raw.as_ref() };
                                            if idx > 0 && label != "/" {
                                                ui.label(
                                                    RichText::new("›")
                                                        .size(12.0)
                                                        .color(pal.line_strong),
                                                );
                                            }
                                            let btn = egui::Button::new(
                                                RichText::new(label).monospace().size(12.0).color(
                                                    if is_last { pal.ink } else { pal.ink_muted },
                                                ),
                                            )
                                            .frame(false);
                                            if ui.add(btn).clicked() && !is_last {
                                                navigate = Some(target);
                                            }
                                        }
                                    });
                                });
                        });

                    // Компактный фильтр по имени
                    let prev_filter = self.filter.clone();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(lang.t("Filter files…", "Поиск файла…"))
                            .margin(egui::Margin::symmetric(8, 5))
                            .desired_width(172.0),
                    );
                    if self.filter != prev_filter {
                        self.cached_key = None;
                    }
                });

                // ── Двухколоночная область: места слева + список файлов справа ──
                let places = quick_places(lang);
                let show_places = modal_w >= 540.0;
                let side_w = if show_places { 148.0 } else { 0.0 };
                let main_w = if show_places {
                    (modal_w - side_w - 10.0).max(240.0)
                } else {
                    modal_w
                };

                ui.horizontal_top(|ui| {
                    if show_places {
                        ui.vertical(|ui| {
                            ui.set_width(side_w);
                            egui::Frame::new()
                                .fill(pal.surface_0)
                                .stroke(Stroke::new(1.0, pal.line))
                                .corner_radius(8)
                                .inner_margin(6)
                                .show(ui, |ui| {
                                    ui.set_width(side_w - 14.0);
                                    ui.set_min_height(list_h);
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(
                                        RichText::new(lang.t("Places", "Места"))
                                            .size(11.0)
                                            .color(pal.ink_muted),
                                    );
                                    ui.add_space(2.0);
                                    for (label, path) in &places {
                                        let active = self.directory == *path;
                                        let resp = egui::Frame::new()
                                            .fill(if active {
                                                pal.surface_2
                                            } else {
                                                Color32::TRANSPARENT
                                            })
                                            .corner_radius(6)
                                            .inner_margin(egui::Margin::symmetric(6, 5))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    paint_folder(
                                                        ui,
                                                        if active {
                                                            pal.brand_text
                                                        } else {
                                                            pal.ink_muted
                                                        },
                                                    );
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(*label).size(12.5).color(
                                                                if active {
                                                                    pal.ink
                                                                } else {
                                                                    pal.ink_muted
                                                                },
                                                            ),
                                                        )
                                                        .truncate(),
                                                    );
                                                });
                                            })
                                            .response
                                            .interact(egui::Sense::click());
                                        if resp.hovered() && !active {
                                            ui.painter().rect_filled(
                                                resp.rect,
                                                6,
                                                anim::with_alpha(pal.ink, 12),
                                            );
                                        }
                                        if resp.clicked() {
                                            navigate = Some(path.clone());
                                        }
                                    }
                                });
                        });
                    }

                    // Правая колонка: чистый список каталогов и файлов
                    ui.vertical(|ui| {
                        ui.set_width(main_w);
                        egui::Frame::new()
                            .fill(pal.surface_0)
                            .stroke(Stroke::new(1.0, pal.line))
                            .corner_radius(8)
                            .inner_margin(6)
                            .show(ui, |ui| {
                                ui.set_width((main_w - 14.0).max(200.0));
                                ui.set_min_height(list_h);
                                egui::ScrollArea::vertical()
                                    .id_salt("список-файлов-каик")
                                    .max_height(list_h)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        if self.cached_rows.is_empty() {
                                            ui.centered_and_justified(|ui| {
                                                ui.label(
                                                    RichText::new(if self.filter.is_empty() {
                                                        lang.t("Folder is empty", "Каталог пуст")
                                                    } else {
                                                        lang.t(
                                                            "No matching files",
                                                            "Файлы по фильтру не найдены",
                                                        )
                                                    })
                                                    .size(13.0)
                                                    .color(pal.ink_muted),
                                                );
                                            });
                                            return;
                                        }
                                        for row in &self.cached_rows {
                                            let is_sel = !row.is_dir && self.filename == row.name;
                                            let over_limit =
                                                row.size.is_some_and(|s| s > 128 * 1024);
                                            let resp = egui::Frame::new()
                                                .fill(if is_sel {
                                                    pal.surface_2
                                                } else {
                                                    Color32::TRANSPARENT
                                                })
                                                .stroke(Stroke::new(
                                                    1.0,
                                                    if is_sel {
                                                        pal.brand
                                                    } else {
                                                        Color32::TRANSPARENT
                                                    },
                                                ))
                                                .corner_radius(6)
                                                .inner_margin(egui::Margin::symmetric(8, 5))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        if row.is_dir {
                                                            paint_folder(ui, pal.brand_text);
                                                        } else {
                                                            paint_file(
                                                                ui,
                                                                if is_sel {
                                                                    pal.ink
                                                                } else {
                                                                    pal.ink_muted
                                                                },
                                                            );
                                                        }
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(&row.name)
                                                                    .size(13.0)
                                                                    .color(if over_limit {
                                                                        pal.ink_muted
                                                                    } else {
                                                                        pal.ink
                                                                    }),
                                                            )
                                                            .truncate(),
                                                        );
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                let meta = match row.size {
                                                                    Some(sz) => {
                                                                        lang.format_bytes(sz)
                                                                    }
                                                                    None => lang
                                                                        .t("dir", "папка")
                                                                        .to_string(),
                                                                };
                                                                ui.label(
                                                                    RichText::new(meta)
                                                                        .monospace()
                                                                        .size(11.0)
                                                                        .color(pal.ink_muted),
                                                                );
                                                            },
                                                        );
                                                    });
                                                })
                                                .response
                                                .interact(egui::Sense::click());
                                            if resp.hovered() && !is_sel {
                                                ui.painter().rect_filled(
                                                    resp.rect,
                                                    6,
                                                    anim::with_alpha(pal.ink, 12),
                                                );
                                            }
                                            if resp.double_clicked() && !row.is_dir {
                                                selected = Some(row.path.clone());
                                            } else if resp.clicked() {
                                                if row.is_dir {
                                                    navigate = Some(row.path.clone());
                                                } else {
                                                    self.filename = row.name.clone();
                                                }
                                            }
                                        }
                                    });
                            });
                    });
                });

                if let Some(p) = navigate {
                    self.navigate(p);
                }

                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).size(12.0).color(pal.err));
                }

                // ── Подвал: выбранный файл / лимит + Отмена / Прикрепить ──
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if !self.filename.is_empty() {
                            ui.label(
                                RichText::new(&self.filename)
                                    .monospace()
                                    .size(12.5)
                                    .color(pal.ink),
                            );
                        }
                        ui.label(
                            RichText::new(lang.t(
                                "Any text or image file · drag & drop supported",
                                "Любые текстовые файлы и фото · можно перетащить в окно",
                            ))
                            .size(11.5)
                            .color(pal.ink_muted),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = !self.filename.trim().is_empty();
                        let attach_lbl = lang.t("Attach", "Прикрепить");
                        let ok_btn = if ready {
                            egui::Button::new(
                                RichText::new(attach_lbl).color(pal.on_brand).strong(),
                            )
                            .fill(pal.brand)
                            .stroke(Stroke::NONE)
                            .corner_radius(7)
                        } else {
                            egui::Button::new(attach_lbl).corner_radius(7)
                        };
                        if ui.add_enabled(ready, ok_btn).clicked() {
                            selected = Some(self.directory.join(&self.filename));
                        }
                        if ui
                            .add(egui::Button::new(lang.t("Cancel", "Отмена")).corner_radius(7))
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });

                if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                    cancel = true;
                } else if !self.filename.trim().is_empty()
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                {
                    selected = Some(self.directory.join(&self.filename));
                }
            });

        if cancel {
            *open = false;
        }
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_browser_sorted_navigation_filter_and_validator() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        std::fs::create_dir(d.path().join("z-dir")).unwrap();
        std::fs::write(d.path().join("B.rs"), b"fn main() {}").unwrap();
        std::fs::write(d.path().join("a.txt"), b"text").unwrap();
        std::fs::write(d.path().join("bad.rs"), [0, 1]).unwrap();
        let list = entries(d.path(), "").unwrap();
        assert!(list[0].1);
        assert_eq!(list[1].0.file_name().unwrap(), "a.txt");
        assert_eq!(entries(d.path(), "B.RS").unwrap().len(), 2);
        let mut browser = Browser::default();
        browser.navigate(d.path().into());
        assert_eq!(browser.directory, d.path());
        assert_eq!(browser.back.len(), 1);
        assert!(crate::attachments::read_attachment(&d.path().join("B.rs"), 0).is_ok());
        assert!(crate::attachments::read_attachment(&d.path().join("bad.rs"), 0).is_err());
    }
}

#[cfg(test)]
mod parent_regressions {
    use super::*;
    #[test]
    fn parent_documents_not_a_runtime_agent_sandbox() {
        assert!(allowed(Path::new("/home/kirpich/Documents/project.rs")));
    }
    #[test]
    fn parent_browser_starts_at_user_home_not_build_directory() {
        if let Some(h) = std::env::var_os("HOME") {
            assert_eq!(Browser::default().directory, PathBuf::from(h));
        }
    }
}
