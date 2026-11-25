//! Структура интерфейса адаптирована из Ollama Desktop (MIT), Copyright (c) Ollama.
//! Источник и полный текст лицензии: THIRD_PARTY_NOTICES; rev 42ac2c0c1e132c0b4f0d9cfcd4e7950f0c5470bc.
use super::{Studio, anim, icons, markdown, palette};
use crate::{catalog, kaic::Secret, service::Job, storage::Message};
use eframe::egui::{self, Color32, RichText, Stroke};
impl Studio {
    pub(super) fn chat_ui(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        if self.state.chats[self.active].messages.is_empty() {
            let pal = palette::p();
            let active_model =
                catalog::resolve_selection(&self.models, &self.state.settings.selected_repo)
                    .cloned();
            let active_kind = active_model
                .as_ref()
                .map(|m| m.kind())
                .unwrap_or(catalog::ModelKind::Text);
            let active_profile = active_model
                .as_ref()
                .and_then(|m| catalog::live_profile(&m.repo).ok());
            let hero_title = active_model
                .as_ref()
                .map(|m| m.display_name())
                .unwrap_or_else(|| lang.t("Local LLM Workbench", "Стенд локальных LLM").into());
            let gen_opts = &self.state.chats[self.active].options.media_gen;
            let hero_meta = if let Some(m) = &active_model {
                match active_kind {
                    catalog::ModelKind::Image => format!(
                        "stable-diffusion.cpp · {} · {}×{} · {} {}",
                        lang.t("Image", "Фото"),
                        gen_opts.width,
                        gen_opts.height,
                        gen_opts.steps,
                        lang.t("steps", "шаг.")
                    ),
                    catalog::ModelKind::Video => format!(
                        "stable-diffusion.cpp · {} · {}×{} · {}f @ {}fps",
                        lang.t("Video", "Видео"),
                        gen_opts.width,
                        gen_opts.height,
                        gen_opts.video_frames,
                        gen_opts.fps
                    ),
                    catalog::ModelKind::Text => {
                        let mut parts = vec!["llama.cpp".to_string(), lang.format_gb(m.size)];
                        if let Some(tps) = m.generation_tps {
                            parts.push(format!("{tps:.1} {}", lang.t("tok/s", "t/c")));
                        }
                        if let Some(p) = &active_profile {
                            parts.push(format!("{}K ctx", p.context / 1024));
                        }
                        parts.join(" · ")
                    }
                }
            } else {
                lang.t(
                    "llama.cpp · local inference · offline",
                    "llama.cpp · локальный инференс · без облака",
                )
                .into()
            };

            let avail_h = ui.available_height();
            let avail_w = ui.available_width();
            // Рекомендуемый размер из kich-identity.md (128..192 px с адаптацией под компактное окно 540×620)
            let kich_px = if avail_h >= 720.0 && avail_w >= 760.0 {
                148.0
            } else if avail_h >= 600.0 {
                96.0
            } else {
                72.0
            };
            let ctx_clone = ui.ctx().clone();
            let kich_tex = self.ensure_kich_texture(&ctx_clone);

            ui.add_space(((avail_h - (kich_px + 236.0)) * 0.35).max(8.0));
            let mut prompt_click: Option<&'static str> = None;
            centered(ui, |ui| {
                ui.vertical_centered(|ui| {
                    if let Some(tid) = kich_tex {
                        let resp = ui.add(
                            egui::Image::new((tid, egui::vec2(kich_px, kich_px)))
                                .corner_radius(8)
                                .sense(egui::Sense::hover()),
                        );
                        resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Image,
                                true,
                                "Кич — маскот KAIC и Kirpich MIND, монитор с символом >_",
                            )
                        });
                    } else {
                        anim::animated_mark(ui, 56.0, self.busy);
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(hero_title)
                            .strong()
                            .size(20.0)
                            .color(pal.ink),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        RichText::new(hero_meta)
                            .monospace()
                            .size(12.0)
                            .color(pal.ink_muted),
                    );
                    ui.add_space(10.0);
                    let row_w = ui.available_width();
                    let est_w = if lang == super::i18n::Lang::En {
                        364.0
                    } else {
                        404.0
                    };
                    let pad = ((row_w - est_w) * 0.5).max(0.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                        if pad > 4.0 {
                            ui.add_space(pad);
                        }
                        let hints = match active_kind {
                            catalog::ModelKind::Image => [
                                (
                                    lang.t("Orbital rocket", "Ракета на орбите"),
                                    "Photorealistic stainless steel orbital rocket on launchpad at dusk, crisp volumetric exhaust steam, 35mm lens, sharp engineering details",
                                ),
                                (
                                    lang.t("PCB macro", "Макро платы"),
                                    "Macro studio photograph of a matte black aerospace PCB with gold traces, STM32 microcontroller, subtle warm rim lighting, shallow depth of field",
                                ),
                                (
                                    lang.t("Dark lab", "Инженерная лаборатория"),
                                    "Minimalist dark aerospace telemetry workstation with oscilloscopes and warm amber accents, architectural photography, high contrast",
                                ),
                                (
                                    lang.t("Isometric chip", "Изометрия чипа"),
                                    "Clean isometric technical render of an AI compute module on dark carbon desk, brushed aluminum heatsink, studio softbox light",
                                ),
                            ],
                            catalog::ModelKind::Video => [
                                (
                                    lang.t("Rocket ignition", "Запуск ракеты"),
                                    "Slow motion orbital rocket engine ignition on launchpad at twilight, glowing orange flame, billowing steam clouds, cinematic steady camera",
                                ),
                                (
                                    lang.t("Drone flyover", "Пролёт дрона"),
                                    "Smooth aerial drone flyover of a snow-dusted mountain ridge at golden hour, dramatic shadows, crisp natural lighting",
                                ),
                                (
                                    lang.t("Server rack", "Серверная стойка"),
                                    "Slow dolly shot along a dark local AI compute cluster rack, subtle amber status LEDs blinking, shallow depth of field",
                                ),
                                (
                                    lang.t("Orbital sunrise", "Рассвет на орбите"),
                                    "Cinematic view from low Earth orbit as sunlight creeps over the curved horizon, glowing thin blue atmosphere, calm slow pan",
                                ),
                            ],
                            catalog::ModelKind::Text => [
                                (
                                    lang.t("Rust code", "Код на Rust"),
                                    lang.t(
                                        "Write a thread-safe ring buffer in Rust without unsafe and explain operation complexity.",
                                        "Напиши потокобезопасный ring buffer на Rust без unsafe и объясни сложность операций.",
                                    ),
                                ),
                                (
                                    lang.t("Reasoning", "Рассуждение"),
                                    lang.t(
                                        "A GPU has 8 GB VRAM, a model takes 4.8 GB, and KV cache needs 0.4 MB per token. How much context fits with a 15% safety margin?",
                                        "У сервера 8 ГБ VRAM, модель весит 4.8 ГБ, KV-кэш требует 0.4 МБ на токен. Сколько контекста поместится с запасом 15%?",
                                    ),
                                ),
                                (
                                    lang.t("Strict JSON", "Строгий JSON"),
                                    lang.t(
                                        "Return only valid JSON with no extra text: an object with fields model, quant, fits_vram (bool), and notes (array of 2 strings).",
                                        "Верни только валидный JSON без пояснений: объект с полями model, quant, fits_vram (bool) и notes (массив из 2 строк).",
                                    ),
                                ),
                                (
                                    lang.t("Speed test", "Тест скорости"),
                                    lang.t(
                                        "List 10 key architectural features of llama.cpp and the GGUF format in a concise bullet list.",
                                        "Перечисли 10 ключевых архитектурных особенностей llama.cpp и формата GGUF в одном коротком списке.",
                                    ),
                                ),
                            ],
                        };
                        for (label, prompt) in hints {
                            let btn = egui::Button::new(
                                RichText::new(label).size(12.0).color(pal.ink_muted),
                            )
                            .fill(pal.surface_1)
                            .stroke(Stroke::new(1.0, pal.line))
                            .corner_radius(12);
                            if ui.add_enabled(!self.busy, btn).clicked() {
                                prompt_click = Some(prompt);
                            }
                        }
                    });
                });
                ui.add_space(20.0);
                self.composer_ui(ui);
            });
            if let Some(p) = prompt_click {
                self.composer = p.to_string();
                if let Err(e) = self.send_message() {
                    self.error = Some(format!("{e:#}"));
                }
            }
            return;
        }
        egui::Panel::bottom("композитор")
            .resizable(false)
            .default_size(112.0)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(palette::SURFACE_0)
                    .inner_margin(egui::Margin::symmetric(0, 12)),
            )
            .show(ui, |ui| centered(ui, |ui| self.composer_ui(ui)));
        let mut retry = false;
        let mut edit = None;
        let mut open_media_path: Option<std::path::PathBuf> = None;
        // Лениво подгружаем превью-текстуры для сохранённых сгенерированных фото/видео
        let media_paths: Vec<std::path::PathBuf> = self.state.chats[self.active]
            .messages
            .iter()
            .flat_map(|m| m.media.iter())
            .map(|gm| gm.preview_path.clone().unwrap_or_else(|| gm.path.clone()))
            .filter(|p| !self.media_textures.contains_key(p) && p.is_file())
            .collect();
        let ctx_clone = ui.ctx().clone();
        for p in media_paths {
            let _ = self.ensure_media_texture(&ctx_clone, &p);
        }
        egui::ScrollArea::vertical()
            .id_salt(("лента", self.state.chats[self.active].id))
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                centered(ui, |ui| {
                    ui.set_max_width(720.0_f32.min(ui.available_width()));
                    let chat = &self.state.chats[self.active];
                    let task = chat
                        .kaic_task
                        .clone()
                        .map(|id| (id, chat.id, chat.status.clone()));
                    for (index, m) in chat.messages.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            // Плавное появление нового сообщения (лёгкий slide-up 10px -> 0px)
                            let appear = if ui.input(|i| i.time) > 0.0 {
                                anim::ease_out_cubic(ui.ctx().animate_bool_with_time(
                                    ui.id().with((chat.id, index, "appear")),
                                    true,
                                    0.22,
                                ))
                            } else {
                                1.0
                            };
                            ui.add_space(18.0 + (1.0 - appear) * 10.0);
                            let is_live = self.busy
                                && self.pending_chat == Some(chat.id)
                                && index + 1 == chat.messages.len();
                            if m.role != "user"
                                && (!m.agent_events.is_empty() || !m.subtasks.is_empty())
                            {
                                render_agent_activity(ui, index, m, lang);
                                ui.add_space(8.0);
                            }
                            if !m.reasoning.is_empty() && m.agent_events.is_empty() {
                                egui::CollapsingHeader::new(
                                    lang.t("Reasoning", "Рассуждение модели"),
                                )
                                .id_salt((index, "мысли"))
                                .show(ui, |ui| markdown::render(ui, &m.reasoning));
                            }
                            if !m.content.is_empty() || (m.role == "user" && !m.media.is_empty()) {
                                if m.role == "user" {
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::RIGHT),
                                        |ui| {
                                            for gm in &m.media {
                                                let thumb = gm
                                                    .preview_path
                                                    .as_deref()
                                                    .unwrap_or(gm.path.as_path());
                                                egui::Frame::new()
                                                    .fill(palette::SURFACE_2)
                                                    .stroke(Stroke::new(1.0, palette::LINE))
                                                    .corner_radius(12)
                                                    .inner_margin(6)
                                                    .show(ui, |ui| {
                                                        if let Some(tex) =
                                                            self.media_textures.get(thumb)
                                                        {
                                                            let sz = tex.size_vec2();
                                                            let max_w =
                                                                320.0_f32.min(ui.available_width());
                                                            let scale = (max_w / sz.x.max(1.0))
                                                                .min(240.0 / sz.y.max(1.0))
                                                                .min(1.0);
                                                            let img_size = sz * scale;
                                                            let resp = ui.add(
                                                                egui::Image::new((
                                                                    tex.id(),
                                                                    img_size,
                                                                ))
                                                                .corner_radius(8)
                                                                .sense(egui::Sense::click()),
                                                            );
                                                            if resp.clicked() {
                                                                open_media_path =
                                                                    Some(gm.path.clone());
                                                            }
                                                        } else {
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "[{}×{}] {}",
                                                                    gm.width,
                                                                    gm.height,
                                                                    gm.path
                                                                        .file_name()
                                                                        .unwrap_or_default()
                                                                        .to_string_lossy()
                                                                ))
                                                                .monospace()
                                                                .size(11.5),
                                                            );
                                                        }
                                                    });
                                                ui.add_space(4.0);
                                            }
                                            if !m.content.is_empty() {
                                                egui::Frame::new()
                                                    .fill(palette::SURFACE_2)
                                                    .stroke(Stroke::new(1.0, palette::LINE))
                                                    .corner_radius(16)
                                                    .inner_margin(egui::Margin::symmetric(14, 8))
                                                    .show(ui, |ui| {
                                                        ui.set_max_width(
                                                            416.0_f32.min(ui.available_width()),
                                                        );
                                                        ui.add(
                                                            egui::Label::new(&m.content)
                                                                .selectable(true)
                                                                .wrap(),
                                                        );
                                                    });
                                            }
                                            ui.add_enabled_ui(
                                                !self.busy && chat.model != "KAIC",
                                                |ui| {
                                                    if icons::button(
                                                        ui,
                                                        icons::Icon::Edit,
                                                        lang.t(
                                                            "Edit in new chat",
                                                            "Изменить в новом разговоре",
                                                        ),
                                                    )
                                                    .clicked()
                                                    {
                                                        edit = Some(index);
                                                    }
                                                },
                                            );
                                        },
                                    );
                                } else {
                                    markdown::render(ui, &m.content);
                                    if is_live {
                                        anim::streaming_cursor(ui);
                                    }
                                    ui.add_space(4.0);
                                    ui.horizontal_wrapped(|ui| {
                                        icons::copy_button(
                                            ui,
                                            ui.id().with((chat.id, index, "copy")),
                                            &m.content,
                                            lang,
                                        );
                                        if index + 1 == chat.messages.len() && chat.model != "KAIC"
                                        {
                                            ui.add_enabled_ui(!self.busy, |ui| {
                                                if icons::button(
                                                    ui,
                                                    icons::Icon::Retry,
                                                    lang.t("Regenerate", "Повторить ответ"),
                                                )
                                                .clicked()
                                                {
                                                    retry = true;
                                                }
                                            });
                                        }
                                        if let Some(tps) = m.tps {
                                            ui.add_space(4.0);
                                            anim::tps_badge(
                                                ui,
                                                tps,
                                                m.tokens,
                                                m.duration_secs,
                                                is_live,
                                                lang,
                                            );
                                        }
                                    });
                                }
                            } else if !m.media.is_empty() {
                                let pal = palette::p();
                                for gm in &m.media {
                                    let thumb =
                                        gm.preview_path.as_deref().unwrap_or(gm.path.as_path());
                                    egui::Frame::new()
                                        .fill(pal.surface_1)
                                        .stroke(Stroke::new(1.0, pal.line))
                                        .corner_radius(10)
                                        .inner_margin(10)
                                        .show(ui, |ui| {
                                            if let Some(tex) = self.media_textures.get(thumb) {
                                                let sz = tex.size_vec2();
                                                let max_w = ui.available_width().min(540.0);
                                                let scale = (max_w / sz.x.max(1.0))
                                                    .min(420.0 / sz.y.max(1.0))
                                                    .min(1.0);
                                                let img_size = sz * scale;
                                                let resp = ui.add(
                                                    egui::Image::new((tex.id(), img_size))
                                                        .corner_radius(8)
                                                        .sense(egui::Sense::click()),
                                                );
                                                if gm.kind == "video" {
                                                    let badge = egui::Rect::from_min_size(
                                                        resp.rect.min + egui::vec2(10.0, 10.0),
                                                        egui::vec2(64.0, 24.0),
                                                    );
                                                    ui.painter().rect_filled(
                                                        badge,
                                                        6,
                                                        anim::with_alpha(pal.surface_0, 215),
                                                    );
                                                    ui.painter().rect_stroke(
                                                        badge,
                                                        6,
                                                        Stroke::new(1.0, pal.brand),
                                                        egui::StrokeKind::Inside,
                                                    );
                                                    ui.painter().text(
                                                        badge.center(),
                                                        egui::Align2::CENTER_CENTER,
                                                        "▶ MP4",
                                                        egui::FontId::monospace(11.5),
                                                        pal.brand_text,
                                                    );
                                                }
                                                if resp.clicked() {
                                                    open_media_path = Some(gm.path.clone());
                                                }
                                            }
                                            ui.add_space(6.0);
                                            ui.horizontal_wrapped(|ui| {
                                                let mut meta = vec![
                                                    if gm.kind == "video" { "MP4" } else { "PNG" }
                                                        .to_string(),
                                                    format!("{}×{}", gm.width, gm.height),
                                                ];
                                                if let Some(f) = gm.frames {
                                                    let fps = gm.fps.unwrap_or(16);
                                                    meta.push(format!("{f}f @ {fps}fps"));
                                                }
                                                if let Some(s) = gm.steps {
                                                    meta.push(format!(
                                                        "{s} {}",
                                                        lang.t("steps", "шаг.")
                                                    ));
                                                }
                                                if let Some(sec) = gm.elapsed_secs {
                                                    meta.push(format!(
                                                        "{sec:.1} {}",
                                                        lang.t("s", "с")
                                                    ));
                                                }
                                                ui.label(
                                                    RichText::new(meta.join(" · "))
                                                        .monospace()
                                                        .size(11.5)
                                                        .color(pal.ink_muted),
                                                );
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui
                                                            .small_button(lang.t(
                                                                if gm.kind == "video" {
                                                                    "Play"
                                                                } else {
                                                                    "Open"
                                                                },
                                                                if gm.kind == "video" {
                                                                    "Открыть видео"
                                                                } else {
                                                                    "Открыть"
                                                                },
                                                            ))
                                                            .clicked()
                                                        {
                                                            open_media_path = Some(gm.path.clone());
                                                        }
                                                        if ui
                                                            .small_button(
                                                                lang.t(
                                                                    "Copy path",
                                                                    "Копировать путь",
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            ui.ctx().copy_text(
                                                                gm.path
                                                                    .to_string_lossy()
                                                                    .into_owned(),
                                                            );
                                                        }
                                                        if index + 1 == chat.messages.len() {
                                                            ui.add_enabled_ui(!self.busy, |ui| {
                                                                if icons::button(
                                                                    ui,
                                                                    icons::Icon::Retry,
                                                                    lang.t(
                                                                        "Regenerate",
                                                                        "Сгенерировать снова",
                                                                    ),
                                                                )
                                                                .clicked()
                                                                {
                                                                    retry = true;
                                                                }
                                                            });
                                                        }
                                                    },
                                                );
                                            });
                                        });
                                }
                            } else if is_live {
                                anim::thinking_wave(ui, &self.status);
                            }
                            if let Some(error) = &m.error {
                                ui.add_space(8.0);
                                egui::Frame::new()
                                    .fill(palette::SURFACE_1)
                                    .stroke(Stroke::new(1.0, palette::ERR))
                                    .inner_margin(12)
                                    .corner_radius(8)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(if m.state == "остановлен" {
                                                lang.t("Response stopped", "Ответ остановлен")
                                            } else {
                                                lang.t("Response failed", "Ответ не завершён")
                                            })
                                            .strong(),
                                        );
                                        ui.add(egui::Label::new(error).selectable(true).wrap());
                                        if index + 1 == chat.messages.len() && chat.model != "KAIC"
                                        {
                                            ui.add_enabled_ui(!self.busy, |ui| {
                                                if icons::button(
                                                    ui,
                                                    icons::Icon::Retry,
                                                    lang.t("Retry", "Повторить ответ"),
                                                )
                                                .clicked()
                                                {
                                                    retry = true;
                                                }
                                            });
                                        }
                                    });
                            }
                            ui.add_space(12.0);
                        });
                    }
                    if let Some((id, chat, status)) = task {
                        ui.add_space(12.0);
                        egui::CollapsingHeader::new("Задача KAIC").show(ui, |ui| {
                            ui.label(&id);
                            ui.label(status);
                            if ui
                                .add_enabled(!self.busy, egui::Button::new("Обновить задачу"))
                                .clicked()
                            {
                                self.start(Job::Kaic {
                                    chat,
                                    url: self.state.settings.kaic_url.clone(),
                                    token: Secret::new(self.token.clone()),
                                    text: String::new(),
                                    resume: Some(id),
                                });
                            }
                        });
                    }
                })
            });
        if let Some(p) = open_media_path {
            let _ = std::process::Command::new("xdg-open").arg(p).spawn();
        }
        if let Some(index) = edit {
            self.branch_message(index);
        }
        if retry && let Err(e) = self.retry_response() {
            self.error = Some(format!("{e:#}"));
        }
    }
    fn composer_ui(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        let pal = palette::p();
        let mut send = false;
        let ctx_clone = ui.ctx().clone();

        // Перехват Ctrl+V (в т.ч. на русской раскладке Ctrl+М и в Wayland, где egui-winit
        // глотает pressed=true при отсутствии text/plain и отдаёт Event::Key(V, pressed=false))
        #[cfg(not(test))]
        if !self.busy {
            let now = ui.input(|i| i.time);
            let ctrl_down = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
            let ctrl_id = egui::Id::new("kaic-last-ctrl-ts");
            let text_paste_id = egui::Id::new("kaic-last-text-paste-ts");
            let img_paste_id = egui::Id::new("kaic-last-img-paste-ts");
            if ctrl_down {
                ui.ctx().data_mut(|d| d.insert_temp(ctrl_id, now));
            }
            let last_ctrl = ui
                .ctx()
                .data_mut(|d| d.get_temp::<f64>(ctrl_id))
                .unwrap_or(-10.0);
            let last_img_paste = ui
                .ctx()
                .data_mut(|d| d.get_temp::<f64>(img_paste_id))
                .unwrap_or(-10.0);

            let has_paste_event =
                ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            if has_paste_event {
                if let Ok(Some(att)) = crate::attachments::try_paste_clipboard_image(
                    &self.data.join("attachments"),
                    true,
                ) {
                    if let Some(p) = &att.image_path {
                        let _ = self.ensure_media_texture(&ctx_clone, p);
                    }
                    self.attachments.push(att);
                    self.error = None;
                    ui.ctx().data_mut(|d| d.insert_temp(img_paste_id, now));
                    ui.input_mut(|i| {
                        i.events.retain(|e| !matches!(e, egui::Event::Paste(_)));
                    });
                } else {
                    ui.ctx().data_mut(|d| d.insert_temp(text_paste_id, now));
                }
            } else {
                let last_text_paste = ui
                    .ctx()
                    .data_mut(|d| d.get_temp::<f64>(text_paste_id))
                    .unwrap_or(-10.0);
                let v_key_paste = ui.input(|i| {
                    i.events.iter().any(|e| match e {
                        egui::Event::Key {
                            key,
                            physical_key,
                            pressed,
                            modifiers,
                            ..
                        } => {
                            let is_v = *key == egui::Key::V
                                || *physical_key == Some(egui::Key::V)
                                || *key == egui::Key::Paste;
                            let is_shift_ins =
                                *key == egui::Key::Insert && (modifiers.shift || i.modifiers.shift);
                            let ctrl_active = modifiers.ctrl
                                || modifiers.command
                                || i.modifiers.ctrl
                                || i.modifiers.command
                                || (now - last_ctrl) < 0.65;
                            (is_v && ctrl_active && !*pressed)
                                || (is_v && ctrl_active && *pressed)
                                || (is_shift_ins && !*pressed)
                        }
                        _ => false,
                    })
                });
                if v_key_paste
                    && (now - last_text_paste) > 0.35
                    && (now - last_img_paste) > 0.25
                    && let Ok(Some(att)) = crate::attachments::try_paste_clipboard_image(
                        &self.data.join("attachments"),
                        false,
                    )
                {
                    if let Some(p) = &att.image_path {
                        let _ = self.ensure_media_texture(&ctx_clone, p);
                    }
                    self.attachments.push(att);
                    self.error = None;
                    ui.ctx().data_mut(|d| d.insert_temp(img_paste_id, now));
                }
            }
        }

        // Подгружаем превью-текстуры для вложенных изображений в композиторе
        let pending_thumbs: Vec<std::path::PathBuf> = self
            .attachments
            .iter()
            .filter_map(|a| a.image_path.clone())
            .filter(|p| !self.media_textures.contains_key(p) && p.is_file())
            .collect();
        for p in pending_thumbs {
            let _ = self.ensure_media_texture(&ctx_clone, &p);
        }

        let active_chat_id = self.state.chats[self.active].id;
        if let Some((cid, req)) = self.pending_approval.clone()
            && cid == active_chat_id
        {
            egui::Frame::new()
                .fill(pal.surface_2)
                .stroke(Stroke::new(1.5, pal.warn))
                .corner_radius(10)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(
                                lang.t("Approval required", "Требуется подтверждение действия"),
                            )
                            .strong()
                            .size(13.0)
                            .color(pal.warn),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let short_id: String = req.request_id.chars().take(8).collect();
                            ui.label(
                                RichText::new(format!("id:{short_id}"))
                                    .monospace()
                                    .size(10.5)
                                    .color(pal.ink_muted),
                            )
                            .on_hover_text(&req.request_id);
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("{} · {}", req.tool, req.summary))
                            .monospace()
                            .size(12.0)
                            .color(pal.ink),
                    );
                    let iso_col =
                        if req.isolation == crate::agent::IsolationLevel::NoneUserPrivileges {
                            pal.err
                        } else {
                            pal.ink_muted
                        };
                    ui.label(
                        RichText::new(format!(
                            "{}: {}",
                            lang.t("Isolation", "Изоляция"),
                            req.isolation.label_ru()
                        ))
                        .monospace()
                        .size(10.5)
                        .color(iso_col),
                    );
                    ui.label(
                        RichText::new(&req.risk_note)
                            .size(11.5)
                            .color(pal.ink_muted),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let approve_btn = egui::Button::new(
                            RichText::new(lang.t("Approve", "Подтвердить"))
                                .strong()
                                .color(pal.on_brand),
                        )
                        .fill(pal.brand)
                        .stroke(Stroke::NONE)
                        .corner_radius(6);
                        if ui.add(approve_btn).clicked() {
                            self.worker.respond_approval(
                                req.request_id.clone(),
                                req.action_digest.clone(),
                                true,
                            );
                            self.pending_approval = None;
                        }
                        let deny_btn = egui::Button::new(
                            RichText::new(lang.t("Deny", "Отклонить")).color(pal.err),
                        )
                        .fill(pal.surface_2)
                        .stroke(Stroke::new(1.0, pal.err))
                        .corner_radius(6);
                        if ui.add(deny_btn).clicked() {
                            self.worker.respond_approval(
                                req.request_id.clone(),
                                req.action_digest.clone(),
                                false,
                            );
                            self.pending_approval = None;
                        }
                    });
                });
            ui.add_space(8.0);
        }

        let focused = ui
            .ctx()
            .memory(|m| m.has_focus(egui::Id::new("каик-композитор")));
        let focus_t = if ui.input(|i| i.time) > 0.0 {
            ui.ctx()
                .animate_bool_with_time(egui::Id::new("kaic-composer-focus"), focused, 0.16)
        } else {
            0.0
        };
        let border_color = anim::lerp_color(palette::LINE, palette::LINE_STRONG, focus_t);
        egui::Frame::new()
            .fill(palette::SURFACE_1)
            .stroke(Stroke::new(1.0, border_color))
            .corner_radius(14)
            .inner_margin(egui::Margin {
                left: 16,
                right: 14,
                top: 12,
                bottom: 8,
            })
            .show(ui, |ui| {
                let mut remove = None;
                if !self.attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (i, a) in self.attachments.iter().enumerate() {
                            if let Some(img_path) = &a.image_path {
                                egui::Frame::new()
                                    .fill(pal.surface_2)
                                    .stroke(Stroke::new(1.0, pal.brand))
                                    .corner_radius(8)
                                    .inner_margin(egui::Margin::symmetric(8, 6))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            if let Some(tex) = self.media_textures.get(img_path) {
                                                let sz = tex.size_vec2();
                                                let scale = (44.0 / sz.x.max(1.0))
                                                    .min(44.0 / sz.y.max(1.0))
                                                    .min(1.0);
                                                ui.add(
                                                    egui::Image::new((tex.id(), sz * scale))
                                                        .corner_radius(5),
                                                );
                                            }
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(&a.name)
                                                        .strong()
                                                        .size(12.0)
                                                        .color(pal.ink),
                                                );
                                                let dims = match (a.width, a.height) {
                                                    (Some(w), Some(h)) => format!(
                                                        "{w}×{h} · {}",
                                                        lang.format_bytes(a.bytes as u64)
                                                    ),
                                                    _ => lang.format_bytes(a.bytes as u64),
                                                };
                                                ui.label(
                                                    RichText::new(dims)
                                                        .monospace()
                                                        .size(10.5)
                                                        .color(pal.ink_muted),
                                                );
                                            });
                                            if ui
                                                .small_button(RichText::new("×").color(pal.err))
                                                .on_hover_text(
                                                    lang.t("Remove image", "Убрать изображение"),
                                                )
                                                .clicked()
                                            {
                                                remove = Some(i);
                                            }
                                        });
                                    });
                            } else if ui
                                .button(format!(
                                    "{} · {}  ×",
                                    a.name,
                                    lang.format_bytes(a.bytes as u64)
                                ))
                                .on_hover_text(lang.t("Remove attachment", "Убрать вложение"))
                                .clicked()
                            {
                                remove = Some(i);
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
                if let Some(i) = remove {
                    self.attachments.remove(i);
                }
                let active_kind =
                    catalog::resolve_selection(&self.models, &self.state.settings.selected_repo)
                        .map(|m| m.kind())
                        .unwrap_or(catalog::ModelKind::Text);
                let hint = match active_kind {
                    catalog::ModelKind::Image => {
                        lang.t("Describe image to generate…", "Опишите фото для генерации…")
                    }
                    catalog::ModelKind::Video => lang.t(
                        "Describe video to generate…",
                        "Опишите видео для генерации…",
                    ),
                    catalog::ModelKind::Text => lang.t(
                        "Ask the model or press Ctrl+V to paste an image…",
                        "Запрос к модели (Ctrl+V — вставить фото из буфера)…",
                    ),
                };
                let editor = ui
                    .add_enabled(
                        !self.busy,
                        egui::TextEdit::multiline(&mut self.composer)
                            .id(egui::Id::new("каик-композитор"))
                            .hint_text(hint)
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .frame(egui::Frame::NONE)
                            .return_key(egui::KeyboardShortcut::new(
                                egui::Modifiers::SHIFT,
                                egui::Key::Enter,
                            )),
                    )
                    .on_hover_text(lang.t(
                        "Enter to send · Shift+Enter for newline · Ctrl+V to paste image",
                        "Enter — отправить · Shift+Enter — новая строка · Ctrl+V — вставить фото",
                    ));
                send = editor.has_focus()
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    // Кнопка «+» (выбор файлов и фото) строго в левом нижнем углу поля ввода
                    ui.add_enabled_ui(!self.busy, |ui| {
                        if icons::button(
                            ui,
                            icons::Icon::Attach,
                            lang.t(
                                "Attach file or image (.png, .jpg, .txt, .rs…)",
                                "Прикрепить файл или фото (.png, .jpg, .txt, .rs…)",
                            ),
                        )
                        .clicked()
                        {
                            self.attach_open = true;
                        }
                    });
                    if ui.available_width() > 440.0 {
                        ui.add_space(4.0);
                        anim::keycap(ui, "Enter");
                        ui.label(
                            RichText::new(lang.t("send", "отправить"))
                                .size(11.0)
                                .color(palette::p().ink_muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.busy {
                            ui.add_enabled_ui(!self.worker.cancel.is_cancelled(), |ui| {
                                if icons::button(
                                    ui,
                                    icons::Icon::Stop,
                                    lang.t("Stop response", "Остановить ответ"),
                                )
                                .clicked()
                                {
                                    self.worker.cancel.cancel();
                                    self.status = lang
                                        .t("Stopping request…", "Остановка собственного запроса…")
                                        .into();
                                }
                            });
                        } else {
                            ui.add_enabled_ui(
                                !self.composer.trim().is_empty() || !self.attachments.is_empty(),
                                |ui| {
                                    if icons::button(
                                        ui,
                                        icons::Icon::Send,
                                        lang.t("Send message", "Отправить сообщение"),
                                    )
                                    .clicked()
                                    {
                                        send = true;
                                    }
                                },
                            );
                        }
                        if self.state.settings.use_kaic {
                            if ui
                                .button("KAIC")
                                .on_hover_text(
                                    lang.t("Local API settings", "Настройки локального API"),
                                )
                                .clicked()
                            {
                                self.settings_open = true;
                            }
                        } else {
                            self.composer_model_picker(ui);
                        }
                    });
                });
            });
        if self.state.chats[self.active].options.agent_enabled && !self.state.settings.use_kaic {
            let opts = &self.state.chats[self.active].options;
            let origin = match opts.provider.execution_origin() {
                crate::agent::ExecutionOrigin::KaicNativeAgent => "native",
                crate::agent::ExecutionOrigin::ExternallyManagedCli => "CLI",
            };
            let prov = opts.provider.label_en();
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{}: [{}] {} · files:{} · exec:{} · dyn:{} · {}",
                        lang.t("Agent", "Агент"),
                        origin,
                        prov,
                        opts.agent_policy.label_en(),
                        opts.exec_policy.label_en(),
                        if opts.dynamic_interaction_enabled {
                            "on"
                        } else {
                            "off"
                        },
                        if opts.workspace_path.trim().is_empty() {
                            lang.t("no workspace", "workspace не задан")
                        } else {
                            opts.workspace_path.trim()
                        }
                    ))
                    .monospace()
                    .size(10.5)
                    .color(pal.ink_muted),
                );
            });
        }
        if self.busy {
            ui.add_space(4.0);
            ui.label(
                RichText::new(&self.status)
                    .small()
                    .color(palette::INK_MUTED),
            );
        }
        if send
            && !self.busy
            && let Err(e) = self.send_message()
        {
            self.error = Some(format!("{e:#}"));
        }
    }
    fn composer_model_picker(&mut self, ui: &mut egui::Ui) {
        let pal = palette::p();
        let lang = self.lang();
        let selected_model =
            catalog::resolve_selection(&self.models, &self.state.settings.selected_repo);
        let selected = selected_model
            .map(|m| m.alias().to_owned())
            .unwrap_or_else(|| lang.t("Select model", "Выберите модель").into());
        let btn_w = 236.0_f32.min(ui.available_width());
        let button = ui
            .add_enabled_ui(!self.busy, |ui| {
                ui.add_sized([btn_w, 32.0], egui::Button::new("").corner_radius(8))
            })
            .inner;
        button.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, !self.busy, &selected)
        });
        // Рисуем индикатор типа/статуса и усечённый текст со строгим правым отступом до шеврона
        let dot_col = match selected_model.map(|m| m.kind()) {
            Some(catalog::ModelKind::Image | catalog::ModelKind::Video) => pal.brand,
            Some(catalog::ModelKind::Text) => pal.ceramic_200,
            None => pal.line_strong,
        };
        ui.painter().circle_filled(
            egui::pos2(button.rect.left() + 11.0, button.rect.center().y),
            3.2,
            dot_col,
        );
        let text_max_w = (button.rect.width() - 44.0).max(40.0);
        let font = egui::FontId::proportional(13.0);
        let mut job = egui::text::LayoutJob::simple_singleline(selected.clone(), font, pal.ink);
        job.wrap.max_width = text_max_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let text_pos = egui::pos2(
            button.rect.left() + 20.0,
            button.rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(text_pos, galley, pal.ink);
        let c = egui::pos2(button.rect.right() - 13.0, button.rect.center().y);
        ui.painter().add(egui::Shape::line(
            vec![
                c + egui::vec2(-3.0, -1.5),
                c + egui::vec2(0.0, 1.5),
                c + egui::vec2(3.0, -1.5),
            ],
            Stroke::new(1.2, pal.ink_muted),
        ));
        if button.clicked() {
            self.model_picker_open = !self.model_picker_open;
            self.model_cursor = self
                .models
                .iter()
                .position(|m| {
                    m.selection_id() == self.state.settings.selected_repo
                        || m.repo == self.state.settings.selected_repo
                })
                .unwrap_or(0);
        }
        let mut choose = None;
        let mut open = self.model_picker_open;
        egui::Popup::from_response(&button)
            .open_bool(&mut open)
            .align(egui::RectAlign::TOP_END)
            .width(360.0)
            .frame(
                egui::Frame::popup(ui.style())
                    .fill(pal.surface_1)
                    .stroke(Stroke::new(1.0, pal.line_strong))
                    .corner_radius(12)
                    .inner_margin(10),
            )
            .show(|ui| {
                choose = self.render_model_picker_contents(ui, button.clicked());
            });
        self.model_picker_open = open;
        if let Some(repo) = choose {
            self.select_model(&repo);
        }
        if !self.model_picker_open {
            self.model_search.clear();
        }
    }

    pub(super) fn render_model_picker_contents(
        &mut self,
        ui: &mut egui::Ui,
        just_opened: bool,
    ) -> Option<String> {
        let pal = palette::p();
        let lang = self.lang();
        let mut choose = None;
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);

        let search = ui.add(
            egui::TextEdit::singleline(&mut self.model_search)
                .hint_text(lang.t(
                    "Search model, quant (Q4_K_M), type…",
                    "Поиск модели, квантизации (Q4_K_M), типа…",
                ))
                .margin(egui::Margin::symmetric(10, 6))
                .desired_width(f32::INFINITY),
        );
        if just_opened {
            search.request_focus();
        }
        if search.changed() {
            self.model_cursor = 0;
        }

        // Быстрые чипы фильтра по типу модели
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for (kind, lbl) in [
                (super::models::LocalKindFilter::All, lang.t("All", "Все")),
                (super::models::LocalKindFilter::Text, "LLM"),
                (
                    super::models::LocalKindFilter::Image,
                    lang.t("Image", "Фото"),
                ),
                (
                    super::models::LocalKindFilter::Video,
                    lang.t("Video", "Видео"),
                ),
            ] {
                let act = self.hf.local_kind == kind;
                let btn = egui::Button::new(RichText::new(lbl).size(11.5).color(if act {
                    pal.ink
                } else {
                    pal.ink_muted
                }))
                .fill(if act {
                    pal.surface_2
                } else {
                    Color32::TRANSPARENT
                })
                .stroke(Stroke::new(1.0, if act { pal.brand } else { pal.line }))
                .corner_radius(10);
                if ui.add(btn).clicked() {
                    self.hf.local_kind = kind;
                    self.model_cursor = 0;
                }
            }
        });

        let q = self.model_search.trim().to_lowercase();
        let kind_filter = self.hf.local_kind;
        let filtered_indices: Vec<usize> = (0..self.models.len())
            .filter(|&i| {
                let m = &self.models[i];
                let kind_ok = match kind_filter {
                    super::models::LocalKindFilter::All => true,
                    super::models::LocalKindFilter::Text => m.kind() == catalog::ModelKind::Text,
                    super::models::LocalKindFilter::Image => m.kind() == catalog::ModelKind::Image,
                    super::models::LocalKindFilter::Video => m.kind() == catalog::ModelKind::Video,
                };
                let text_ok = q.is_empty()
                    || format!("{} {} {}", m.display_name(), m.repo, m.filename)
                        .to_lowercase()
                        .contains(&q);
                kind_ok && text_ok
            })
            .collect();

        let mut keyboard_move = false;
        if !filtered_indices.is_empty() {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.model_cursor = (self.model_cursor + 1) % filtered_indices.len();
                keyboard_move = true;
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.model_cursor =
                    (self.model_cursor + filtered_indices.len() - 1) % filtered_indices.len();
                keyboard_move = true;
            }
            self.model_cursor = self.model_cursor.min(filtered_indices.len() - 1);
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                let idx = filtered_indices[self.model_cursor];
                choose = Some(self.models[idx].selection_id());
                ui.close();
            }
        }

        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("popup-models-scroll")
            .max_height(300.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if filtered_indices.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(lang.t("No models found", "Модели не найдены"))
                            .color(pal.ink_muted),
                    );
                    ui.add_space(8.0);
                    return;
                }
                for (pos, &idx) in filtered_indices.iter().enumerate() {
                    let m = &self.models[idx];
                    let available = m.loadable();
                    let is_active = self.state.settings.selected_repo == m.selection_id()
                        || self.state.settings.selected_repo == m.repo;
                    let is_cursor = pos == self.model_cursor;
                    let quant = super::models::parse_quant(&m.filename);
                    let (fit_text, fit_col, _, _, _) = super::models::fit_hint(m.size, lang);
                    let (kind_lbl, kind_col) = match m.kind() {
                        catalog::ModelKind::Image => ("IMAGE", pal.brand_text),
                        catalog::ModelKind::Video => ("VIDEO", pal.brand_text),
                        catalog::ModelKind::Text => ("LLM", pal.ink_muted),
                    };

                    let card_fill = if is_active || is_cursor {
                        pal.surface_2
                    } else {
                        pal.surface_0
                    };
                    let card_stroke = if is_active {
                        pal.brand
                    } else if is_cursor {
                        pal.line_strong
                    } else {
                        pal.line
                    };

                    let resp = egui::Frame::new()
                        .fill(card_fill)
                        .stroke(Stroke::new(1.0, card_stroke))
                        .corner_radius(7)
                        .inner_margin(egui::Margin {
                            left: 10,
                            right: 8,
                            top: 7,
                            bottom: 7,
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(m.display_name())
                                            .strong()
                                            .size(13.0)
                                            .color(if available { pal.ink } else { pal.ink_muted }),
                                    )
                                    .truncate(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        super::models::draw_badge(
                                            ui,
                                            &quant,
                                            pal.ink,
                                            pal.surface_1,
                                            pal.line,
                                        );
                                        ui.label(
                                            RichText::new(kind_lbl)
                                                .monospace()
                                                .size(10.0)
                                                .color(kind_col),
                                        );
                                        if is_active {
                                            icons::dot(ui, pal.brand);
                                        }
                                    },
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(lang.format_gb(m.size))
                                        .monospace()
                                        .size(11.0)
                                        .color(pal.ink_muted),
                                );
                                ui.label(
                                    RichText::new(format!("· {fit_text}"))
                                        .size(11.0)
                                        .color(fit_col),
                                );
                                if let Some(tps) = m.generation_tps {
                                    ui.label(
                                        RichText::new(format!(
                                            "· {tps:.1} {}",
                                            lang.t("tok/s", "t/c")
                                        ))
                                        .monospace()
                                        .size(11.0)
                                        .color(pal.ceramic_200),
                                    );
                                }
                            });
                        })
                        .response
                        .interact(egui::Sense::click());

                    if is_active {
                        let bar = egui::Rect::from_min_max(
                            egui::pos2(resp.rect.left() + 1.0, resp.rect.top() + 6.0),
                            egui::pos2(resp.rect.left() + 3.5, resp.rect.bottom() - 6.0),
                        );
                        ui.painter().rect_filled(bar, 2, pal.brand);
                    }
                    if (keyboard_move || just_opened) && is_cursor {
                        resp.scroll_to_me(Some(egui::Align::Center));
                    }
                    if resp.hovered() {
                        self.model_cursor = pos;
                    }
                    if resp.clicked() && available && !self.busy {
                        choose = Some(m.selection_id());
                        ui.close();
                    }
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .button(format!(
                    "{} ({})",
                    lang.t("My Models", "Мои модели"),
                    self.models.len()
                ))
                .clicked()
            {
                self.tab = super::Tab::Models;
                self.hf.tab = super::models::ModelsTab::Local;
                self.settings_open = false;
                ui.close();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dl_btn = egui::Button::new(
                    RichText::new(lang.t("+ Download Models (HF)", "+ Скачать модели (HF)"))
                        .color(pal.on_brand)
                        .strong(),
                )
                .fill(pal.brand)
                .stroke(Stroke::NONE);
                if ui.add(dl_btn).clicked() {
                    self.open_discover_tab();
                    ui.close();
                }
            });
        });
        choose
    }

    fn send_message(&mut self) -> anyhow::Result<()> {
        if self.composer.trim().is_empty() && self.attachments.is_empty() {
            return Ok(());
        }
        let mut content = self.composer.trim().to_string();
        let mut user_media = Vec::new();
        for a in &self.attachments {
            content.push_str(&a.for_prompt());
            if let Some(img_path) = &a.image_path {
                user_media.push(crate::storage::GeneratedMedia {
                    kind: "image".into(),
                    path: img_path.clone(),
                    preview_path: Some(img_path.clone()),
                    width: a.width.unwrap_or(0),
                    height: a.height.unwrap_or(0),
                    frames: None,
                    fps: None,
                    seed: None,
                    steps: None,
                    elapsed_secs: None,
                });
            }
        }
        let mut messages = self.state.chats[self.active].messages.clone();
        messages.retain(|m| {
            m.state != "предыдущая попытка"
                && (!m.content.is_empty() || !m.media.is_empty())
                && matches!(m.role.as_str(), "user" | "assistant" | "system")
        });
        let mut outgoing_user = Message::user(content.clone());
        outgoing_user.media = user_media.clone();
        messages.push(outgoing_user);
        let id = self.state.chats[self.active].id;
        let previous_chat = self.state.chats[self.active].clone();
        let previous_dirty = self.dirty;
        let job = if self.state.settings.use_kaic {
            if self.token.trim().is_empty() {
                anyhow::bail!("Введите токен KAIC в настройках или задайте KAIC_CONTROL_TOKEN");
            }
            crate::network::LocalEndpoint::parse(&self.state.settings.kaic_url)?;
            let text = if messages.len() == 1 {
                content.clone()
            } else {
                messages
                    .iter()
                    .map(|m| {
                        format!(
                            "{}:\n{}",
                            if m.role == "user" {
                                "Пользователь"
                            } else {
                                "Ассистент"
                            },
                            m.content
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            self.state.chats[self.active].kaic_task = None;
            self.state.chats[self.active].kaic_base = self.state.chats[self.active].messages.len();
            Job::Kaic {
                chat: id,
                url: self.state.settings.kaic_url.clone(),
                token: Secret::new(self.token.clone()),
                text,
                resume: None,
            }
        } else {
            self.direct_job(id, messages)?
        };
        let chat = &mut self.state.chats[self.active];
        if chat.messages.is_empty() {
            chat.title = self.composer.chars().take(44).collect();
            if chat.title.trim().is_empty() {
                chat.title = "Анализ файла".into();
            }
        }
        chat.model = if self.state.settings.use_kaic {
            "KAIC".into()
        } else {
            self.state.settings.selected_repo.clone()
        };
        chat.updated_at = crate::storage::epoch_now();
        let mut stored_user = Message::user(content);
        stored_user.media = user_media;
        chat.messages.push(stored_user);
        chat.updated_at = crate::storage::epoch_now();
        chat.messages.push(Message::assistant());
        chat.status = "Выполняется".into();
        self.mark_dirty();
        if let Err(error) = self.persist() {
            self.state.chats[self.active] = previous_chat;
            self.dirty = previous_dirty;
            return Err(error);
        }
        self.composer.clear();
        self.attachments.clear();
        self.start(job);
        Ok(())
    }
    fn direct_job(&self, id: uuid::Uuid, messages: Vec<Message>) -> anyhow::Result<Job> {
        let chat_options = &self.state.chats[self.active].options;
        let last_prompt = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.trim().to_string())
            .unwrap_or_default();

        if chat_options.agent_enabled {
            let ws_raw = chat_options.workspace_path.trim();
            if ws_raw.is_empty() {
                anyhow::bail!(
                    "В режиме агента укажите абсолютный путь к рабочему каталогу (workspace) в правой панели «Инспектор → Агент»"
                );
            }
            let workspace = std::path::PathBuf::from(ws_raw);
            crate::agent::WorkspaceGuard::new(&workspace)?;
            if last_prompt.is_empty() {
                anyhow::bail!("Введите задачу для агента");
            }
            let build_prior = |mut prior: Vec<Message>| {
                // Последнее пользовательское сообщение передаётся как task; предыдущие остаются в prior_messages.
                if prior.last().is_some_and(|m| m.role == "user") {
                    prior.pop();
                }
                let mut sys = chat_options.system_prompt.trim().to_owned();
                if !chat_options.skills.trim().is_empty() {
                    sys.push_str("\n\nSKILLS:\n");
                    sys.push_str(chat_options.skills.trim());
                }
                if !sys.is_empty() {
                    let mut sm = Message::user(sys);
                    sm.role = "system".into();
                    prior.insert(0, sm);
                }
                prior
            };
            let requested_ctx = if chat_options.context > 0 {
                chat_options.context
            } else {
                self.state.settings.context_override
            };
            let limits = crate::agent::AgentLimits {
                dynamic_interaction_enabled: chat_options.dynamic_interaction_enabled,
                ..crate::agent::AgentLimits::default()
            };
            match chat_options.provider {
                crate::agent::ProviderKind::CodexCli
                | crate::agent::ProviderKind::ClaudeCli
                | crate::agent::ProviderKind::AgyCli => {
                    if !chat_options.external_provider_consent {
                        anyhow::bail!(
                            "Для внешнего CLI ({}) включите явное согласие на передачу контекста в правой панели",
                            chat_options.provider.label_ru()
                        );
                    }
                    return Ok(Job::AgentExternalCli {
                        chat: id,
                        provider: chat_options.provider,
                        workspace,
                        policy: chat_options.agent_policy,
                        task: last_prompt,
                        external_consent: chat_options.external_provider_consent,
                        model: (!chat_options.provider_model.trim().is_empty())
                            .then(|| chat_options.provider_model.trim().to_string()),
                    });
                }
                crate::agent::ProviderKind::OllamaLocal
                | crate::agent::ProviderKind::OpenAiCompat
                | crate::agent::ProviderKind::AnthropicApi => {
                    let endpoint = if chat_options.provider_endpoint.trim().is_empty() {
                        match chat_options.provider {
                            crate::agent::ProviderKind::OllamaLocal => {
                                "http://127.0.0.1:11434".to_string()
                            }
                            crate::agent::ProviderKind::AnthropicApi => {
                                "https://api.anthropic.com".to_string()
                            }
                            crate::agent::ProviderKind::OpenAiCompat => anyhow::bail!(
                                "Укажите URL endpoint для OpenAI-совместимого провайдера в правой панели"
                            ),
                            _ => String::new(),
                        }
                    } else {
                        chat_options.provider_endpoint.trim().to_string()
                    };
                    if matches!(
                        chat_options.provider,
                        crate::agent::ProviderKind::OpenAiCompat
                            | crate::agent::ProviderKind::AnthropicApi
                    ) {
                        crate::agent::remote::validate_api_endpoint(
                            &endpoint,
                            chat_options.external_provider_consent,
                        )?;
                    }
                    let model = if chat_options.provider_model.trim().is_empty() {
                        match chat_options.provider {
                            crate::agent::ProviderKind::AnthropicApi => {
                                "claude-sonnet-4-5".to_string()
                            }
                            _ => anyhow::bail!(
                                "Укажите имя модели провайдера ({}) в правой панели",
                                chat_options.provider.label_ru()
                            ),
                        }
                    } else {
                        chat_options.provider_model.trim().to_string()
                    };
                    return Ok(Job::AgentRemoteApi {
                        chat: id,
                        provider: chat_options.provider,
                        endpoint,
                        model,
                        context: requested_ctx,
                        workspace,
                        policy: chat_options.agent_policy,
                        exec_policy: chat_options.exec_policy,
                        external_consent: chat_options.external_provider_consent,
                        task: last_prompt,
                        prior_messages: build_prior(messages),
                        limits,
                    });
                }
                crate::agent::ProviderKind::LocalLlama => {
                    let selected_model = catalog::resolve_selection(
                        &self.models,
                        &self.state.settings.selected_repo,
                    )
                    .filter(|m| m.loadable() && m.kind() == catalog::ModelKind::Text)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Для локального агента выберите доступную проверенную текстовую GGUF-модель"
                        )
                    })?
                    .clone();
                    return Ok(Job::AgentLocal {
                        chat: id,
                        model: selected_model,
                        context: requested_ctx,
                        workspace,
                        policy: chat_options.agent_policy,
                        exec_policy: chat_options.exec_policy,
                        task: last_prompt,
                        prior_messages: build_prior(messages),
                        limits,
                    });
                }
            }
        }

        let selected_model =
            catalog::resolve_selection(&self.models, &self.state.settings.selected_repo)
                .filter(|m| m.loadable())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Выберите доступную проверенную GGUF-модель на вкладке «Модели»"
                    )
                })?
                .clone();
        let is_aux = catalog::is_auxiliary_encoder(&selected_model.filename);
        let base_kind = if selected_model.kind() != catalog::ModelKind::Text {
            selected_model.kind()
        } else if is_aux {
            catalog::ModelKind::Image
        } else {
            catalog::ModelKind::Text
        };
        let kind = detect_media_intent(&last_prompt, base_kind);
        if kind != catalog::ModelKind::Text {
            if last_prompt.is_empty() {
                anyhow::bail!("Введите текстовый промпт для генерации");
            }
            let media_model = if selected_model.kind() == kind && !is_aux {
                selected_model
            } else {
                catalog::find_working_media_model(&self.models, kind).unwrap_or(selected_model)
            };
            return Ok(Job::GenerateMedia {
                chat: id,
                sd_cli: self.state.settings.sd_cli_path.clone(),
                model: media_model,
                kind,
                prompt: last_prompt,
                options: chat_options.media_gen.clone(),
                outputs: self.data.join("outputs"),
            });
        }
        let model = selected_model;
        let mut messages = messages;
        let mut sys = chat_options.system_prompt.trim().to_owned();
        if !chat_options.skills.trim().is_empty() {
            sys.push_str("\n\nSKILLS:\n");
            sys.push_str(chat_options.skills.trim());
        }
        if !sys.is_empty() {
            let mut sm = Message::user(sys);
            sm.role = "system".into();
            messages.insert(0, sm);
        }
        let requested_ctx = if chat_options.context > 0 {
            chat_options.context
        } else {
            self.state.settings.context_override
        };
        Ok(Job::Direct {
            chat: id,
            model,
            context: requested_ctx,
            messages,
            max_tokens: self.state.settings.max_tokens,
        })
    }
    pub(super) fn retry_response(&mut self) -> anyhow::Result<()> {
        if self.busy {
            anyhow::bail!("Дождитесь завершения текущей операции");
        }
        let chat = &self.state.chats[self.active];
        if chat.model == "KAIC" || self.state.settings.use_kaic {
            anyhow::bail!("Проверьте существующую задачу KAIC перед повторной отправкой");
        }
        let last_user = chat
            .messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or_else(|| anyhow::anyhow!("Нет запроса для повтора"))?;
        let messages = chat.messages[..=last_user]
            .iter()
            .filter(|m| m.state != "предыдущая попытка")
            .cloned()
            .collect();
        let job = self.direct_job(chat.id, messages)?;
        let chat = &mut self.state.chats[self.active];
        if let Some(m) = chat.messages.last_mut()
            && m.role == "assistant"
        {
            m.state = "предыдущая попытка".into();
        }
        chat.updated_at = crate::storage::epoch_now();
        chat.messages.push(Message::assistant());
        chat.status = "Выполняется".into();
        chat.model = self.state.settings.selected_repo.clone();
        self.mark_dirty();
        let _ = self.persist();
        self.start(job);
        Ok(())
    }
    pub(super) fn attachment_ui(&mut self, ctx: &egui::Context) {
        if !self.attach_open {
            return;
        }
        let lang = self.lang();
        if let Some(path) = self.file_browser.show(ctx, &mut self.attach_open, lang) {
            match crate::attachments::read_attachment_unrestricted(&path) {
                Ok(a) => {
                    if let Some(p) = &a.image_path {
                        let _ = self.ensure_media_texture(ctx, p);
                    }
                    self.attachments.push(a);
                    self.attach_open = false;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
    }
}
pub(super) fn centered(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = content_width(ui.available_width());
    let margin = ((ui.available_width() - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui);
        });
    });
}

