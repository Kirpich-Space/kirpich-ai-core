//! Структура интерфейса адаптирована из Ollama Desktop (MIT), Copyright (c) Ollama.
//! Источник и полный текст лицензии: THIRD_PARTY_NOTICES; rev 42ac2c0c1e132c0b4f0d9cfcd4e7950f0c5470bc.
mod anim;
mod calendar;
mod chat;
mod file_browser;
mod i18n;
mod icons;
mod live_canvas;
mod markdown;
mod models;
mod palette;
mod right_panel;
mod settings;
mod transitions;
use crate::{
    attachments::Attachment,
    catalog::{self, Model},
    desktop::{hf, skills},
    kaic::Task,
    service::{Event, Job, Worker},
    storage::{self, Chat, SavedState},
};
use eframe::egui::{self, Color32, RichText, Stroke};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;
#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Chat,
    Models,
    Workspace,
}
pub struct Studio {
    state: SavedState,
    state_path: PathBuf,
    data: PathBuf,
    active: usize,
    tab: Tab,
    models: Vec<Model>,
    catalog_warnings: Vec<String>,
    worker: Worker,
    busy: bool,
    pending_chat: Option<Uuid>,
    status: String,
    error: Option<String>,
    loaded: Option<String>,
    dirty: Option<Instant>,
    composer: String,
    attachments: Vec<Attachment>,
    attach_open: bool,
    file_browser: file_browser::Browser,
    date_cutoffs: Option<calendar::Cutoffs>,
    date_checked: Instant,
    delete_confirmation: Option<Uuid>,
    selected_chats: HashSet<Uuid>,
    select_mode: bool,
    batch_delete_confirmation: bool,
    search: String,
    sidebar: bool,
    right_panel: bool,
    disk_skills: Vec<skills::SkillSummary>,
    skill_search: String,
    hf: models::HfPanel,
    model_search: String,
    model_picker_open: bool,
    top_model_picker_open: bool,
    model_cursor: usize,
    rename: Option<usize>,
    rename_value: String,
    settings_open: bool,
    token: String,
    kaic_info: Option<Value>,
    media_textures: HashMap<PathBuf, egui::TextureHandle>,
    kich_texture: Option<egui::TextureHandle>,
    pending_approval: Option<(Uuid, crate::agent::PendingApproval)>,
    provider_caps: Vec<crate::agent::ProviderCapabilities>,
    live_canvas: live_canvas::LiveCanvas,
}
impl Studio {
    pub fn new(cc: &eframe::CreationContext<'_>, data: PathBuf) -> Self {
        Self::from_context(&cc.egui_ctx, data)
    }
    pub fn from_context(ctx: &egui::Context, data: PathBuf) -> Self {
        configure_style(ctx);
        let (mut state, path, error) = restore_state(&data);
        for c in &mut state.chats {
            if let Some(m) = c.messages.last_mut()
                && m.role == "assistant"
                && m.content.is_empty()
                && m.error.is_none()
                && !c.status.is_empty()
                && c.status != "Выполняется"
            {
                m.error = Some(c.status.clone());
                m.state = "ошибка".into();
            }
            if c.status == "Выполняется" {
                c.status = "Предыдущий сеанс завершился до подтверждения ответа".into();
                if let Some(m) = c.messages.last_mut()
                    && m.role == "assistant"
                {
                    m.error = Some(c.status.clone());
                    m.state = "сеанс прерван".into();
                }
            }
        }
        if state.chats.is_empty() {
            state.chats.push(Chat::new());
        }
        let (mut models, catalog_warnings) =
            catalog::discover(std::path::Path::new(catalog::MODEL_ROOT));
        models.extend(hf::registered(&data));
        let hf = models::HfPanel::new(data.clone());
        merge_media_models(&mut models, std::path::Path::new(catalog::MODEL_ROOT));
        if !hf.directory.is_empty() {
            merge_media_models(&mut models, std::path::Path::new(&hf.directory));
        }
        if catalog::resolve_selection(&models, &state.settings.selected_repo)
            .is_none_or(|m| !m.loadable())
            && let Some(m) = catalog::default_model(&models)
        {
            state.settings.selected_repo = m.selection_id();
        }
        let repaint = ctx.clone();
        let worker = Worker::new(move || repaint.request_repaint());
        let disk_skills = skills::discover();
        let init_lang = i18n::Lang::from_code(&state.settings.language);
        Self {
            state,
            state_path: path,
            data,
            active: 0,
            tab: Tab::Chat,
            models,
            catalog_warnings,
            worker,
            busy: false,
            pending_chat: None,
            status: init_lang
                .t("Local · ready", "Локально · готово к работе")
                .into(),
            error,
            loaded: None,
            dirty: None,
            composer: String::new(),
            attachments: vec![],
            attach_open: false,
            file_browser: file_browser::Browser::default(),
            date_cutoffs: calendar::Cutoffs::local(),
            date_checked: Instant::now(),
            delete_confirmation: None,
            selected_chats: HashSet::new(),
            select_mode: false,
            batch_delete_confirmation: false,
            search: String::new(),
            sidebar: false,
            right_panel: true,
            disk_skills,
            skill_search: String::new(),
            hf,
            model_search: String::new(),
            model_picker_open: false,
            top_model_picker_open: false,
            model_cursor: 0,
            rename: None,
            rename_value: String::new(),
            settings_open: false,
            token: std::env::var("KAIC_CONTROL_TOKEN").unwrap_or_default(),
            kaic_info: None,
            media_textures: HashMap::new(),
            kich_texture: None,
            pending_approval: None,
            provider_caps: crate::agent::probe_providers(),
            live_canvas: live_canvas::LiveCanvas::default(),
        }
    }
    pub(super) fn lang(&self) -> i18n::Lang {
        i18n::Lang::from_code(&self.state.settings.language)
    }
    /// Лениво загружает эталонный `Kich.png` (`1254×1254`, `TextureOptions::NEAREST`, без перекраски и кропа)
    /// строго один раз на процесс (через `OnceLock`), соблюдая `kaic-project/docs/branding/kich-identity.md`.
    pub(super) fn ensure_kich_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureId> {
        if let Some(ref tex) = self.kich_texture {
            return Some(tex.id());
        }
        static KICH_DECODED: std::sync::OnceLock<(usize, usize, Vec<u8>)> =
            std::sync::OnceLock::new();
        let (w, h, rgba) = KICH_DECODED.get_or_init(|| {
            let im = image::load_from_memory(include_bytes!("../../assets/kich/Kich.png"))
                .expect("Канонический assets/kich/Kich.png должен декодироваться")
                .to_rgba8();
            (im.width() as usize, im.height() as usize, im.into_raw())
        });
        let handle = ctx.load_texture(
            "kich-canonical-df5d727a5c71",
            egui::ColorImage::from_rgba_unmultiplied([*w, *h], rgba),
            egui::TextureOptions::NEAREST,
        );
        let id = handle.id();
        self.kich_texture = Some(handle);
        Some(id)
    }
    pub(super) fn ensure_media_texture(
        &mut self,
        ctx: &egui::Context,
        path: &std::path::Path,
    ) -> Option<egui::TextureId> {
        if let Some(tex) = self.media_textures.get(path) {
            return Some(tex.id());
        }
        let color_image = if crate::attachments::is_svg_path(path)
            || std::fs::read(path)
                .ok()
                .is_some_and(|b| crate::attachments::is_svg_bytes(&b))
        {
            let bytes = std::fs::read(path).ok()?;
            let (w, h, rgba) = crate::attachments::render_svg_premultiplied(&bytes, 1200).ok()?;
            egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba)
        } else {
            let reader = image::ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?;
            let im = reader.decode().ok()?.thumbnail(1200, 900).to_rgba8();
            egui::ColorImage::from_rgba_unmultiplied(
                [im.width() as usize, im.height() as usize],
                &im.into_raw(),
            )
        };
        let tex = ctx.load_texture(
            format!("media:{}", path.display()),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        let id = tex.id();
        self.media_textures.insert(path.to_path_buf(), tex);
        Some(id)
    }
    /// Только выбор видимого состояния; данные не подменяются.
    pub fn show_capture_view(&mut self, scenario: &str) {
        self.sidebar = scenario == "sidebar";
        self.settings_open = scenario == "settings";
        self.model_picker_open = scenario == "model-picker";
    }
    fn mark_dirty(&mut self) {
        if self.dirty.is_none() {
            self.dirty = Some(Instant::now());
        }
    }
    fn persist(&mut self) -> anyhow::Result<()> {
        if let Err(e) = storage::save_json(&self.state_path, &self.state) {
            let error = format!("Не удалось сохранить разговоры: {e:#}");
            self.error = Some(error.clone());
            return Err(anyhow::anyhow!(error));
        }
        self.dirty = None;
        Ok(())
    }
    fn start(&mut self, job: Job) {
        if self.busy {
            return;
        }
        let target = match &job {
            Job::Direct { chat, .. }
            | Job::AgentLocal { chat, .. }
            | Job::AgentRemoteApi { chat, .. }
            | Job::AgentExternalCli { chat, .. }
            | Job::Kaic { chat, .. }
            | Job::GenerateMedia { chat, .. } => Some(*chat),
            _ => None,
        };
        match self.worker.submit(job) {
            Ok(()) => {
                self.busy = true;
                self.pending_chat = target;
                self.error = None;
            }
            Err(e) => {
                let error = e.to_string();
                self.status = error.clone();
                self.pending_chat = target;
                self.apply_chat_error(&error);
                self.pending_chat = None;
                self.error = Some(error);
            }
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.worker.rx.try_recv() {
            match event {
                Event::Status(s) => self.status = s,
                Event::Loaded(m) => self.loaded = m,
                Event::Delta(id, s, reasoning) => {
                    if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id)
                        && let Some(m) = c.messages.last_mut()
                    {
                        c.updated_at = storage::epoch_now();
                        m.state = "ответ".into();
                        if reasoning {
                            m.reasoning.push_str(&s);
                        } else {
                            m.content.push_str(&s);
                        }
                    }
                    self.mark_dirty();
                }
                Event::Metrics {
                    chat,
                    tokens,
                    tps,
                    seconds,
                } => {
                    if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == chat)
                        && let Some(m) = c.messages.last_mut()
                    {
                        m.tokens = Some(tokens);
                        m.tps = Some(tps);
                        m.duration_secs = Some(seconds);
                    }
                    self.mark_dirty();
                }
                Event::Task(id, t) => self.apply_task(id, t),
                Event::AgentEvent(id, ev) => {
                    let mut reload_ws = None;
                    if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id)
                        && let Some(m) = c.messages.last_mut()
                    {
                        c.updated_at = storage::epoch_now();
                        if let crate::agent::AgentEvent::ToolResult {
                            ref changed_files, ..
                        } = ev
                        {
                            for f in changed_files {
                                if !m.changed_files.iter().any(|x| x == f) {
                                    m.changed_files.push(f.clone());
                                }
                                if !self.live_canvas.open_file.is_empty()
                                    && f == &self.live_canvas.open_file
                                {
                                    reload_ws = Some(c.options.workspace_path.clone());
                                }
                            }
                        }
                        m.agent_events.push(ev);
                    }
                    if let Some(ws) = reload_ws {
                        let open = self.live_canvas.open_file.clone();
                        let _ = self.live_canvas.load_file(&ws, &open);
                    }
                    self.mark_dirty();
                }
                Event::SeatUpdate(id, seat) => {
                    let target_file = seat.pointer.target_file.clone();
                    let typing = seat.keyboard.typing;
                    self.live_canvas.seats.upsert_runtime_seat(seat);
                    if typing
                        && let Some(tf) = target_file
                        && !self.live_canvas.open_file.is_empty()
                        && tf == self.live_canvas.open_file
                        && let Some(c) = self.state.chats.iter().find(|c| c.id == id)
                    {
                        let ws = c.options.workspace_path.clone();
                        let open = self.live_canvas.open_file.clone();
                        let _ = self.live_canvas.load_file(&ws, &open);
                    }
                }
                Event::Subtask(id, st) => {
                    if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id)
                        && let Some(m) = c.messages.last_mut()
                    {
                        c.updated_at = storage::epoch_now();
                        if let Some(existing) = m.subtasks.iter_mut().find(|x| x.id == st.id) {
                            *existing = st;
                        } else {
                            m.subtasks.push(st);
                        }
                    }
                    self.mark_dirty();
                }
                Event::ApprovalNeeded(id, req) => {
                    self.pending_approval = Some((id, req));
                }
                Event::ContextUsage(_id, m) => {
                    self.status = format!(
                        "ctx {}% (~{}/{} tok)",
                        m.utilization_pct, m.used_tokens, m.context_limit
                    );
                }
                Event::ApprovalCleared(id) => {
                    if self
                        .pending_approval
                        .as_ref()
                        .is_some_and(|(cid, _)| *cid == id)
                    {
                        self.pending_approval = None;
                    }
                }
                Event::InspectedKaic(v) => self.kaic_info = Some(v),
                Event::GeneratedMedia(chat_id, media) => {
                    if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == chat_id)
                        && let Some(m) = c.messages.last_mut()
                    {
                        c.updated_at = storage::epoch_now();
                        m.state = "готово".into();
                        m.duration_secs = media.elapsed_secs;
                        m.media.push(media);
                    }
                    self.mark_dirty();
                }
                Event::Preview(path, w, h, pixels) => {
                    let texture = ctx.load_texture(
                        format!("media:{}", path.display()),
                        egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels),
                        egui::TextureOptions::LINEAR,
                    );
                    self.media_textures.insert(path, texture);
                }
                Event::InspectedComfy(_) | Event::Media(_) => {}
                Event::Error(e) => {
                    self.status = e.clone();
                    self.apply_chat_error(&e);
                    self.error = Some(e);
                }
                Event::Finished => {
                    self.busy = false;
                    self.pending_approval = None;
                    if let Some(id) = self.pending_chat.take()
                        && let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id)
                        && let Some(m) = c.messages.last_mut()
                        && m.role == "assistant"
                        && m.error.is_none()
                    {
                        m.state = self.status.clone();
                    }
                    for c in &mut self.state.chats {
                        if c.status == "Выполняется" {
                            c.status = self.status.clone();
                        }
                    }
                    self.mark_dirty();
                    let _ = self.persist();
                }
            }
        }
        if self
            .dirty
            .is_some_and(|t| t.elapsed() > Duration::from_millis(750))
        {
            let _ = self.persist();
        }
    }
    fn apply_chat_error(&mut self, error: &str) {
        if let Some(id) = self.pending_chat
            && let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id)
        {
            c.status = error.into();
            if let Some(m) = c.messages.last_mut()
                && m.role == "assistant"
            {
                m.state = if self.worker.cancel.is_cancelled() {
                    "остановлен"
                } else {
                    "ошибка"
                }
                .into();
                m.error = Some(error.into());
            }
        }
        self.mark_dirty();
    }
    fn shortcuts(&mut self, ctx: &egui::Context) {
        for (key, tab) in [
            (egui::Key::Num1, Tab::Chat),
            (egui::Key::Num2, Tab::Models),
            (egui::Key::Num3, Tab::Workspace),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, key)) {
                self.tab = tab;
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.settings_open = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) && !self.busy {
            self.new_chat();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::B)) {
            self.sidebar = !self.sidebar;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Period)) {
            self.right_panel = !self.right_panel;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::L)) {
            self.tab = Tab::Chat;
            self.settings_open = false;
            self.attach_open = false;
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("каик-композитор")));
        }
    }
    fn apply_task(&mut self, id: Uuid, t: Task) {
        if let Some(c) = self.state.chats.iter_mut().find(|c| c.id == id) {
            c.updated_at = storage::epoch_now();
            c.kaic_task = Some(t.id.clone());
            c.status = if t.pending_tool_approval.is_some() {
                "KAIC · ожидает подтверждения инструмента".into()
            } else {
                format!("KAIC · {}", rus_status(&t.status))
            };
            if !t.context.is_empty() {
                let base = c.kaic_base;
                c.messages.truncate(base);
                c.messages.extend(t.messages());
            }
        }
        self.mark_dirty();
    }
    fn new_chat(&mut self) {
        if self.busy {
            return;
        }
        // Запрещаем плодить бесконечные пустые диалоги: если пустой уже есть, переходим в него.
        if let Some(idx) = self.state.chats.iter().position(|c| c.messages.is_empty()) {
            self.active = idx;
            self.tab = Tab::Chat;
            self.settings_open = false;
            return;
        }
        self.state.chats.insert(0, Chat::new());
        self.active = 0;
        self.composer.clear();
        self.attachments.clear();
        self.tab = Tab::Chat;
        self.settings_open = false;
        self.mark_dirty();
    }
    fn top(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang();
        egui::Panel::top("навигация")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(palette::SURFACE_1)
                    .stroke(Stroke::new(1.0, palette::LINE))
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if icons::button(
                        ui,
                        icons::Icon::Sidebar,
                        lang.t("Chat history · Ctrl+B", "История разговоров · Ctrl+B"),
                    )
                    .clicked()
                    {
                        self.sidebar = !self.sidebar;
                    }
                    ui.add_enabled_ui(!self.busy, |ui| {
                        if icons::button(
                            ui,
                            icons::Icon::NewChat,
                            lang.t("New chat · Ctrl+N", "Новый разговор · Ctrl+N"),
                        )
                        .clicked()
                        {
                            self.new_chat();
                        }
                    });
                    ui.add_space(4.0);
                    icons::mark_sized(ui, 20.0);
                    ui.label(
                        RichText::new("KAIC Studio")
                            .strong()
                            .size(13.5)
                            .color(palette::INK),
                    );
                    ui.label(RichText::new("·").color(palette::LINE_STRONG));
                    let title = if self.settings_open {
                        lang.t("Settings", "Настройки")
                    } else {
                        match self.tab {
                            Tab::Models => lang.t("Models & Downloads", "Модели и загрузки"),
                            Tab::Workspace => lang.t("Live Workspace", "Живой холст"),
                            Tab::Chat => lang.chat_title(self.state.chats[self.active].title.as_str()),
                        }
                    };
                    ui.add(
                        egui::Label::new(
                            RichText::new(title).size(13.0).color(palette::INK_MUTED),
                        )
                        .truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let menu = ui.menu_button("    ", |ui| {
                            for (tab, label) in [
                                (Tab::Chat, lang.t("Chat · Ctrl+1", "Диалог · Ctrl+1")),
                                (
                                    Tab::Models,
                                    lang.t("Models & Downloads · Ctrl+2", "Модели и загрузки · Ctrl+2"),
                                ),
                                (
                                    Tab::Workspace,
                                    lang.t("Live Workspace · Ctrl+3", "Живой холст · Ctrl+3"),
                                ),
                            ] {
                                if ui.selectable_label(self.tab == tab, label).clicked() {
                                    self.tab = tab;
                                    self.settings_open = false;
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui
                                .button(lang.t("Settings · Ctrl+,", "Настройки · Ctrl+,"))
                                .clicked()
                            {
                                self.settings_open = true;
                                ui.close();
                            }
                            if self.loaded.is_some()
                                && ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new(lang.t("Unload model", "Выгрузить модель")),
                                    )
                                    .clicked()
                            {
                                self.start(Job::Unload);
                                ui.close();
                            }
                            ui.separator();
                            ui.label(RichText::new(&self.status).small());
                        });
                        menu.response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                lang.t("Studio menu", "Меню студии"),
                            )
                        });
                        icons::paint(
                            ui,
                            menu.response.rect,
                            icons::Icon::More,
                            palette::INK_MUTED,
                        );
                        menu.response
                            .on_hover_text(lang.t("Studio menu", "Меню студии"));
                        if icons::button(
                            ui,
                            icons::Icon::Settings,
                            lang.t("Settings · Ctrl+,", "Настройки · Ctrl+,"),
                        )
                        .clicked()
                        {
                            self.settings_open = !self.settings_open;
                        }
                        if self.tab == Tab::Chat && !self.settings_open {
                            if icons::button(
                                ui,
                                icons::Icon::RightPanel,
                                lang.t("Inspector · Ctrl+.", "Правая панель параметров · Ctrl+."),
                            )
                            .clicked()
                            {
                                self.right_panel = !self.right_panel;
                            }
                        }
                        // Интерактивная пилюля выбора модели в верхней панели
                        let selected_model = catalog::resolve_selection(
                            &self.models,
                            &self.state.settings.selected_repo,
                        );
                        let (dot, txt, is_active_dot) = if self.busy {
                            (palette::BRAND, self.status.clone(), true)
                        } else if let Some(m) = self.loaded.as_deref() {
                            (palette::CERAMIC_200, m.to_string(), true)
                        } else if let Some(m) = selected_model {
                            (palette::CERAMIC_200, m.alias().to_string(), true)
                        } else {
                            (
                                palette::LINE_STRONG,
                                lang.t("Select model", "Выбрать модель").to_string(),
                                false,
                            )
                        };
                        let pill_resp = egui::Frame::new()
                            .fill(if self.top_model_picker_open {
                                palette::SURFACE_2
                            } else {
                                palette::SURFACE_0
                            })
                            .stroke(Stroke::new(
                                1.0,
                                if self.top_model_picker_open {
                                    palette::BRAND
                                } else {
                                    palette::LINE
                                },
                            ))
                            .corner_radius(12)
                            .inner_margin(egui::Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        anim::pulse_dot(ui, dot, is_active_dot, self.busy);
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(txt).size(12.0).color(
                                                    if is_active_dot || self.busy {
                                                        palette::INK
                                                    } else {
                                                        palette::INK_MUTED
                                                    },
                                                ),
                                            )
                                            .truncate(),
                                        );
                                        let (chev_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(10.0, 10.0),
                                            egui::Sense::hover(),
                                        );
                                        let c = chev_rect.center();
                                        ui.painter().add(egui::Shape::line(
                                            vec![
                                                c + egui::vec2(-3.0, -1.5),
                                                c + egui::vec2(0.0, 1.5),
                                                c + egui::vec2(3.0, -1.5),
                                            ],
                                            Stroke::new(1.2, palette::INK_MUTED),
                                        ));
                                    },
                                );
                            })
                            .response
                            .interact(egui::Sense::click())
                            .on_hover_text(lang.t(
                                "Switch active local model",
                                "Быстрый выбор локальной модели",
                            ));
                        let pill_clicked = pill_resp.clicked() && !self.busy;
                        if pill_clicked {
                            self.top_model_picker_open = !self.top_model_picker_open;
                            self.model_cursor = self
                                .models
                                .iter()
                                .position(|m| {
                                    m.selection_id() == self.state.settings.selected_repo
                                        || m.repo == self.state.settings.selected_repo
                                })
                                .unwrap_or(0);
                        }
                        let mut top_choose = None;
                        let mut top_open = self.top_model_picker_open;
                        egui::Popup::from_response(&pill_resp)
                            .open_bool(&mut top_open)
                            .align(egui::RectAlign::BOTTOM_END)
                            .width(360.0)
                            .frame(
                                egui::Frame::popup(ui.style())
                                    .fill(palette::SURFACE_1)
                                    .stroke(Stroke::new(1.0, palette::LINE_STRONG))
                                    .corner_radius(12)
                                    .inner_margin(10),
                            )
                            .show(|ui| {
                                top_choose =
                                    self.render_model_picker_contents(ui, pill_clicked);
                            });
                        self.top_model_picker_open = top_open;
                        if let Some(repo) = top_choose {
                            self.select_model(&repo);
                        }

                        // Живой индикатор активной загрузки HF в верхней панели
                        if let Some((_, frac, speed)) = self.active_download_badge() {
                            ui.ctx().request_repaint_after(Duration::from_millis(250));
                            let pct = (frac * 100.0).round() as u32;
                            let spd = models::format_speed(speed, lang);
                            let dl_txt = if spd.is_empty() {
                                format!("↓ {pct}%")
                            } else {
                                format!("↓ {pct}% · {spd}")
                            };
                            let dl_pill = egui::Frame::new()
                                .fill(palette::SURFACE_0)
                                .stroke(Stroke::new(1.0, palette::BRAND))
                                .corner_radius(12)
                                .inner_margin(egui::Margin::symmetric(10, 4))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        anim::pulse_dot(ui, palette::BRAND, true, true);
                                        ui.label(
                                            RichText::new(dl_txt)
                                                .monospace()
                                                .strong()
                                                .size(11.5)
                                                .color(palette::p().brand_text),
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click())
                                .on_hover_text(lang.t(
                                    "Open Download Manager",
                                    "Открыть экран загрузок",
                                ));
                            if dl_pill.clicked() {
                                self.open_downloads_tab();
                            }
                        }

                        // Быстрый переключатель живого холста и мульти-курсоров (Ctrl+3)
                        let seats_count = self.live_canvas.seats.seats().len();
                        let control_live = self.live_canvas.seats.is_control_live();
                        let ws_active = self.tab == Tab::Workspace && !self.settings_open;
                        let ws_stroke = if control_live {
                            palette::ERR
                        } else if ws_active {
                            palette::BRAND
                        } else {
                            palette::LINE
                        };
                        let ws_dot = if control_live {
                            palette::ERR
                        } else if ws_active {
                            palette::BRAND
                        } else {
                            palette::CERAMIC_200
                        };
                        let ws_label = if control_live {
                            format!(
                                "{} · {seats_count}",
                                lang.t("LIVE SEATS", "АКТИВНО")
                            )
                        } else {
                            format!("{} · {seats_count}", lang.t("Canvas", "Холст"))
                        };
                        let ws_pill = egui::Frame::new()
                            .fill(if ws_active {
                                palette::SURFACE_2
                            } else {
                                palette::SURFACE_0
                            })
                            .stroke(Stroke::new(1.0, ws_stroke))
                            .corner_radius(12)
                            .inner_margin(egui::Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    anim::pulse_dot(ui, ws_dot, true, control_live);
                                    ui.label(
                                        RichText::new(ws_label)
                                            .size(11.5)
                                            .strong()
                                            .color(if control_live {
                                                palette::ERR
                                            } else {
                                                palette::INK
                                            }),
                                    );
                                });
                            })
                            .response
                            .interact(egui::Sense::click())
                            .on_hover_text(lang.t(
                                "Live Workspace: dynamic editing, multi-cursors & virtual keyboards · Ctrl+3",
                                "Живой холст: динамическое редактирование, мульти-курсоры и клавиатуры · Ctrl+3",
                            ));
                        if ws_pill.clicked() {
                            self.tab = if ws_active {
                                Tab::Chat
                            } else {
                                Tab::Workspace
                            };
                            self.settings_open = false;
                        }

                        if self.busy {
                            ui.spinner();
                        }
                    });
                });
            });
    }
    fn sidebar_content(&mut self, ui: &mut egui::Ui) {
        let pal = palette::p();
        let lang = self.lang();
        ui.set_max_width(222.0);
        ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("KAIC Studio")
                    .strong()
                    .size(15.5)
                    .color(pal.ink),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                anim::keycap(ui, "Ctrl+B");
            });
        });
        ui.add_space(12.0);
        if ui
            .add_enabled(
                !self.busy,
                egui::Button::new(lang.t("+  New chat", "+  Новый разговор"))
                    .frame(false)
                    .min_size(egui::vec2(ui.available_width(), 32.0)),
            )
            .clicked()
        {
            self.new_chat();
        }
        for (tab, label) in [
            (Tab::Chat, lang.t("Chats", "Диалоги")),
            (Tab::Models, lang.t("Models", "Локальные модели")),
            (Tab::Workspace, lang.t("Live Workspace", "Живой холст")),
        ] {
            if ui
                .add(
                    egui::Button::new(label)
                        .stroke(Stroke::NONE)
                        .fill(if self.tab == tab && !self.settings_open {
                            pal.surface_2
                        } else {
                            Color32::TRANSPARENT
                        })
                        .selected(self.tab == tab && !self.settings_open)
                        .min_size(egui::vec2(ui.available_width(), 32.0)),
                )
                .clicked()
            {
                self.tab = tab;
                self.settings_open = false;
            }
        }
        ui.add_space(12.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text(lang.t("Search chats…", "Поиск разговоров…"))
                .margin(egui::Margin::symmetric(10, 6))
                .desired_width(f32::INFINITY),
        );
        ui.add_space(16.0);
        // Заголовок «Разговоры» + кнопка «Выбрать» для массового выделения/удаления
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.t("Chats", "Разговоры"))
                    .size(11.5)
                    .color(pal.ink_muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let toggle_lbl = if self.select_mode {
                    lang.t("Done", "Готово")
                } else {
                    lang.t("Select", "Выбрать")
                };
                if ui
                    .add_enabled(
                        !self.busy && !self.state.chats.is_empty(),
                        egui::Button::new(RichText::new(toggle_lbl).size(11.0).color(
                            if self.select_mode {
                                pal.brand_text
                            } else {
                                pal.ink_muted
                            },
                        ))
                        .frame(false),
                    )
                    .on_hover_text(lang.t(
                        "Select multiple chats to delete (or Ctrl+click)",
                        "Выделить несколько диалогов для удаления (или Ctrl+клик)",
                    ))
                    .clicked()
                {
                    self.select_mode = !self.select_mode;
                    if !self.select_mode {
                        self.selected_chats.clear();
                    }
                }
            });
        });

        let selecting = self.select_mode || !self.selected_chats.is_empty();
        if selecting {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui
                    .small_button(RichText::new(lang.t("All", "Все")).size(11.0))
                    .clicked()
                {
                    for i in 0..self.state.chats.len() {
                        if self.chat_matches(i, &self.search) {
                            self.selected_chats.insert(self.state.chats[i].id);
                        }
                    }
                    self.select_mode = true;
                }
                if ui
                    .small_button(RichText::new(lang.t("Clear", "Снять")).size(11.0))
                    .clicked()
                {
                    self.selected_chats.clear();
                    self.select_mode = false;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let count = self.selected_chats.len();
                    let del_btn = egui::Button::new(
                        RichText::new(format!("{} ({count})", lang.t("Delete", "Удалить")))
                            .size(11.0)
                            .strong()
                            .color(if count > 0 {
                                pal.on_brand
                            } else {
                                pal.ink_muted
                            }),
                    )
                    .fill(if count > 0 { pal.err } else { pal.surface_2 })
                    .stroke(Stroke::NONE)
                    .corner_radius(5);
                    if ui.add_enabled(!self.busy && count > 0, del_btn).clicked() {
                        self.batch_delete_confirmation = true;
                    }
                });
            });
            ui.add_space(4.0);
        }

        let scroll_reserve = if selecting { 104.0 } else { 78.0 };
        egui::ScrollArea::vertical()
            .id_salt("история")
            .max_height((ui.available_height() - scroll_reserve).max(40.0))
            .show(ui, |ui| {
                let mut found = false;
                let mut previous_group = None;
                let ctrl_held = ui.input(|inp| inp.modifiers.command);
                for (i, group) in calendar::rows(&self.state.chats, self.date_cutoffs) {
                    if !self.chat_matches(i, &self.search) {
                        continue;
                    }
                    found = true;
                    if previous_group != Some(group) {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(if self.date_cutoffs.is_none() {
                                lang.t("Chats", "Разговоры")
                            } else {
                                group.label_for(lang)
                            })
                            .size(11.5)
                            .color(pal.ink_muted),
                        );
                        previous_group = Some(group);
                    }
                    if self.rename == Some(i) {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.rename_value)
                                .margin(egui::Margin::symmetric(8, 5))
                                .desired_width(f32::INFINITY),
                        );
                        response.request_focus();
                        if ui.input_mut(|v| v.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                        {
                            self.rename = None;
                        } else if ui
                            .input_mut(|v| v.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                        {
                            let value = self.rename_value.clone();
                            if self.rename_chat(i, &value) {
                                self.rename = None;
                            }
                        }
                        continue;
                    }
                    let chat_id = self.state.chats[i].id;
                    let is_checked = self.selected_chats.contains(&chat_id);
                    ui.push_id(chat_id, |ui| {
                        ui.horizontal(|ui| {
                            let raw_title = self.state.chats[i].title.clone();
                            let display_title = lang.chat_title(&raw_title);
                            if selecting {
                                let mut checked = is_checked;
                                if ui.checkbox(&mut checked, "").changed() {
                                    if checked {
                                        self.selected_chats.insert(chat_id);
                                    } else {
                                        self.selected_chats.remove(&chat_id);
                                    }
                                }
                            }
                            let row_width = if selecting {
                                (ui.available_width() - 6.0).max(40.0)
                            } else {
                                (ui.available_width() - 42.0).max(40.0)
                            };
                            let is_active =
                                self.active == i && self.tab == Tab::Chat && !self.settings_open;
                            let row = ui
                                .add_enabled_ui(!self.busy, |ui| {
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(row_width, 32.0),
                                        egui::Layout::top_down(egui::Align::LEFT),
                                        |ui| {
                                            ui.set_max_width(row_width);
                                            ui.add(
                                                egui::Button::new(
                                                    RichText::new(display_title).size(13.5),
                                                )
                                                .stroke(if is_checked {
                                                    Stroke::new(1.0, pal.brand)
                                                } else {
                                                    Stroke::NONE
                                                })
                                                .fill(if is_checked || is_active {
                                                    pal.surface_2
                                                } else {
                                                    Color32::TRANSPARENT
                                                })
                                                .min_size(egui::vec2(row_width, 32.0))
                                                .truncate(),
                                            )
                                        },
                                    )
                                    .inner
                                })
                                .inner;
                            if row.clicked() {
                                if selecting || ctrl_held {
                                    self.select_mode = true;
                                    if !self.selected_chats.remove(&chat_id) {
                                        self.selected_chats.insert(chat_id);
                                    }
                                } else {
                                    self.active = i;
                                    self.tab = Tab::Chat;
                                    self.settings_open = false;
                                    self.composer.clear();
                                    self.attachments.clear();
                                    if ui.ctx().content_rect().width() < 850.0 {
                                        self.sidebar = false;
                                    }
                                }
                            }
                            if !selecting {
                                let more = icons::button(
                                    ui,
                                    icons::Icon::More,
                                    lang.t("Chat menu", "Меню разговора"),
                                );
                                egui::Popup::menu(&more).show(|ui| {
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new(lang.t("Rename", "Переименовать")),
                                        )
                                        .clicked()
                                    {
                                        self.rename = Some(i);
                                        self.rename_value = raw_title;
                                        ui.close();
                                    }
                                    if ui
                                        .button(lang.t("Copy chat", "Копировать разговор"))
                                        .clicked()
                                    {
                                        let text = self.state.chats[i]
                                            .messages
                                            .iter()
                                            .map(|m| format!("{}: {}", m.role, m.content))
                                            .collect::<Vec<_>>()
                                            .join("\n\n");
                                        ui.ctx().copy_text(text);
                                        ui.close();
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new(lang.t("Select", "Выбрать")),
                                        )
                                        .clicked()
                                    {
                                        self.select_mode = true;
                                        self.selected_chats.insert(chat_id);
                                        ui.close();
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Button::new(
                                                RichText::new(lang.t("Delete…", "Удалить…"))
                                                    .color(pal.err),
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.delete_confirmation = Some(chat_id);
                                        ui.close();
                                    }
                                });
                            }
                        });
                    });
                }
                if !found {
                    ui.label(lang.t("No chats found", "Разговоры не найдены"));
                }
            });
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            // Кнопка «Настройки» с векторной иконкой шестерёнки и кейкапом Ctrl+,
            let footer_inner_w = (ui.available_width() - 22.0).max(120.0);
            let settings_resp = egui::Frame::new()
                .fill(if self.settings_open {
                    pal.surface_2
                } else {
                    pal.surface_0
                })
                .stroke(Stroke::new(
                    1.0,
                    if self.settings_open {
                        pal.brand
                    } else {
                        pal.line
                    },
                ))
                .corner_radius(7)
                .inner_margin(egui::Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(footer_inner_w);
                    ui.horizontal(|ui| {
                        icons::inline(
                            ui,
                            icons::Icon::Settings,
                            18.0,
                            if self.settings_open {
                                pal.brand_text
                            } else {
                                pal.ink
                            },
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(lang.t("Settings", "Настройки"))
                                .size(13.0)
                                .color(pal.ink),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            anim::keycap(ui, "Ctrl+,");
                        });
                    });
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_text(lang.t("Open settings · Ctrl+,", "Открыть настройки · Ctrl+,"));
            if settings_resp.hovered() && !self.settings_open {
                ui.painter()
                    .rect_filled(settings_resp.rect, 7, anim::with_alpha(pal.ink, 12));
            }
            if settings_resp.clicked() {
                self.settings_open = !self.settings_open;
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(lang.t("Local · KAIC Studio", "Локально · KAIC Studio"))
                    .size(11.0)
                    .color(pal.ink_muted),
            );
        });
    }
}
impl Studio {
    fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let cur_lang = self.lang();
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("kaic-lang"), cur_lang));
        self.poll(&ctx);
        if self.settings_open && self.hf.poll(&ctx) {
            self.refresh_models_catalog();
        }
        self.shortcuts(&ctx);
        self.delete_dialog(&ctx);
        if self.date_checked.elapsed() >= Duration::from_secs(60) {
            self.date_cutoffs = calendar::Cutoffs::local();
            self.date_checked = Instant::now();
        }
        if !self.busy {
            for file in ctx.input(|i| i.raw.dropped_files.clone()) {
                match crate::attachments::read_attachment_unrestricted(file.path()) {
                    Ok(a) => {
                        if let Some(p) = &a.image_path {
                            let _ = self.ensure_media_texture(&ctx, p);
                        }
                        self.attachments.push(a);
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
        }
        let narrow = ui.available_width() < 850.0;
        self.top(ui);
        let left_t = anim::panel_t(&ctx, "kaic-anim-left", self.sidebar && !narrow);
        if left_t > 0.02 {
            egui::Panel::left("разговоры")
                .exact_size((256.0 * left_t).max(1.0))
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(palette::SURFACE_1)
                        .inner_margin(egui::Margin::symmetric((14.0 * left_t) as i8, 14))
                        .stroke(Stroke::new(1.0, palette::LINE)),
                )
                .show(ui, |ui| {
                    if left_t > 0.35 {
                        self.sidebar_content(ui);
                    }
                });
        }
        let right_open =
            self.right_panel && self.tab == Tab::Chat && !self.settings_open && !narrow;
        let right_t = anim::panel_t(&ctx, "kaic-anim-right", right_open);
        if right_t > 0.02 {
            egui::Panel::right("параметры-диалога")
                .exact_size((310.0 * right_t).max(1.0))
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(palette::SURFACE_1)
                        .inner_margin(egui::Margin::symmetric((12.0 * right_t) as i8, 12))
                        .stroke(Stroke::new(1.0, palette::LINE)),
                )
                .show(ui, |ui| {
                    if right_t > 0.35 {
                        self.right_panel_ui(ui);
                    }
                });
        }
        if let Some(error) = self.error.clone() {
            egui::Panel::bottom("ошибка")
                .frame(
                    egui::Frame::new()
                        .fill(palette::SURFACE_2)
                        .stroke(Stroke::new(1.0, palette::ERR))
                        .inner_margin(10),
                )
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(error).color(palette::INK));
                        if ui.button(self.lang().t("Dismiss", "Скрыть")).clicked() {
                            self.error = None;
                        }
                    });
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette::SURFACE_0).inner_margin(12))
            .show(ui, |ui| match self.tab {
                _ if self.settings_open => self.settings_page(ui),
                Tab::Chat => self.chat_ui(ui),
                Tab::Models => self.models_ui(ui),
                Tab::Workspace => self.live_canvas_ui(ui),
            });
        if narrow && self.sidebar {
            let mut open = true;
            egui::Window::new("Разговоры")
                .open(&mut open)
                .default_width(280.0)
                .show(&ctx, |ui| self.sidebar_content(ui));
            if !open {
                self.sidebar = false;
            }
        }
        self.attachment_ui(&ctx);
        if self.busy || self.dirty.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
impl eframe::App for Studio {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.worker.shutdown();
        let _ = self.persist();
    }
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [
            0x0B as f32 / 255.0,
            0x0B as f32 / 255.0,
            0x0C as f32 / 255.0,
            1.0,
        ]
    }
}

/// 100% нативный Rust GUI на eframe / egui (без WebKitGTK / Electron / браузера).
pub fn run(data: PathBuf) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KAIC Studio")
            .with_app_id("kaic-studio")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([520.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "KAIC Studio",
        options,
        Box::new(move |cc| Ok(Box::new(Studio::new(cc, data)))),
    )
    .map_err(|e| anyhow::anyhow!("Не удалось запустить нативное окно eframe: {e}"))
}

fn configure_style(ctx: &egui::Context) {
    let pal = palette::p();
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(pal.ink);
    style.visuals.panel_fill = pal.surface_0;
    style.visuals.window_fill = pal.surface_1;
    style.visuals.extreme_bg_color = pal.surface_0;
    style.visuals.faint_bg_color = pal.surface_1;
    style.visuals.code_bg_color = pal.surface_2;
    style.visuals.selection.bg_fill = pal.selection;
    style.visuals.selection.stroke = Stroke::new(1.0, pal.line_strong);
    style.visuals.hyperlink_color = pal.brand_text;
    style.visuals.warn_fg_color = pal.warn;
    style.visuals.error_fg_color = pal.err;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.line);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, pal.ink);
    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(7);
        w.bg_fill = pal.surface_2;
        w.weak_bg_fill = pal.surface_1;
        w.bg_stroke = Stroke::new(1.0, pal.line);
        w.fg_stroke = Stroke::new(1.0, pal.ink);
    }
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, pal.line_strong);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.5, pal.line_strong);
    style.visuals.window_corner_radius = egui::CornerRadius::same(10);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 28.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.5));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, style);
}
pub(super) fn merge_media_models(models: &mut Vec<Model>, dir: &std::path::Path) {
    for extra in
        catalog::discover_media_models(dir)
            .into_iter()
            .chain(catalog::discover_media_models(std::path::Path::new(
                catalog::BUILTIN_MEDIA_ROOT,
            )))
    {
        if !models
            .iter()
            .any(|m| m.path == extra.path || m.filename == extra.filename)
        {
            models.push(extra);
        }
    }
}