fn render_agent_activity(ui: &mut egui::Ui, index: usize, m: &Message, lang: super::i18n::Lang) {
    let pal = palette::p();
    egui::Frame::new()
        .fill(pal.surface_1)
        .stroke(Stroke::new(1.0, pal.line))
        .corner_radius(10)
        .inner_margin(10)
        .show(ui, |ui| {
            let header = format!(
                "{} · {} {}",
                lang.t("Agent actions", "Действия агента"),
                m.agent_events.len(),
                lang.t("events", "событий")
            );
            egui::CollapsingHeader::new(RichText::new(header).strong().size(12.0).color(pal.ink))
                .id_salt((index, "agent-timeline"))
                .default_open(true)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (ev_i, ev) in m.agent_events.iter().enumerate() {
                        match ev {
                            crate::agent::AgentEvent::Step {
                                step,
                                max_steps,
                                model,
                                summary,
                                ..
                            } => {
                                ui.label(
                                    RichText::new(format!(
                                        "▸ [{step}/{max_steps}] {model} — {summary}"
                                    ))
                                    .monospace()
                                    .size(11.0)
                                    .color(pal.ink_muted),
                                );
                            }
                            crate::agent::AgentEvent::ToolCall {
                                name,
                                args_summary,
                                policy_note,
                                ..
                            } => {
                                ui.label(
                                    RichText::new(format!(
                                        "  → {name} ({policy_note}): {args_summary}"
                                    ))
                                    .monospace()
                                    .size(11.0)
                                    .color(pal.ink),
                                );
                            }
                            crate::agent::AgentEvent::ApprovalRequested {
                                tool, summary, ..
                            } => {
                                ui.label(
                                    RichText::new(format!("  ? {tool}: {summary}"))
                                        .size(11.0)
                                        .color(pal.brand_text),
                                );
                            }
                            crate::agent::AgentEvent::ApprovalResolved {
                                approved, reason, ..
                            } => {
                                let col = if *approved { pal.brand_text } else { pal.err };
                                let sym = if *approved { "✓" } else { "✕" };
                                ui.label(
                                    RichText::new(format!("  {sym} {reason}"))
                                        .size(11.0)
                                        .color(col),
                                );
                            }
                            crate::agent::AgentEvent::ToolResult {
                                name,
                                ok,
                                summary,
                                output_preview,
                                diff_preview,
                                ..
                            } => {
                                let col = if *ok { pal.ceramic_200 } else { pal.err };
                                let sym = if *ok { "✓" } else { "✕" };
                                ui.label(
                                    RichText::new(format!("  {sym} {name}: {summary}"))
                                        .monospace()
                                        .size(11.0)
                                        .color(col),
                                );
                                if let Some(diff) = diff_preview
                                    && !diff.trim().is_empty()
                                {
                                    egui::CollapsingHeader::new(
                                        RichText::new("Diff").size(10.5).color(pal.brand_text),
                                    )
                                    .id_salt((index, "tool-diff", ev_i))
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for dline in diff.lines() {
                                            let dcol = if dline.starts_with('+')
                                                && !dline.starts_with("+++")
                                            {
                                                pal.ok
                                            } else if dline.starts_with('-')
                                                && !dline.starts_with("---")
                                            {
                                                pal.err
                                            } else {
                                                pal.ink_muted
                                            };
                                            ui.label(
                                                RichText::new(dline)
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(dcol),
                                            );
                                        }
                                    });
                                }
                                if !output_preview.trim().is_empty() {
                                    egui::CollapsingHeader::new(
                                        RichText::new(lang.t("Output", "Вывод"))
                                            .size(10.5)
                                            .color(pal.ink_muted),
                                    )
                                    .id_salt((index, "tool-out", ev_i))
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(output_preview)
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(pal.ink_muted),
                                            )
                                            .selectable(true)
                                            .wrap(),
                                        );
                                    });
                                }
                            }
                            crate::agent::AgentEvent::Blocked { reason } => {
                                ui.label(
                                    RichText::new(format!("! {reason}"))
                                        .size(11.5)
                                        .color(pal.err),
                                );
                            }
                            crate::agent::AgentEvent::Cancelled { summary, .. } => {
                                ui.label(
                                    RichText::new(format!("■ {summary}"))
                                        .size(11.5)
                                        .color(pal.ink_muted),
                                );
                            }
                            crate::agent::AgentEvent::Completed { summary, .. } => {
                                ui.label(
                                    RichText::new(format!("✓ {summary}"))
                                        .size(11.5)
                                        .color(pal.brand_text),
                                );
                            }
                        }
                    }

                    if !m.changed_files.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(lang.t("Changed:", "Изменено:"))
                                    .size(11.0)
                                    .color(pal.ink_muted),
                            );
                            for f in &m.changed_files {
                                egui::Frame::new()
                                    .fill(pal.surface_2)
                                    .corner_radius(6)
                                    .inner_margin(egui::Margin::symmetric(6, 2))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(f).monospace().size(10.5).color(pal.ink),
                                        );
                                    });
                            }
                        });
                    }

                    if !m.subtasks.is_empty() {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(
                            RichText::new(lang.t("Subtasks", "Дочерние подзадачи"))
                                .strong()
                                .size(11.5)
                                .color(pal.ink),
                        );
                        for (st_idx, st) in m.subtasks.iter().enumerate() {
                            let accent = if st_idx % 2 == 0 {
                                pal.local
                            } else {
                                pal.agent_3
                            };
                            let (ver_tag, ver_col) = if st.verified {
                                (lang.t("verified", "подтверждено"), pal.ok)
                            } else if st.status == "invalidated_after_edit" {
                                (lang.t("invalidated", "аннулировано"), pal.err)
                            } else {
                                (lang.t("unverified", "не подтверждено"), pal.warn)
                            };
                            egui::Frame::new()
                                .fill(pal.surface_0)
                                .stroke(Stroke::new(1.0, accent))
                                .corner_radius(6)
                                .inner_margin(6)
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} (parent {}) · {} · {} · {}",
                                                st.id,
                                                st.parent_id,
                                                st.title,
                                                st.effective_policy.label_en(),
                                                st.model
                                            ))
                                            .monospace()
                                            .size(10.5)
                                            .color(pal.ink),
                                        );
                                        ui.label(
                                            RichText::new(format!("[{ver_tag}]"))
                                                .monospace()
                                                .strong()
                                                .size(10.5)
                                                .color(ver_col),
                                        );
                                    });
                                    ui.label(
                                        RichText::new(format!("[{}] {}", st.status, st.summary))
                                            .size(11.0)
                                            .color(pal.ink_muted),
                                    );
                                    if let Some(ev) = &st.verification_evidence {
                                        ui.label(
                                            RichText::new(format!("✓ {ev}"))
                                                .size(10.5)
                                                .color(pal.ok),
                                        );
                                    }
                                    if !st.verified_file_hashes.is_empty() {
                                        let hashes = st
                                            .verified_file_hashes
                                            .iter()
                                            .map(|(f, h)| {
                                                let short: String = h.chars().take(8).collect();
                                                format!("{f}@{short}")
                                            })
                                            .collect::<Vec<_>>()
                                            .join(" · ");
                                        ui.label(
                                            RichText::new(format!("sha256: {hashes}"))
                                                .monospace()
                                                .size(10.0)
                                                .color(pal.ink_muted),
                                        );
                                    }
                                });
                        }
                    }
                });
        });
}