pub(super) fn rus_status(status: &str) -> &str {
    match status {
        "done" => "завершена",
        "failed" => "ошибка",
        "cancelled" => "отменена",
        "paused" => "приостановлена",
        "waiting_for_human" => "ожидает человека",
        "running" => "выполняется",
        "queued" => "в очереди",
        _ => "состояние не распознано",
    }
}
fn restore_state(data: &std::path::Path) -> (SavedState, PathBuf, Option<String>) {
    let original = data.join("state.json");
    if !original.exists() {
        return (SavedState::default(), original, None);
    }
    match storage::load_json::<SavedState>(&original) {
        Ok(s) => (s, original, None),
        Err(e) => {
            let mut candidates = std::fs::read_dir(data)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|f| {
                    f.file_name()
                        .to_string_lossy()
                        .starts_with("state-recovery-")
                        && f.path().extension().is_some_and(|x| x == "json")
                })
                .collect::<Vec<_>>();
            candidates
                .sort_by_key(|f| std::cmp::Reverse(f.metadata().and_then(|m| m.modified()).ok()));
            for f in candidates {
                if let Ok(state) = storage::load_json::<SavedState>(&f.path()) {
                    return (
                        state,
                        f.path(),
                        Some(format!(
                            "{e:#}. Загружена последняя исправная резервная история; исходный state.json сохранён."
                        )),
                    );
                }
            }
            let path = data.join(format!("state-recovery-{}.json", Uuid::new_v4()));
            (
                SavedState::default(),
                path.clone(),
                Some(format!(
                    "{e:#}. Новые данные сохраняются отдельно: {}",
                    path.display()
                )),
            )
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_sidebar_width_and_rename_keyboard() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().into());
        app.sidebar = true;
        app.state.chats[0].title = "Очень длинный заголовок ".repeat(40);
        let input = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 850.0),
            )),
            events,
            ..Default::default()
        };
        for _ in 0..2 {
            let mut out = ctx.run_ui(input(vec![]), |ui| app.render(ui));
            out.textures_delta.clear();
            let sidebar = out
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::Shape::Rect(r)
                        if r.fill == palette::SURFACE_1
                            && r.rect.left() < 2.0
                            && r.rect.height() > 500.0 =>
                    {
                        Some(r.rect)
                    }
                    _ => None,
                })
                .expect("sidebar");
            assert!((sidebar.width() - 256.0).abs() <= 1.5, "{sidebar:?}");
        }
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        app.rename = Some(0);
        app.rename_value = "Новое имя".into();
        ctx.run_ui(input(vec![]), |ui| app.render(ui))
            .textures_delta
            .clear();
        ctx.run_ui(input(vec![key(egui::Key::Enter)]), |ui| app.render(ui))
            .textures_delta
            .clear();
        assert_eq!(app.rename, None);
        assert_eq!(app.state.chats[0].title, "Новое имя");
        app.rename = Some(0);
        app.rename_value = "Не сохранять".into();
        ctx.run_ui(input(vec![]), |ui| app.render(ui))
            .textures_delta
            .clear();
        ctx.run_ui(input(vec![key(egui::Key::Escape)]), |ui| app.render(ui))
            .textures_delta
            .clear();
        assert_eq!(app.rename, None);
        assert_eq!(app.state.chats[0].title, "Новое имя");
    }
    #[test]
    fn composer_stays_inside_canvas_in_empty_and_conversation_views() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.right_panel = false;
        assert!(
            !app.sidebar,
            "history starts collapsed at every window size"
        );
        for size in [[1200.0, 850.0], [540.0, 620.0]] {
            for conversation in [false, true] {
                app.state.chats[0].messages = if conversation {
                    vec![storage::Message::user("Тестовая беседа".into())]
                } else {
                    vec![]
                };
                for _ in 0..2 {
                    let mut out = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(size[0], size[1]),
                            )),
                            ..Default::default()
                        },
                        |ui| app.render(ui),
                    );
                    out.textures_delta.clear();
                    let composer = out
                        .shapes
                        .iter()
                        .find_map(|shape| {
                            if let egui::Shape::Rect(rect) = &shape.shape
                                && rect.fill == palette::SURFACE_1
                                && rect.corner_radius == egui::CornerRadius::same(14)
                            {
                                Some(rect.rect)
                            } else {
                                None
                            }
                        })
                        .expect("visible composer");
                    assert!(
                        composer.left() >= 0.0 && composer.right() <= size[0],
                        "{composer:?}"
                    );
                    assert!(
                        composer.top() > 50.0 && composer.bottom() <= size[1],
                        "{composer:?}"
                    );
                    if size[0] > 700.0 {
                        assert!((composer.width() - 768.0).abs() < 4.0, "{composer:?}");
                    }
                    if conversation {
                        assert!(composer.bottom() > size[1] - 60.0);
                    } else {
                        assert!(
                            composer.center().y > size[1] * 0.4
                                && composer.center().y < size[1] * 0.75
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn all_views_render_at_desktop_and_narrow_sizes() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.state.chats[0].messages = vec![storage::Message::user(
            "Текст **жирный** и `код`.\n\n```rust\nfn main() {}\n```".into(),
        )];
        for size in [[1200.0, 850.0], [540.0, 620.0]] {
            for tab in [Tab::Chat, Tab::Models, Tab::Workspace] {
                app.tab = tab;
                app.sidebar = size[0] < 850.0;
                app.settings_open = true;
                app.attach_open = true;
                let mut out = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(size[0], size[1]),
                        )),
                        ..Default::default()
                    },
                    |ui| app.render(ui),
                );
                assert!(!out.shapes.is_empty());
                out.textures_delta.clear();
            }
        }
    }
    #[test]
    fn corrupt_state_recovers_without_overwriting_original() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{broken").unwrap();
        let (mut s, recovery, warning) = restore_state(dir.path());
        assert!(warning.is_some());
        s.chats.push(Chat::new());
        s.chats[0].title = "Сохранённый разговор".into();
        storage::save_json(&recovery, &s).unwrap();
        let (restored, new_path, _) = restore_state(dir.path());
        assert_eq!(new_path, recovery);
        assert_eq!(restored.chats[0].title, "Сохранённый разговор");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{broken");
    }
    #[test]
    fn right_panel_does_not_overlap_central_panel() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.state.chats[0].messages = vec![
            storage::Message::user("привет".into()),
            storage::Message {
                role: "assistant".into(),
                content: "Здравствуйте!".into(),
                reasoning: String::new(),
                state: String::new(),
                error: None,
                tps: Some(142.5),
                tokens: Some(12),
                duration_secs: Some(0.08),
                media: Vec::new(),
                agent_events: Vec::new(),
                subtasks: Vec::new(),
                changed_files: Vec::new(),
            },
        ];
        app.right_panel = true;
        let mut out = egui::FullOutput::default();
        for _ in 0..3 {
            out.textures_delta.clear();
            out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(942.0, 1026.0),
                    )),
                    ..Default::default()
                },
                |ui| app.render(ui),
            );
        }
        out.textures_delta.clear();
        let rp = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Rect(r)
                    if r.fill == palette::SURFACE_1
                        && r.rect.right() > 930.0
                        && r.rect.height() > 500.0 =>
                {
                    Some(r.rect)
                }
                _ => None,
            })
            .expect("right panel rect");
        let central = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Rect(r) if r.fill == palette::SURFACE_0 && r.rect.height() > 500.0 => {
                    Some(r.rect)
                }
                _ => None,
            })
            .expect("central panel rect");
        assert!(
            (rp.width() - 310.0).abs() <= 4.0 && central.right() <= rp.left() + 2.0,
            "rp={rp:?}, central={central:?}"
        );
    }
    #[test]
    fn kaic_refresh_keeps_earlier_turns_and_pending_approval() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, dir.path().to_path_buf());
        app.state.chats[0]
            .messages
            .push(storage::Message::user("Предыдущая задача".into()));
        app.state.chats[0].kaic_base = 1;
        let id = app.state.chats[0].id;
        let task:Task=serde_json::from_value(serde_json::json!({"id":"t","category":"coding","status":"waiting_for_human","context":[{"role":"assistant","content":"Нужен человек","at":"now"}],"pending_tool_approval":{"id":"a"}})).unwrap();
        app.apply_task(id, task.clone());
        app.apply_task(id, task);
        assert_eq!(app.state.chats[0].messages.len(), 2);
        assert_eq!(app.state.chats[0].messages[0].content, "Предыдущая задача");
        assert!(app.state.chats[0].status.contains("подтверждения"));
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    fn key(ctx: &egui::Context, app: &mut Studio, key: egui::Key) {
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events: vec![
                    egui::Event::ModifiersChanged(modifiers),
                    egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    },
                ],
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        out.textures_delta.clear();
    }
    #[test]
    fn keyboard_navigation_focus_and_busy_guard() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, d.path().to_path_buf());
        key(&ctx, &mut app, egui::Key::Num2);
        assert!(app.tab == Tab::Models);
        key(&ctx, &mut app, egui::Key::Num3);
        assert!(app.tab == Tab::Workspace);
        key(&ctx, &mut app, egui::Key::Num1);
        assert!(app.tab == Tab::Chat);
        key(&ctx, &mut app, egui::Key::Comma);
        assert!(app.settings_open);
        key(&ctx, &mut app, egui::Key::L);
        assert!(!app.settings_open);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::new("каик-композитор"))
        );
        let n = app.state.chats.len();
        app.busy = true;
        key(&ctx, &mut app, egui::Key::N);
        assert_eq!(app.state.chats.len(), n);
        app.busy = false;
        // Пустой диалог уже существует — повторный Ctrl+N не плодит дубликаты
        key(&ctx, &mut app, egui::Key::N);
        assert_eq!(app.state.chats.len(), n);
        // Как только в диалоге есть сообщение — Ctrl+N создаёт новый
        app.state.chats[0].messages = vec![storage::Message::user("Тест".into())];
        key(&ctx, &mut app, egui::Key::N);
        assert_eq!(app.state.chats.len(), n + 1);
        assert!(ctx.style_of(egui::Theme::Dark).text_styles[&egui::TextStyle::Small].size >= 12.0);
    }
    #[test]
    fn failure_and_retry_keep_partial_output_without_duplicate_user() {
        let d = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let ctx = egui::Context::default();
        let mut app = Studio::from_context(&ctx, d.path().to_path_buf());
        let p = d.path().join("test.gguf");
        std::fs::write(&p, b"GGUF").unwrap();
        let m:Model=serde_json::from_value(serde_json::json!({"repo":"fixture","filename":"test.gguf","path":p,"size":4,"sha256":"test","status":"verified"})).unwrap();
        app.models = vec![m];
        app.state.settings.selected_repo = "fixture".into();
        let id = app.state.chats[0].id;
        app.pending_chat = Some(id);
        app.state.chats[0].model = "fixture".into();
        app.state.chats[0].messages = vec![
            storage::Message::user("Запрос".into()),
            storage::Message::assistant(),
        ];
        app.state.chats[0].messages[1].content = "Частичный ответ".into();
        app.apply_chat_error("Проверяемая ошибка");
        assert_eq!(app.state.chats[0].messages[1].content, "Частичный ответ");
        assert!(app.state.chats[0].messages[1].error.is_some());
        // Shut executor before retry: inspect UI state without loading a real model/GPU.
        app.worker.shutdown();
        app.retry_response().unwrap();
        let messages = &app.state.chats[0].messages;
        assert_eq!(messages.iter().filter(|m| m.role == "user").count(), 1);
        assert_eq!(messages[1].content, "Частичный ответ");
        assert_eq!(messages[1].state, "предыдущая попытка");
        assert!(messages.last().unwrap().error.is_some());
    }
}