fn content_width(available: f32) -> f32 {
    available.min(768.0)
}

/// Определяет, запрашивает ли пользователь генерацию фото или видео (даже если в селекторе сейчас выбрана текстовая модель).
fn detect_media_intent(prompt: &str, selected_kind: catalog::ModelKind) -> catalog::ModelKind {
    let low = prompt.to_lowercase();
    let is_code_request = low.contains("код")
        || low.contains("code")
        || low.contains("rust")
        || low.contains("python")
        || low.contains("json")
        || low.contains("функци");

    if !is_code_request {
        let explicit_video = [
            "сгенерируй видео",
            "сделай видео",
            "создай видео",
            "сними видео",
            "видеоролик",
            "анимаци",
            "generate video",
            "generate a video",
            "make a video",
            "create a video",
        ]
        .iter()
        .any(|k| low.contains(k));
        if explicit_video {
            return catalog::ModelKind::Video;
        }

        let explicit_photo = [
            "иллюстрац",
            "сгенерируй фото",
            "сгенерируй картинк",
            "сгенерируй изображен",
            "нарисуй",
            "рисунок",
            "фотореалист",
            "сделай фото",
            "сделай картинк",
            "создай фото",
            "создай картинк",
            "обложк",
            "плакат",
            "постер",
            "портрет ",
            "пейзаж ",
            "generate image",
            "generate an image",
            "generate a photo",
            "generate a picture",
            "generate a photorealistic",
            "generate photorealistic",
            "illustration",
            "draw a ",
        ]
        .iter()
        .any(|k| low.contains(k));
        if explicit_photo {
            return catalog::ModelKind::Image;
        }
    }

    selected_kind
}

#[cfg(test)]
mod history_review_regressions {
    use super::*;
    use crate::{kaic::Task, storage};

    fn app(dir: &std::path::Path) -> Studio {
        let mut app = Studio::from_context(&egui::Context::default(), dir.to_path_buf());
        app.state.settings.use_kaic = true;
        app.state.settings.kaic_url = "http://127.0.0.1:1".into();
        app.token = "test-only".into();
        app
    }

    #[test]
    fn history_review_restore_running_preserves_partial_and_enables_retry() {
        for partial in ["", "Частичный ответ"] {
            let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let mut chat = storage::Chat::new();
            chat.model = "fixture".into();
            chat.status = "Выполняется".into();
            chat.messages = vec![Message::user("Запрос".into()), Message::assistant()];
            chat.messages[1].content = partial.into();
            let state = storage::SavedState {
                chats: vec![chat],
                ..Default::default()
            };
            storage::save_json(&dir.path().join("state.json"), &state).unwrap();
            let mut restored =
                Studio::from_context(&egui::Context::default(), dir.path().to_path_buf());
            let last = restored.state.chats[0].messages.last().unwrap();
            assert_eq!(last.content, partial);
            assert!(last.error.is_some(), "restored assistant must expose retry");
            assert_eq!(last.state, "сеанс прерван");
            let model_path = dir.path().join("test.gguf");
            std::fs::write(&model_path, b"GGUF").unwrap();
            restored.models = vec![serde_json::from_value(serde_json::json!({"repo":"fixture","filename":"test.gguf","path":model_path,"size":4,"sha256":"test","status":"verified"})).unwrap()];
            restored.state.settings.selected_repo = "fixture".into();
            restored.worker.shutdown();
            restored.retry_response().unwrap();
            assert_eq!(restored.state.chats[0].messages[1].content, partial);
            assert_eq!(
                restored.state.chats[0].messages[1].state,
                "предыдущая попытка"
            );
            assert_eq!(
                restored.state.chats[0]
                    .messages
                    .iter()
                    .filter(|m| m.role == "user")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn history_review_failed_b_cannot_refresh_a_at_b_base() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = app(dir.path());
        let id = app.state.chats[0].id;
        let task: Task = serde_json::from_value(serde_json::json!({"id":"A","category":"coding","status":"done","context":[{"role":"user","content":"A request","at":"now"},{"role":"assistant","content":"A answer","at":"now"}]})).unwrap();
        app.apply_task(id, task.clone());
        app.state.chats[0].model = "KAIC".into();
        app.worker.shutdown(); // deterministic submission failure, no network/inference
        app.composer = "B request".into();
        app.send_message().unwrap();
        // The refresh control exists exactly when kaic_task is Some.
        if app.state.chats[0].kaic_task.is_some() {
            app.apply_task(id, task);
        }
        let chat = &app.state.chats[0];
        assert!(
            chat.messages.iter().any(|m| m.content == "B request"),
            "refresh erased B"
        );
        assert_eq!(
            chat.messages
                .iter()
                .filter(|m| m.content == "A answer")
                .count(),
            1,
            "refresh duplicated A"
        );
        assert!(
            chat.kaic_task.is_none(),
            "failed B must not retain A's task ID"
        );
        app.persist().unwrap();
        let saved: storage::SavedState = storage::load_json(&app.state_path).unwrap();
        assert!(saved.chats[0].kaic_task.is_none());
    }

    #[test]
    fn history_review_initial_save_failure_stops_send_and_keeps_input() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut app = app(dir.path());
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"blocked").unwrap();
        app.state_path = blocker.join("state.json");
        app.composer = "Unsaved request".into();
        app.attachments
            .push(crate::attachments::validate_text("note.txt", b"attachment text", 0).unwrap());
        let result = app.send_message();
        assert!(
            result.is_err(),
            "mandatory initial save failure must propagate, not start backend"
        );
        assert!(!app.busy);
        assert!(app.pending_chat.is_none());
        assert_eq!(app.composer, "Unsaved request");
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.attachments[0].text, "attachment text");
        assert!(app.error.as_deref().unwrap().contains("сохранить"));
        assert!(
            app.state.chats[0].messages.is_empty(),
            "failed staging must roll back to avoid duplicate retry"
        );
        // Fix the path and resend: exactly one user turn should be persisted.
        app.state_path = dir.path().join("state.json");
        app.worker.shutdown();
        app.send_message().unwrap();
        assert_eq!(
            app.state.chats[0]
                .messages
                .iter()
                .filter(|m| m.role == "user")
                .count(),
            1
        );
        assert!(app.composer.is_empty());
        assert!(app.attachments.is_empty());
    }

    #[test]
    fn agent_ui_validates_workspace_external_consent_and_renders_timeline_and_approval() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ws = dir.path().join("task-ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.state.chats[0].options.agent_enabled = true;
        app.composer = "Проверь файл".into();

        // Без workspace запуск агента должен отклоняться.
        assert!(app.send_message().is_err());
        assert!(app.state.chats[0].messages.is_empty());

        // Внешний провайдер без явного согласия должен отклоняться.
        app.state.chats[0].options.workspace_path = ws.display().to_string();
        app.state.chats[0].options.provider = crate::agent::ProviderKind::CodexCli;
        app.state.chats[0].options.external_provider_consent = false;
        assert!(app.send_message().is_err());

        // Рендер таймлайна, подзадач и карточки подтверждения на широком и узком экране.
        let mut assistant = Message::assistant();
        assistant.content = "Исправил и проверил.".into();
        assistant.changed_files = vec!["calc.py".into()];
        assistant.agent_events = vec![
            crate::agent::AgentEvent::Step {
                step: 1,
                max_steps: 12,
                provider: crate::agent::ProviderKind::LocalLlama,
                model: "gemma26".into(),
                summary: "Шаг 1/12".into(),
            },
            crate::agent::AgentEvent::ToolResult {
                id: "c1".into(),
                name: "edit_file".into(),
                ok: true,
                summary: "edit_file(calc.py) · 1 замена".into(),
                output_preview: "Файл calc.py обновлён".into(),
                diff_preview: None,
                changed_files: vec!["calc.py".into()],
            },
        ];
        assistant.subtasks = vec![crate::agent::SubtaskRecord {
            id: "task-1-sub-1".into(),
            parent_id: "task-1".into(),
            title: "Check alpha".into(),
            requested_policy: crate::agent::AgentPolicy::ReadOnly,
            effective_policy: crate::agent::AgentPolicy::ReadOnly,
            owned_paths: vec!["alpha.txt".into()],
            provider: crate::agent::ProviderKind::LocalLlama,
            model: "gemma26".into(),
            status: "completed_unverified".into(),
            summary: "alpha ok".into(),
            changed_files: vec![],
            artifact_hashes: std::collections::BTreeMap::new(),
            verified: false,
            verification_evidence: None,
            verified_file_hashes: std::collections::BTreeMap::new(),
        }];
        let chat_id = app.state.chats[0].id;
        app.state.chats[0].messages = vec![Message::user("Исправь calc.py".into()), assistant];
        app.pending_approval = Some((
            chat_id,
            crate::agent::PendingApproval {
                request_id: "req-12345678".into(),
                action_digest: "deadbeef".into(),
                tool: "edit_file".into(),
                isolation: crate::agent::IsolationLevel::WorkspaceFdNoFollow,
                summary: "Правка файла calc.py".into(),
                canonical_target: "calc.py".into(),
                risk_note: "Точечная замена внутри workspace".into(),
            },
        ));
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
