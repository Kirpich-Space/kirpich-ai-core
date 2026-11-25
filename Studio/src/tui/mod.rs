//! Нативный inline-терминальный интерфейс KAIC (без полноэкранного alt-screen и без внешних TUI-крейтов).
//! Работает в основном буфере терминала с нативным scrollback (TrueColor, ANSI-16, SSH, голый TTY).

pub mod logo;
pub mod render;
pub mod term;

use crate::{
    agent::{
        AgentEvent, AgentLimits, AgentPolicy, AgentRunResult, AgentSeat, AgentStreamEvent,
        AnthropicApiBackend, ApprovalGate, ApprovalHandle, CodexCliConfig, CompactionSummary,
        ContextMetrics, ExecPolicy, ManagedServerBackend, OllamaBackend, OpenAiCompatBackend,
        PendingApproval, ProviderKind, SubtaskRecord, context, probe_providers,
        run_agent_with_policies, run_agy_cli, run_claude_cli, run_codex_cli,
    },
    catalog, desktop,
    kaic::Secret,
    llama::ManagedServer,
    network::Cancel,
    storage::{self, Chat, Message, SavedState},
};
use anyhow::{Context, Result, bail};
use logo::LogoProtocol;
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use term::{
    ColorMode, InlineViewport, InputParser, KeyEvent, RawTerminalGuard, is_tty,
    read_stdin_nonblocking, take_resize_flag, terminal_size, wait_stdin_ready,
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct TuiConfig {
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub provider: ProviderKind,
    pub model: String,
    pub endpoint: String,
    pub policy: AgentPolicy,
    pub exec_policy: ExecPolicy,
    pub allow_external: bool,
    /// По умолчанию `false`: динамическое взаимодействие включается только явно пользователем.
    pub dynamic_interaction: bool,
    pub resume_session: Option<String>,
    pub color_override: Option<ColorMode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolEntry {
    pub id: String,
    pub name: String,
    pub ok: Option<bool>,
    pub summary: String,
    pub output_preview: String,
    pub diff_preview: Option<String>,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub enum TranscriptItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
        metrics: Option<(u32, f32, f32)>,
    },
    Tool(ToolEntry),
    Subtask(SubtaskRecord),
    Notice {
        text: String,
        is_error: bool,
    },
}

/// Элемент интерактивного меню выбора модели и провайдера (`/model` или `Ctrl+P`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: ProviderKind,
    pub model: String,
    pub locality_local: bool,
    pub badge_ru: String,
    pub detail_ru: String,
}

/// Активное интерактивное меню над/под композером.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ActiveMenu {
    #[default]
    None,
    ModelPicker {
        filter: String,
        cursor: usize,
        items: Vec<ModelOption>,
    },
}

pub fn filtered_model_indices(items: &[ModelOption], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return (0..items.len()).collect();
    }
    items
        .iter()
        .enumerate()
        .filter(|(_, opt)| {
            opt.model.to_lowercase().contains(&needle)
                || opt.provider.short_label().to_lowercase().contains(&needle)
                || opt.badge_ru.to_lowercase().contains(&needle)
                || opt.detail_ru.to_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiAction {
    CommitSlice {
        start: usize,
        end: usize,
    },
    ClearScreen,
    RunPrompt(String),
    CancelRun,
    RespondApproval {
        request_id: String,
        action_digest: String,
        approved: bool,
    },
    Redraw,
    Quit,
}

pub struct SlashDef {
    pub cmd: &'static str,
    pub args: &'static str,
    pub desc: &'static str,
}

pub const SLASH_COMMANDS: &[SlashDef] = &[
    SlashDef {
        cmd: "/model",
        args: "[имя]",
        desc: "Интерактивный выбор модели и провайдера (Ctrl+P)",
    },
    SlashDef {
        cmd: "/provider",
        args: "[local|ollama|openai|anthropic|codex|claude|agy]",
        desc: "Переключение провайдера или диагностика среды",
    },
    SlashDef {
        cmd: "/theme",
        args: "[carbon|bone]",
        desc: "Официальная тема палитры (Carbon или Bone)",
    },
    SlashDef {
        cmd: "/policy",
        args: "[read-only|approval-required|workspace-write]",
        desc: "Режим доступа к файлам рабочей области",
    },
    SlashDef {
        cmd: "/exec",
        args: "[disabled|safe-inspect|confirm-user-code|allow-builds]",
        desc: "Режим запуска проверок и сборок",
    },
    SlashDef {
        cmd: "/compact",
        args: "",
        desc: "Сжать историю сообщений для экономии контекста",
    },
    SlashDef {
        cmd: "/sessions",
        args: "[list|resume <id>|new]",
        desc: "Список сохранённых сессий и возобновление",
    },
    SlashDef {
        cmd: "/toggle",
        args: "[all|last]",
        desc: "Свернуть или развернуть блоки инструментов (Ctrl+O)",
    },
    SlashDef {
        cmd: "/skills",
        args: "[list|use <id>|clear]",
        desc: "Подключение локальных скиллов проекта",
    },
    SlashDef {
        cmd: "/endpoint",
        args: "[url]",
        desc: "Адрес сервера для Ollama / OpenAI / Anthropic",
    },
    SlashDef {
        cmd: "/consent",
        args: "[on|off]",
        desc: "Явное разрешение передачи контекста внешнему провайдеру",
    },
    SlashDef {
        cmd: "/dynamic",
        args: "[on|off|stop]",
        desc: "Динамическое взаимодействие с местами (по умолчанию выкл.)",
    },
    SlashDef {
        cmd: "/budget",
        args: "[steps <n>|wall <сек>]",
        desc: "Лимиты числа шагов и времени хода",
    },
    SlashDef {
        cmd: "/clear",
        args: "",
        desc: "Очистить видимую область терминала (Ctrl+L)",
    },
    SlashDef {
        cmd: "/help",
        args: "",
        desc: "Справка по командам и сочетаниям клавиш",
    },
    SlashDef {
        cmd: "/quit",
        args: "",
        desc: "Сохранить сессию и завершить работу",
    },
    SlashDef {
        cmd: "/exit",
        args: "",
        desc: "Синоним /quit",
    },
];

pub fn matching_slash_commands(prefix: &str) -> Vec<usize> {
    let head = prefix.split_whitespace().next().unwrap_or("");
    SLASH_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, def)| def.cmd.starts_with(head))
        .map(|(i, _)| i)
        .collect()
}

pub struct TuiState {
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub provider: ProviderKind,
    pub model: String,
    pub endpoint: String,
    pub policy: AgentPolicy,
    pub exec_policy: ExecPolicy,
    pub allow_external: bool,
    pub dynamic_interaction_enabled: bool,
    pub limits: AgentLimits,
    pub active_skills: Vec<String>,
    pub saved_state: SavedState,
    pub session_idx: usize,
    pub transcript: Vec<TranscriptItem>,
    pub turn_start_idx: usize,
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    pub history_draft: String,
    pub autocomplete_idx: usize,
    pub slash_menu_dismissed: bool,
    pub active_menu: ActiveMenu,
    pub scroll_from_bottom: usize,
    pub context_metrics: Option<ContextMetrics>,
    pub pending_approval: Option<PendingApproval>,
    pub busy: bool,
    pub anim_tick: usize,
    pub run_started_at: Option<Instant>,
    pub should_quit: bool,
    pub status_line: String,
    pub last_seat: Option<AgentSeat>,
    pub theme: term::TuiTheme,
}

impl TuiState {
    pub fn load(cfg: &TuiConfig) -> Self {
        let state_path = cfg.data_dir.join("state.json");
        let mut saved_state: SavedState = storage::load_json(&state_path).unwrap_or_default();
        let history_path = cfg.data_dir.join("tui-history.json");
        let history: Vec<String> = storage::load_json(&history_path).unwrap_or_default();

        let session_idx = if let Some(prefix) = &cfg.resume_session {
            if let Some(pos) = saved_state
                .chats
                .iter()
                .position(|c| c.id.to_string().starts_with(prefix))
            {
                pos
            } else {
                saved_state.chats.push(new_tui_chat(cfg));
                saved_state.chats.len() - 1
            }
        } else {
            saved_state.chats.push(new_tui_chat(cfg));
            saved_state.chats.len() - 1
        };

        let mut limits = AgentLimits::default();
        limits.dynamic_interaction_enabled = cfg.dynamic_interaction;

        let mut st = Self {
            data_dir: cfg.data_dir.clone(),
            workspace: cfg.workspace.clone(),
            provider: cfg.provider,
            model: cfg.model.clone(),
            endpoint: cfg.endpoint.clone(),
            policy: cfg.policy,
            exec_policy: cfg.exec_policy,
            allow_external: cfg.allow_external,
            dynamic_interaction_enabled: cfg.dynamic_interaction,
            limits,
            active_skills: Vec::new(),
            saved_state,
            session_idx,
            transcript: Vec::new(),
            turn_start_idx: 0,
            input: String::new(),
            cursor: 0,
            history,
            history_pos: None,
            history_draft: String::new(),
            autocomplete_idx: 0,
            slash_menu_dismissed: false,
            active_menu: ActiveMenu::None,
            scroll_from_bottom: 0,
            context_metrics: None,
            pending_approval: None,
            busy: false,
            anim_tick: 0,
            run_started_at: None,
            should_quit: false,
            status_line: String::new(),
            last_seat: None,
            theme: term::TuiTheme::Carbon,
        };
        st.rebuild_transcript_from_active_chat();
        st
    }

    pub fn elapsed_run_secs(&self) -> f32 {
        self.run_started_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    pub fn is_slash_menu_open(&self) -> bool {
        self.active_menu == ActiveMenu::None
            && !self.slash_menu_dismissed
            && self.input.starts_with('/')
            && !self.input.contains('\n')
            && !self.input.contains(' ')
            && !matching_slash_commands(&self.input).is_empty()
    }

    pub fn open_model_picker(&mut self) {
        let items = self.build_model_options();
        let current_idx = items
            .iter()
            .position(|o| o.provider == self.provider && o.model == self.model)
            .unwrap_or(0);
        self.active_menu = ActiveMenu::ModelPicker {
            filter: String::new(),
            cursor: current_idx,
            items,
        };
    }

    fn build_model_options(&self) -> Vec<ModelOption> {
        let mut items: Vec<ModelOption> = Vec::new();
        let caps = probe_providers();
        let status_for = |kind: ProviderKind| -> String {
            caps.iter()
                .find(|c| c.provider == kind)
                .map(|c| {
                    if c.available {
                        match kind {
                            ProviderKind::CodexCli
                            | ProviderKind::ClaudeCli
                            | ProviderKind::AgyCli => "CLI найден · требуется /consent on",
                            ProviderKind::AnthropicApi | ProviderKind::OpenAiCompat => {
                                "готов · требуется /consent on"
                            }
                            _ => "готов к запуску",
                        }
                    } else {
                        match kind {
                            ProviderKind::CodexCli
                            | ProviderKind::ClaudeCli
                            | ProviderKind::AgyCli => "CLI не найден в PATH",
                            ProviderKind::AnthropicApi | ProviderKind::OpenAiCompat => {
                                "нужен ключ API в окружении"
                            }
                            _ => "недоступен",
                        }
                    }
                    .to_string()
                })
                .unwrap_or_else(|| "доступно".to_string())
        };

        // 1. Локальные проверенные GGUF из каталога моделей (без запуска llama-server и без касания GPU!)
        let (gguf_models, _) = catalog::discover(Path::new(catalog::MODEL_ROOT));
        let mut any_gguf = false;
        for m in gguf_models
            .iter()
            .filter(|m| m.loadable() && m.kind() == catalog::ModelKind::Text)
        {
            any_gguf = true;
            items.push(ModelOption {
                provider: ProviderKind::LocalLlama,
                model: m.alias().to_string(),
                locality_local: true,
                badge_ru: "Локально · GGUF".into(),
                detail_ru: format!("{} · данные на этом ПК", m.filename),
            });
        }
        if !any_gguf {
            items.push(ModelOption {
                provider: ProviderKind::LocalLlama,
                model: "gemma26".into(),
                locality_local: true,
                badge_ru: "Локально · GGUF".into(),
                detail_ru: "локальный каталог GGUF · данные на этом ПК".into(),
            });
        }

        // 2. Локальные модели Ollama (127.0.0.1)
        for ollama_m in ["qwen2.5-coder:14b", "qwen2.5-coder:7b", "llama3.3:latest"] {
            items.push(ModelOption {
                provider: ProviderKind::OllamaLocal,
                model: ollama_m.into(),
                locality_local: true,
                badge_ru: "Локально · Ollama".into(),
                detail_ru: "127.0.0.1 · данные на этом ПК".into(),
            });
        }

        // 3. Внешние CLI-агенты и облачные API
        items.push(ModelOption {
            provider: ProviderKind::ClaudeCli,
            model: "claude-code".into(),
            locality_local: false,
            badge_ru: "Внешний CLI".into(),
            detail_ru: status_for(ProviderKind::ClaudeCli),
        });
        items.push(ModelOption {
            provider: ProviderKind::CodexCli,
            model: "codex".into(),
            locality_local: false,
            badge_ru: "Внешний CLI".into(),
            detail_ru: status_for(ProviderKind::CodexCli),
        });
        items.push(ModelOption {
            provider: ProviderKind::AgyCli,
            model: "agy".into(),
            locality_local: false,
            badge_ru: "Внешний CLI".into(),
            detail_ru: status_for(ProviderKind::AgyCli),
        });
        items.push(ModelOption {
            provider: ProviderKind::AnthropicApi,
            model: "claude-sonnet-4-5".into(),
            locality_local: false,
            badge_ru: "Anthropic API".into(),
            detail_ru: status_for(ProviderKind::AnthropicApi),
        });
        items.push(ModelOption {
            provider: ProviderKind::OpenAiCompat,
            model: "gpt-4.1".into(),
            locality_local: false,
            badge_ru: "OpenAI-совм. API".into(),
            detail_ru: status_for(ProviderKind::OpenAiCompat),
        });

        // Если активная пара (provider, model) кастомная — гарантируем её присутствие в начале списка
        if !items
            .iter()
            .any(|o| o.provider == self.provider && o.model == self.model)
        {
            let local_now = matches!(
                self.provider,
                ProviderKind::LocalLlama | ProviderKind::OllamaLocal
            );
            items.insert(
                0,
                ModelOption {
                    provider: self.provider,
                    model: self.model.clone(),
                    locality_local: local_now,
                    badge_ru: self.provider.label_ru().to_string(),
                    detail_ru: "текущая конфигурация сессии".into(),
                },
            );
        }

        items
    }

    pub fn active_chat(&self) -> &Chat {
        &self.saved_state.chats[self.session_idx]
    }

    pub fn active_chat_mut(&mut self) -> &mut Chat {
        &mut self.saved_state.chats[self.session_idx]
    }

    fn rebuild_transcript_from_active_chat(&mut self) {
        self.transcript.clear();
        self.turn_start_idx = 0;
        let chat = self.saved_state.chats[self.session_idx].clone();
        if !chat.options.workspace_path.is_empty()
            && Path::new(&chat.options.workspace_path).is_dir()
        {
            self.workspace = PathBuf::from(&chat.options.workspace_path);
        }
        for msg in &chat.messages {
            match msg.role.as_str() {
                "user" => self.transcript.push(TranscriptItem::User {
                    text: msg.content.clone(),
                }),
                "assistant" => {
                    for ev in &msg.agent_events {
                        if let AgentEvent::ToolResult {
                            id,
                            name,
                            ok,
                            summary,
                            output_preview,
                            diff_preview,
                            ..
                        } = ev
                        {
                            self.transcript.push(TranscriptItem::Tool(ToolEntry {
                                id: id.clone(),
                                name: name.clone(),
                                ok: Some(*ok),
                                summary: summary.clone(),
                                output_preview: output_preview.clone(),
                                diff_preview: diff_preview.clone(),
                                expanded: false,
                            }));
                        }
                    }
                    if !msg.content.is_empty() {
                        self.transcript.push(TranscriptItem::Assistant {
                            text: msg.content.clone(),
                            streaming: false,
                            metrics: match (msg.tokens, msg.tps, msg.duration_secs) {
                                (Some(t), Some(tps), Some(d)) => Some((t, tps, d)),
                                _ => None,
                            },
                        });
                    }
                }
                _ => {}
            }
        }
    }

    pub fn persist(&mut self) {
        {
            let opts = &mut self.saved_state.chats[self.session_idx].options;
            opts.agent_enabled = true;
            opts.workspace_path = self.workspace.display().to_string();
            opts.provider = self.provider;
            opts.provider_endpoint = self.endpoint.clone();
            opts.provider_model = self.model.clone();
            opts.agent_policy = self.policy;
            opts.exec_policy = self.exec_policy;
            opts.external_provider_consent = self.allow_external;
            opts.dynamic_interaction_enabled = self.dynamic_interaction_enabled;
        }
        let state_path = self.data_dir.join("state.json");
        let _ = storage::save_json(&state_path, &self.saved_state);
        if self.history.len() > 500 {
            let drop_n = self.history.len() - 500;
            self.history.drain(..drop_n);
        }
        let hist_path = self.data_dir.join("tui-history.json");
        let _ = storage::save_json(&hist_path, &self.history);
    }

    pub fn push_notice(&mut self, text: impl Into<String>, is_error: bool) {
        self.transcript.push(TranscriptItem::Notice {
            text: text.into(),
            is_error,
        });
        self.scroll_from_bottom = 0;
    }

    pub fn apply_stream_event(&mut self, ev: AgentStreamEvent) {
        match ev {
            AgentStreamEvent::Status(s) => self.status_line = s,
            AgentStreamEvent::TextDelta(delta) => {
                if let Some(TranscriptItem::Assistant {
                    text,
                    streaming: true,
                    ..
                }) = self.transcript.last_mut()
                {
                    text.push_str(&delta);
                } else {
                    self.transcript.push(TranscriptItem::Assistant {
                        text: delta,
                        streaming: true,
                        metrics: None,
                    });
                }
                self.scroll_from_bottom = 0;
            }
            AgentStreamEvent::ContextUsage(m) => {
                self.context_metrics = Some(m);
            }
            AgentStreamEvent::ApprovalNeeded(req) => {
                self.pending_approval = Some(req);
                self.status_line = "Ожидается подтверждение действия".into();
            }
            AgentStreamEvent::Subtask(st) => {
                if let Some(TranscriptItem::Subtask(existing)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, TranscriptItem::Subtask(x) if x.id == st.id))
                {
                    *existing = st;
                } else {
                    self.transcript.push(TranscriptItem::Subtask(st));
                }
                self.scroll_from_bottom = 0;
            }
            AgentStreamEvent::SeatUpdate(seat) => {
                self.last_seat = Some(seat);
            }
            AgentStreamEvent::Metrics {
                tokens,
                tps,
                seconds,
            } => {
                if let Some(TranscriptItem::Assistant { metrics, .. }) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, TranscriptItem::Assistant { .. }))
                {
                    *metrics = Some((tokens, tps, seconds));
                }
            }
            AgentStreamEvent::Event(ae) => self.apply_agent_event(ae),
        }
    }

    fn apply_agent_event(&mut self, ae: AgentEvent) {
        if let Some(msg) = self.saved_state.chats[self.session_idx].messages.last_mut()
            && msg.role == "assistant"
        {
            if let AgentEvent::ToolResult {
                ref changed_files, ..
            } = ae
            {
                for f in changed_files {
                    if !msg.changed_files.iter().any(|x| x == f) {
                        msg.changed_files.push(f.clone());
                    }
                }
            }
            msg.agent_events.push(ae.clone());
        }

        match ae {
            AgentEvent::Step { summary, .. } => {
                self.status_line = summary;
            }
            AgentEvent::ToolCall {
                id,
                name,
                args_summary,
                ..
            } => {
                self.status_line = format!("Вызов {name}…");
                self.transcript.push(TranscriptItem::Tool(ToolEntry {
                    id,
                    name,
                    ok: None,
                    summary: args_summary,
                    output_preview: String::new(),
                    diff_preview: None,
                    expanded: false,
                }));
                self.scroll_from_bottom = 0;
            }
            AgentEvent::ToolResult {
                id,
                name,
                ok,
                summary,
                output_preview,
                diff_preview,
                ..
            } => {
                let auto_expand = !ok || diff_preview.is_some();
                if let Some(TranscriptItem::Tool(entry)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, TranscriptItem::Tool(t) if t.id == id))
                {
                    entry.ok = Some(ok);
                    entry.summary = summary;
                    entry.output_preview = output_preview;
                    entry.diff_preview = diff_preview;
                    entry.expanded = auto_expand;
                } else {
                    self.transcript.push(TranscriptItem::Tool(ToolEntry {
                        id,
                        name,
                        ok: Some(ok),
                        summary,
                        output_preview,
                        diff_preview,
                        expanded: auto_expand,
                    }));
                }
                self.scroll_from_bottom = 0;
            }
            AgentEvent::ApprovalResolved { .. } => {
                self.pending_approval = None;
            }
            AgentEvent::Blocked { reason } => {
                self.push_notice(reason, true);
            }
            AgentEvent::Cancelled { summary, .. } => {
                self.push_notice(summary, false);
            }
            AgentEvent::Completed { .. } => {
                for item in self.transcript.iter_mut().rev() {
                    if let TranscriptItem::Assistant { streaming, .. } = item {
                        *streaming = false;
                        break;
                    }
                }
            }
            AgentEvent::ApprovalRequested { .. } => {}
        }
    }

    pub fn finish_run(&mut self, res: Result<AgentRunResult>) {
        self.busy = false;
        self.run_started_at = None;
        self.pending_approval = None;
        for item in self.transcript.iter_mut().rev() {
            if let TranscriptItem::Assistant { streaming, .. } = item {
                *streaming = false;
            }
        }
        match res {
            Ok(run) => {
                let has_assistant_after_user = self
                    .transcript
                    .iter()
                    .rev()
                    .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
                    .any(|i| matches!(i, TranscriptItem::Assistant { .. }));
                if !has_assistant_after_user && !run.final_text.trim().is_empty() {
                    self.transcript.push(TranscriptItem::Assistant {
                        text: run.final_text.clone(),
                        streaming: false,
                        metrics: None,
                    });
                }
                if let Some(msg) = self.saved_state.chats[self.session_idx].messages.last_mut()
                    && msg.role == "assistant"
                {
                    if msg.content.trim().is_empty() {
                        msg.content = run.final_text.clone();
                    }
                    msg.state = if run.cancelled {
                        "остановлено".into()
                    } else if run.blocked {
                        "блокер".into()
                    } else {
                        "готов".into()
                    };
                    msg.changed_files = run.changed_files.clone();
                }
                self.status_line = if run.cancelled {
                    "Остановлено пользователем".into()
                } else if run.blocked {
                    "Остановлено ограничением безопасности".into()
                } else if run.changed_files.is_empty() {
                    String::new()
                } else {
                    format!("Изменено файлов: {}", run.changed_files.len())
                };
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.push_notice(&msg, true);
                if let Some(last) = self.saved_state.chats[self.session_idx].messages.last_mut()
                    && last.role == "assistant"
                {
                    last.error = Some(msg.clone());
                    last.state = "ошибка".into();
                }
                self.status_line = "Ошибка выполнения".into();
            }
        }
        self.persist();
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> Vec<TuiAction> {
        let ActiveMenu::ModelPicker {
            ref mut filter,
            ref mut cursor,
            ref items,
        } = self.active_menu
        else {
            return Vec::new();
        };

        match key {
            KeyEvent::Esc | KeyEvent::Ctrl('c') => {
                self.active_menu = ActiveMenu::None;
                vec![TuiAction::Redraw]
            }
            KeyEvent::Up | KeyEvent::BackTab => {
                let indices = filtered_model_indices(items, filter);
                if !indices.is_empty() {
                    *cursor = if *cursor == 0 {
                        indices.len() - 1
                    } else {
                        (*cursor - 1).min(indices.len() - 1)
                    };
                }
                vec![TuiAction::Redraw]
            }
            KeyEvent::Down | KeyEvent::Tab => {
                let indices = filtered_model_indices(items, filter);
                if !indices.is_empty() {
                    *cursor = (*cursor + 1) % indices.len();
                }
                vec![TuiAction::Redraw]
            }
            KeyEvent::Backspace => {
                filter.pop();
                *cursor = 0;
                vec![TuiAction::Redraw]
            }
            KeyEvent::Ctrl('u') => {
                filter.clear();
                *cursor = 0;
                vec![TuiAction::Redraw]
            }
            KeyEvent::Char(ch) if !ch.is_control() => {
                filter.push(ch);
                *cursor = 0;
                vec![TuiAction::Redraw]
            }
            KeyEvent::Enter => {
                let indices = filtered_model_indices(items, filter);
                if let Some(&chosen_idx) =
                    indices.get((*cursor).min(indices.len().saturating_sub(1)))
                {
                    let chosen = items[chosen_idx].clone();
                    self.active_menu = ActiveMenu::None;
                    self.provider = chosen.provider;
                    self.model = chosen.model.clone();
                    if self.endpoint.is_empty() {
                        if chosen.provider == ProviderKind::OllamaLocal {
                            self.endpoint = "http://127.0.0.1:11434".into();
                        } else if chosen.provider == ProviderKind::AnthropicApi {
                            self.endpoint = "https://api.anthropic.com".into();
                        }
                    }
                    self.persist();
                    self.push_notice(
                        format!("Активна модель: {} ({})", chosen.model, chosen.badge_ru),
                        false,
                    );
                } else {
                    self.active_menu = ActiveMenu::None;
                }
                vec![TuiAction::Redraw]
            }
            _ => Vec::new(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<TuiAction> {
        let mut actions = Vec::new();

        // 1. Модальное окно HITL-подтверждения: пустой Enter НИКОГДА не подтверждает!
        if let Some(req) = self.pending_approval.clone() {
            match key {
                KeyEvent::Char('y') | KeyEvent::Char('Y') => {
                    self.pending_approval = None;
                    actions.push(TuiAction::RespondApproval {
                        request_id: req.request_id,
                        action_digest: req.action_digest,
                        approved: true,
                    });
                    actions.push(TuiAction::Redraw);
                }
                KeyEvent::Char('n') | KeyEvent::Char('N') => {
                    self.pending_approval = None;
                    actions.push(TuiAction::RespondApproval {
                        request_id: req.request_id,
                        action_digest: req.action_digest,
                        approved: false,
                    });
                    actions.push(TuiAction::Redraw);
                }
                KeyEvent::Esc | KeyEvent::Ctrl('c') => {
                    self.pending_approval = None;
                    self.emergency_stop_dynamic();
                    actions.push(TuiAction::RespondApproval {
                        request_id: req.request_id,
                        action_digest: req.action_digest,
                        approved: false,
                    });
                    actions.push(TuiAction::CancelRun);
                    actions.push(TuiAction::Redraw);
                }
                _ => {}
            }
            return actions;
        }

        // 2. Интерактивное меню выбора модели (/model или Ctrl+P)
        if matches!(self.active_menu, ActiveMenu::ModelPicker { .. }) {
            return self.handle_model_picker_key(key);
        }

        match key {
            KeyEvent::Esc => {
                if self.busy || self.dynamic_interaction_enabled {
                    self.emergency_stop_dynamic();
                    if self.busy {
                        actions.push(TuiAction::CancelRun);
                        self.status_line = "Остановка по Esc…".into();
                    } else {
                        self.push_notice("Динамическое управление остановлено (Esc).", false);
                    }
                    actions.push(TuiAction::Redraw);
                } else if self.is_slash_menu_open() {
                    self.slash_menu_dismissed = true;
                    actions.push(TuiAction::Redraw);
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                    self.autocomplete_idx = 0;
                    self.slash_menu_dismissed = false;
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Ctrl('p') => {
                if !self.busy {
                    self.open_model_picker();
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Ctrl('c') => {
                if self.busy {
                    self.emergency_stop_dynamic();
                    actions.push(TuiAction::CancelRun);
                    actions.push(TuiAction::Redraw);
                } else {
                    self.persist();
                    self.should_quit = true;
                    actions.push(TuiAction::Quit);
                }
            }
            KeyEvent::Ctrl('d') => {
                if self.input.is_empty() && !self.busy {
                    self.persist();
                    self.should_quit = true;
                    actions.push(TuiAction::Quit);
                }
            }
            KeyEvent::Ctrl('o') => {
                if let Some(TranscriptItem::Tool(entry)) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, TranscriptItem::Tool(_)))
                {
                    entry.expanded = !entry.expanded;
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Ctrl('t') => {
                self.toggle_all_tools();
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Ctrl('l') => {
                self.transcript.clear();
                self.turn_start_idx = 0;
                self.scroll_from_bottom = 0;
                actions.push(TuiAction::ClearScreen);
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Ctrl('u') => {
                self.input.drain(..self.cursor);
                self.cursor = 0;
                self.autocomplete_idx = 0;
                self.slash_menu_dismissed = false;
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Ctrl('a') | KeyEvent::Home => {
                let line_start = self.input[..self.cursor]
                    .rfind('\n')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                self.cursor = line_start;
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Ctrl('e') | KeyEvent::End => {
                let line_end = self.input[self.cursor..]
                    .find('\n')
                    .map(|rel| self.cursor + rel)
                    .unwrap_or(self.input.len());
                self.cursor = line_end;
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(8);
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8);
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Newline | KeyEvent::Ctrl('j') => {
                self.insert_str("\n");
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Paste(text) => {
                self.insert_str(&text);
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Char(ch) => {
                let mut buf = [0u8; 4];
                self.insert_str(ch.encode_utf8(&mut buf));
                self.autocomplete_idx = 0;
                self.slash_menu_dismissed = false;
                actions.push(TuiAction::Redraw);
            }
            KeyEvent::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.input, self.cursor);
                    self.input.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.autocomplete_idx = 0;
                    self.slash_menu_dismissed = false;
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Delete => {
                if self.cursor < self.input.len() {
                    let next = next_char_boundary(&self.input, self.cursor);
                    self.input.drain(self.cursor..next);
                    self.autocomplete_idx = 0;
                    self.slash_menu_dismissed = false;
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.input, self.cursor);
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Right => {
                if self.cursor < self.input.len() {
                    self.cursor = next_char_boundary(&self.input, self.cursor);
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Tab => {
                if self.input.starts_with('/') && !self.input.contains('\n') {
                    let matches = matching_slash_commands(&self.input);
                    if !matches.is_empty() {
                        let chosen =
                            &SLASH_COMMANDS[matches[self.autocomplete_idx % matches.len()]];
                        self.input = if chosen.args.is_empty() {
                            chosen.cmd.to_string()
                        } else {
                            format!("{} ", chosen.cmd)
                        };
                        self.cursor = self.input.len();
                        self.autocomplete_idx = 0;
                        self.slash_menu_dismissed = false;
                        actions.push(TuiAction::Redraw);
                    }
                }
            }
            KeyEvent::BackTab => {
                if self.is_slash_menu_open() {
                    let matches = matching_slash_commands(&self.input);
                    if !matches.is_empty() {
                        self.autocomplete_idx = if self.autocomplete_idx == 0 {
                            matches.len() - 1
                        } else {
                            self.autocomplete_idx - 1
                        };
                        actions.push(TuiAction::Redraw);
                    }
                }
            }
            KeyEvent::Up => {
                if self.is_slash_menu_open() {
                    let matches = matching_slash_commands(&self.input);
                    if !matches.is_empty() {
                        self.autocomplete_idx = if self.autocomplete_idx == 0 {
                            matches.len() - 1
                        } else {
                            self.autocomplete_idx - 1
                        };
                        actions.push(TuiAction::Redraw);
                        return actions;
                    }
                }
                if !self.input.contains('\n') {
                    self.history_prev();
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Down => {
                if self.is_slash_menu_open() {
                    let matches = matching_slash_commands(&self.input);
                    if !matches.is_empty() {
                        self.autocomplete_idx = (self.autocomplete_idx + 1) % matches.len();
                        actions.push(TuiAction::Redraw);
                        return actions;
                    }
                }
                if !self.input.contains('\n') {
                    self.history_next();
                    actions.push(TuiAction::Redraw);
                }
            }
            KeyEvent::Enter => {
                // Если открыто интерактивное меню slash-команд (без аргументов после пробела),
                // Enter сразу запускает выбранную в меню команду!
                let resolved_slash = if self.is_slash_menu_open() {
                    let matches = matching_slash_commands(&self.input);
                    matches
                        .get(self.autocomplete_idx % matches.len())
                        .map(|&idx| SLASH_COMMANDS[idx].cmd.to_string())
                } else {
                    None
                };

                let submitted = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.history_pos = None;
                self.autocomplete_idx = 0;
                self.slash_menu_dismissed = false;

                let effective = resolved_slash
                    .as_deref()
                    .unwrap_or_else(|| submitted.trim());
                if effective.is_empty() {
                    return actions;
                }
                if self.history.last().is_none_or(|last| last != effective) {
                    self.history.push(effective.to_string());
                }
                if effective.starts_with('/') {
                    self.execute_slash_command(effective, &mut actions);
                } else if self.busy {
                    self.push_notice(
                        "Агент выполняет задачу. Нажмите Esc для остановки перед новым запросом.",
                        true,
                    );
                } else {
                    if self.transcript.len() > self.turn_start_idx {
                        let prev_end = self.transcript.len();
                        actions.push(TuiAction::CommitSlice {
                            start: self.turn_start_idx,
                            end: prev_end,
                        });
                        self.turn_start_idx = prev_end;
                    }
                    self.prepare_prompt_turn(effective.to_string());
                    actions.push(TuiAction::RunPrompt(effective.to_string()));
                }
                actions.push(TuiAction::Redraw);
            }
            _ => {}
        }

        actions
    }

    fn insert_str(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_pos = match self.history_pos {
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next_pos);
        self.input = self.history[next_pos].clone();
        self.cursor = self.input.len();
        self.slash_menu_dismissed = true;
    }

    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            let next = pos + 1;
            self.history_pos = Some(next);
            self.input = self.history[next].clone();
            self.slash_menu_dismissed = true;
        } else {
            self.history_pos = None;
            self.input = std::mem::take(&mut self.history_draft);
            self.slash_menu_dismissed = false;
        }
        self.cursor = self.input.len();
    }

    fn emergency_stop_dynamic(&mut self) {
        self.dynamic_interaction_enabled = false;
        self.limits.dynamic_interaction_enabled = false;
    }

    fn toggle_all_tools(&mut self) {
        let any_collapsed = self
            .transcript
            .iter()
            .any(|i| matches!(i, TranscriptItem::Tool(t) if !t.expanded));
        for item in &mut self.transcript {
            if let TranscriptItem::Tool(t) = item {
                t.expanded = any_collapsed;
            }
        }
    }

    pub fn prepare_prompt_turn(&mut self, prompt: String) {
        self.busy = true;
        self.anim_tick = 0;
        self.run_started_at = Some(Instant::now());
        self.scroll_from_bottom = 0;
        self.status_line = "Подготовка запроса…".into();
        self.transcript.push(TranscriptItem::User {
            text: prompt.clone(),
        });
        let chat = &mut self.saved_state.chats[self.session_idx];
        chat.updated_at = storage::epoch_now();
        if chat.title == "Новый разговор" && !prompt.trim().is_empty() {
            chat.title = prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect();
        }
        chat.messages.push(Message::user(prompt));
        chat.messages.push(Message::assistant());
        self.persist();
    }

    fn execute_slash_command(&mut self, raw: &str, actions: &mut Vec<TuiAction>) {
        let mut parts = raw.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg1 = parts.next();
        let arg2 = parts.next();

        match cmd {
            "/help" => {
                let mut lines = String::from("Команды терминального интерфейса:\n");
                for d in SLASH_COMMANDS {
                    if d.args.is_empty() {
                        lines.push_str(&format!("  {:12} — {}\n", d.cmd, d.desc));
                    } else {
                        lines.push_str(&format!("  {} {} — {}\n", d.cmd, d.args, d.desc));
                    }
                }
                lines.push_str(
                    "Сочетания клавиш: Enter отправить · Ctrl+J / Shift+Enter перенос строки · Ctrl+P выбор модели · Ctrl+O детали инструмента · Ctrl+T все инструменты · Esc остановить",
                );
                self.push_notice(lines, false);
            }
            "/quit" | "/exit" => {
                self.persist();
                self.should_quit = true;
                actions.push(TuiAction::Quit);
            }
            "/clear" => {
                self.transcript.clear();
                self.turn_start_idx = 0;
                self.scroll_from_bottom = 0;
                self.status_line.clear();
                actions.push(TuiAction::ClearScreen);
            }
            "/provider" => {
                if let Some(p) = arg1 {
                    if p == "list" {
                        let caps = probe_providers();
                        let mut report = String::from("Состояние провайдеров:\n");
                        for c in caps {
                            report.push_str(&format!(
                                "  {:10} · {} · {}\n",
                                c.provider.short_label(),
                                c.status.label_ru(),
                                c.label
                            ));
                        }
                        self.push_notice(report.trim_end().to_string(), false);
                    } else {
                        match ProviderKind::parse_str(p) {
                            Ok(kind) => {
                                self.provider = kind;
                                if self.endpoint.is_empty() {
                                    if kind == ProviderKind::OllamaLocal {
                                        self.endpoint = "http://127.0.0.1:11434".into();
                                    } else if kind == ProviderKind::AnthropicApi {
                                        self.endpoint = "https://api.anthropic.com".into();
                                    }
                                }
                                self.persist();
                                self.push_notice(
                                    format!(
                                        "Провайдер установлен: {} ({})",
                                        kind.label_ru(),
                                        kind.execution_origin().label_ru()
                                    ),
                                    false,
                                );
                            }
                            Err(e) => self.push_notice(format!("{e}"), true),
                        }
                    }
                } else {
                    self.open_model_picker();
                }
            }
            "/model" => {
                if let Some(m) = arg1 {
                    self.model = m.to_string();
                    self.persist();
                    self.push_notice(format!("Модель установлена: {}", self.model), false);
                } else {
                    self.open_model_picker();
                }
            }
            "/endpoint" => {
                if let Some(url) = arg1 {
                    self.endpoint = url.to_string();
                    self.persist();
                    self.push_notice(format!("Адрес сервера: {}", self.endpoint), false);
                } else {
                    self.push_notice(
                        format!(
                            "Текущий адрес сервера: {}",
                            if self.endpoint.is_empty() {
                                "(по умолчанию для провайдера)"
                            } else {
                                &self.endpoint
                            }
                        ),
                        false,
                    );
                }
            }
            "/policy" => match arg1 {
                Some(raw_pol) => match AgentPolicy::parse_str(raw_pol) {
                    Ok(p) => {
                        self.policy = p;
                        self.persist();
                        self.push_notice(format!("Файловая политика: {}", p.label_en()), false);
                    }
                    Err(e) => self.push_notice(format!("{e}"), true),
                },
                None => self.push_notice(
                    format!("Файловая политика: {}", self.policy.label_en()),
                    false,
                ),
            },
            "/exec" => match arg1 {
                Some(raw_ep) => match ExecPolicy::parse_str(raw_ep) {
                    Ok(ep) => {
                        self.exec_policy = ep;
                        self.persist();
                        self.push_notice(
                            format!("Политика запуска кода: {}", ep.label_en()),
                            false,
                        );
                    }
                    Err(e) => self.push_notice(format!("{e}"), true),
                },
                None => self.push_notice(
                    format!("Политика запуска кода: {}", self.exec_policy.label_en()),
                    false,
                ),
            },
            "/consent" => match arg1 {
                Some("on") | Some("true") => {
                    self.allow_external = true;
                    self.persist();
                    self.push_notice(
                        "Разрешение на передачу контекста внешнему провайдеру включено.",
                        false,
                    );
                }
                Some("off") | Some("false") => {
                    self.allow_external = false;
                    self.persist();
                    self.push_notice(
                        "Разрешение на передачу контекста внешнему провайдеру отключено.",
                        false,
                    );
                }
                _ => self.push_notice(
                    format!(
                        "Передача внешнему провайдеру: {}",
                        if self.allow_external {
                            "разрешена"
                        } else {
                            "запрещена"
                        }
                    ),
                    false,
                ),
            },
            "/dynamic" => match arg1 {
                Some("on") => {
                    self.dynamic_interaction_enabled = true;
                    self.limits.dynamic_interaction_enabled = true;
                    self.persist();
                    self.push_notice(
                        "Динамическое взаимодействие с местами включено явно пользователем. Esc или `/dynamic stop` немедленно отключают режим.",
                        false,
                    );
                }
                Some("off") | Some("stop") => {
                    self.emergency_stop_dynamic();
                    self.persist();
                    self.push_notice("Динамическое взаимодействие выключено.", false);
                }
                _ => self.push_notice(
                    format!(
                        "Динамическое взаимодействие: {}",
                        if self.dynamic_interaction_enabled {
                            "включено"
                        } else {
                            "выключено по умолчанию"
                        }
                    ),
                    false,
                ),
            },
            "/toggle" => match arg1 {
                Some("all") => self.toggle_all_tools(),
                _ => {
                    if let Some(TranscriptItem::Tool(entry)) = self
                        .transcript
                        .iter_mut()
                        .rev()
                        .find(|i| matches!(i, TranscriptItem::Tool(_)))
                    {
                        entry.expanded = !entry.expanded;
                    } else {
                        self.push_notice("В текущей сессии ещё нет вызовов инструментов.", false);
                    }
                }
            },
            "/budget" => match (arg1, arg2) {
                (Some("steps"), Some(n)) => match n.parse::<usize>() {
                    Ok(v) if (1..=64).contains(&v) => {
                        self.limits.max_steps = v;
                        self.push_notice(format!("Лимит шагов: {v}"), false);
                    }
                    _ => self.push_notice("Укажите число шагов от 1 до 64", true),
                },
                (Some("wall"), Some(n)) => match n.parse::<u64>() {
                    Ok(v) if (10..=3600).contains(&v) => {
                        self.limits.max_wall_secs = v;
                        self.push_notice(format!("Лимит времени: {v} с"), false);
                    }
                    _ => self.push_notice("Укажите время от 10 до 3600 с", true),
                },
                _ => self.push_notice(
                    format!(
                        "Лимиты хода: шагов — {}, время — {} с, окно контекста — {} ток.",
                        self.limits.max_steps,
                        self.limits.max_wall_secs,
                        self.limits.max_context_tokens
                    ),
                    false,
                ),
            },
            "/compact" => {
                let comp = self.compact_session_messages();
                self.push_notice(
                    format!(
                        "История сжата: свернуто сообщений — {}, оценка токенов {} -> {}",
                        comp.dropped_messages, comp.tokens_before, comp.tokens_after
                    ),
                    false,
                );
            }
            "/skills" => match arg1 {
                Some("clear") => {
                    self.active_skills.clear();
                    self.saved_state.chats[self.session_idx]
                        .options
                        .skills
                        .clear();
                    self.persist();
                    self.push_notice("Подключённые скиллы очищены.", false);
                }
                Some("use") => {
                    let Some(id) = arg2 else {
                        self.push_notice("Использование: /skills use <id>", true);
                        return;
                    };
                    match read_skill_any(&self.workspace, id) {
                        Ok(content) => {
                            let opts = &mut self.saved_state.chats[self.session_idx].options;
                            if !opts.skills.is_empty() {
                                opts.skills.push_str("\n\n");
                            }
                            opts.skills.push_str(&format!("# Skill: {id}\n{content}"));
                            if !self.active_skills.iter().any(|s| s == id) {
                                self.active_skills.push(id.to_string());
                            }
                            self.persist();
                            self.push_notice(format!("Подключён скилл: {id}"), false);
                        }
                        Err(e) => self.push_notice(format!("{e}"), true),
                    }
                }
                _ => {
                    let mut names = list_workspace_skill_ids(&self.workspace);
                    for s in desktop::skills::discover() {
                        if !names.contains(&s.id) {
                            names.push(s.id);
                        }
                    }
                    if names.is_empty() {
                        self.push_notice("Скиллы не найдены.", false);
                    } else {
                        self.push_notice(
                            format!("Доступные скиллы ({}): {}", names.len(), names.join(", ")),
                            false,
                        );
                    }
                }
            },
            "/sessions" => match arg1 {
                Some("new") => {
                    let cfg = self.current_as_config();
                    self.saved_state.chats.push(new_tui_chat(&cfg));
                    self.session_idx = self.saved_state.chats.len() - 1;
                    self.transcript.clear();
                    self.turn_start_idx = 0;
                    self.persist();
                    actions.push(TuiAction::ClearScreen);
                    self.push_notice(
                        format!("Создана новая сессия {}", short_uuid(self.active_chat().id)),
                        false,
                    );
                }
                Some("resume") => {
                    let Some(prefix) = arg2 else {
                        self.push_notice("Использование: /sessions resume <префикс-id>", true);
                        return;
                    };
                    if let Some(pos) = self
                        .saved_state
                        .chats
                        .iter()
                        .position(|c| c.id.to_string().starts_with(prefix))
                    {
                        self.session_idx = pos;
                        self.rebuild_transcript_from_active_chat();
                        actions.push(TuiAction::ClearScreen);
                        self.push_notice(
                            format!("Возобновлена сессия {}", short_uuid(self.active_chat().id)),
                            false,
                        );
                    } else {
                        self.push_notice(format!("Сессия «{prefix}» не найдена"), true);
                    }
                }
                _ => {
                    let mut out = String::from("Сохранённые сессии:\n");
                    for (i, c) in self.saved_state.chats.iter().enumerate() {
                        let mark = if i == self.session_idx { "▸" } else { " " };
                        out.push_str(&format!(
                            "  {mark} {} · {} ({} сообщ.)\n",
                            short_uuid(c.id),
                            c.title,
                            c.messages.len()
                        ));
                    }
                    self.push_notice(out.trim_end().to_string(), false);
                }
            },
            "/theme" => match arg1 {
                Some(raw_theme) => match term::TuiTheme::parse_str(raw_theme) {
                    Some(t) => {
                        self.theme = t;
                        self.push_notice(format!("Активна тема палитры: {}", t.label_ru()), false);
                    }
                    None => self.push_notice("Использование: /theme [carbon|bone]", true),
                },
                None => {
                    self.theme = match self.theme {
                        term::TuiTheme::Carbon => term::TuiTheme::Bone,
                        term::TuiTheme::Bone => term::TuiTheme::Carbon,
                    };
                    self.push_notice(
                        format!("Переключена тема палитры: {}", self.theme.label_ru()),
                        false,
                    );
                }
            },
            other => {
                self.push_notice(
                    format!("Неизвестная команда: {other}. Введите /help для справки."),
                    true,
                );
            }
        }
    }

    pub fn compact_session_messages(&mut self) -> CompactionSummary {
        let mut wire: Vec<serde_json::Value> = self.saved_state.chats[self.session_idx]
            .messages
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();
        let tokens_before = context::estimate_messages_tokens(&wire);
        let mut changed_files: Vec<String> = self.saved_state.chats[self.session_idx]
            .messages
            .iter()
            .flat_map(|m| m.changed_files.iter().cloned())
            .collect();
        changed_files.sort();
        changed_files.dedup();

        let prev_compactions = self.context_metrics.map(|m| m.compactions).unwrap_or(0);
        let target_limit = (tokens_before / 2)
            .max(512)
            .min(self.limits.max_context_tokens);
        if let Some(summary) =
            context::compact_wire_messages(&mut wire, target_limit, &changed_files, None, true)
        {
            let msgs = &mut self.saved_state.chats[self.session_idx].messages;
            let keep_tail = msgs.split_off(msgs.len().saturating_sub(6));
            let first_user = msgs.iter().find(|m| m.role == "user").cloned();
            let mut summary_msg = Message::user(summary.summary.clone());
            summary_msg.role = "system".into();
            msgs.clear();
            if let Some(u0) = first_user {
                msgs.push(u0);
            }
            msgs.push(summary_msg);
            msgs.extend(keep_tail);
            self.persist();
            let wire_after: Vec<serde_json::Value> = self.saved_state.chats[self.session_idx]
                .messages
                .iter()
                .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
                .collect();
            self.context_metrics = Some(context::context_metrics(
                &wire_after,
                self.limits.max_context_tokens,
                prev_compactions + 1,
            ));
            summary
        } else {
            self.context_metrics = Some(context::context_metrics(
                &wire,
                self.limits.max_context_tokens,
                prev_compactions,
            ));
            CompactionSummary {
                dropped_messages: 0,
                tokens_before,
                tokens_after: tokens_before,
                summary: String::new(),
            }
        }
    }

    fn current_as_config(&self) -> TuiConfig {
        TuiConfig {
            data_dir: self.data_dir.clone(),
            workspace: self.workspace.clone(),
            provider: self.provider,
            model: self.model.clone(),
            endpoint: self.endpoint.clone(),
            policy: self.policy,
            exec_policy: self.exec_policy,
            allow_external: self.allow_external,
            dynamic_interaction: self.dynamic_interaction_enabled,
            resume_session: None,
            color_override: None,
        }
    }
}

fn new_tui_chat(cfg: &TuiConfig) -> Chat {
    let mut chat = Chat::new();
    chat.options.agent_enabled = true;
    chat.options.workspace_path = cfg.workspace.display().to_string();
    chat.options.provider = cfg.provider;
    chat.options.provider_endpoint = cfg.endpoint.clone();
    chat.options.provider_model = cfg.model.clone();
    chat.options.agent_policy = cfg.policy;
    chat.options.exec_policy = cfg.exec_policy;
    chat.options.external_provider_consent = cfg.allow_external;
    chat.options.dynamic_interaction_enabled = cfg.dynamic_interaction;
    chat
}

fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1).min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_add(1).min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn list_workspace_skill_ids(workspace: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for rel in [".kaic/skills", "skills"] {
        let dir = workspace.join(rel);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir() && p.join("SKILL.md").is_file() {
                if let Some(id) = p.file_name().and_then(|n| n.to_str()) {
                    out.push(id.to_string());
                }
            } else if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("md")
                && let Some(stem) = p.file_stem().and_then(|n| n.to_str())
            {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn read_skill_any(workspace: &Path, id: &str) -> Result<String> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        bail!("Некорректный идентификатор скилла: {id}");
    }
    for rel in [".kaic/skills", "skills"] {
        let dir_skill = workspace.join(rel).join(id).join("SKILL.md");
        if dir_skill.is_file() {
            return Ok(std::fs::read_to_string(&dir_skill)?);
        }
        let md_skill = workspace.join(rel).join(format!("{id}.md"));
        if md_skill.is_file() {
            return Ok(std::fs::read_to_string(&md_skill)?);
        }
    }
    let val = desktop::skills::read_json(id)?;
    val["content"]
        .as_str()
        .map(ToString::to_string)
        .context("Содержимое скилла отсутствует")
}

fn env_secret_tui(names: &[&str]) -> Option<Secret> {
    for name in names {
        if let Ok(v) = std::env::var(name) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(Secret::new(trimmed.to_string()));
            }
        }
    }
    None
}

enum UiInputMsg {
    Key(KeyEvent),
    Resize,
    Eof,
}

enum RunnerMsg {
    Stream(AgentStreamEvent),
    Finished {
        result: Result<AgentRunResult>,
        server: Option<ManagedServer>,
    },
}

fn emit_welcome_and_prior<W: Write>(
    viewport: &mut InlineViewport,
    out: &mut W,
    state: &mut TuiState,
    cols: u16,
    color: ColorMode,
    logo_proto: LogoProtocol,
) -> io::Result<()> {
    let welcome = render::render_welcome_banner(state, cols, color, logo_proto);
    viewport.write_permanent(out, &welcome, color)?;
    if !state.transcript.is_empty() {
        let history_lines =
            render::render_transcript_items(state.theme, &state.transcript, cols, color);
        viewport.write_permanent(out, &history_lines, color)?;
        state.turn_start_idx = state.transcript.len();
    }
    Ok(())
}

/// Запускает интерактивный inline-интерфейс терминала KAIC (POSIX TTY raw mode с нативным scrollback либо построчный stdin-fallback).
pub fn run_tui(cfg: TuiConfig) -> Result<()> {
    crate::agent::WorkspaceGuard::new(&cfg.workspace)?;
    let color = cfg
        .color_override
        .unwrap_or_else(|| ColorMode::detect(false));
    let logo_proto = LogoProtocol::detect(color);
    let mut state = TuiState::load(&cfg);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    if !is_tty() {
        return run_piped_fallback(&rt, &mut state);
    }

    let _guard = RawTerminalGuard::enter()?;
    rt.block_on(async move {
        let mut viewport = InlineViewport::new();
        {
            let (cols, _) = terminal_size();
            let mut out = io::stdout().lock();
            emit_welcome_and_prior(&mut viewport, &mut out, &mut state, cols, color, logo_proto)?;
        }

        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<UiInputMsg>();
        let stop_reader = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_bg = stop_reader.clone();

        let reader_handle = std::thread::spawn(move || {
            let mut parser = InputParser::default();
            let mut buf = [0u8; 256];
            while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                if take_resize_flag() {
                    let _ = in_tx.send(UiInputMsg::Resize);
                }
                let timeout = if parser.has_pending_escape() { 25 } else { 80 };
                if wait_stdin_ready(timeout) {
                    match read_stdin_nonblocking(&mut buf) {
                        Ok(0) => {
                            let _ = in_tx.send(UiInputMsg::Eof);
                            break;
                        }
                        Ok(n) => {
                            for ev in parser.feed(&buf[..n], false) {
                                if in_tx.send(UiInputMsg::Key(ev)).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                } else if parser.has_pending_escape() {
                    for ev in parser.feed(&[], true) {
                        if in_tx.send(UiInputMsg::Key(ev)).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let (run_tx, mut run_rx) = tokio::sync::mpsc::unbounded_channel::<RunnerMsg>();
        let mut cached_server: Option<ManagedServer> = None;
        let mut active_cancel: Option<Cancel> = None;
        let mut active_approval: Option<ApprovalHandle> = None;
        let mut needs_redraw = true;
        let mut anim_interval = tokio::time::interval(Duration::from_millis(80));
        anim_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if needs_redraw {
                let (cols, rows) = terminal_size();
                let turn_lines = render::render_active_turn(&state, cols, color);
                let chrome_lines = render::render_live_chrome(&state, cols, color);
                let mut out = io::stdout().lock();
                viewport.update_live(&mut out, &turn_lines, &chrome_lines, rows, color)?;
                needs_redraw = false;
            }

            if state.should_quit {
                break;
            }

            tokio::select! {
                _ = anim_interval.tick(), if state.busy => {
                    state.anim_tick = state.anim_tick.wrapping_add(1);
                    needs_redraw = true;
                }
                Some(msg) = in_rx.recv() => {
                    match msg {
                        UiInputMsg::Resize => {
                            let (_, new_rows) = terminal_size();
                            let mut out = io::stdout().lock();
                            viewport.on_resize(&mut out, new_rows)?;
                            needs_redraw = true;
                        }
                        UiInputMsg::Eof => {
                            state.persist();
                            break;
                        }
                        UiInputMsg::Key(kev) => {
                            for action in state.handle_key(kev) {
                                match action {
                                    TuiAction::CommitSlice { start, end } => {
                                        let (cols, _) = terminal_size();
                                        let safe_end = end.min(state.transcript.len());
                                        let safe_start = start.min(safe_end);
                                        let prior_lines = render::render_transcript_items(
                                            state.theme,
                                            &state.transcript[safe_start..safe_end],
                                            cols,
                                            color,
                                        );
                                        let mut out = io::stdout().lock();
                                        viewport.commit_turn(&mut out, &prior_lines, color)?;
                                    }
                                    TuiAction::ClearScreen => {
                                        let (cols, _) = terminal_size();
                                        let mut out = io::stdout().lock();
                                        viewport.clear_visible_screen(&mut out)?;
                                        emit_welcome_and_prior(
                                            &mut viewport,
                                            &mut out,
                                            &mut state,
                                            cols,
                                            color,
                                            logo_proto,
                                        )?;
                                        needs_redraw = true;
                                    }
                                    TuiAction::Redraw => needs_redraw = true,
                                    TuiAction::Quit => {
                                        if let Some(c) = &active_cancel {
                                            c.cancel();
                                        }
                                        state.should_quit = true;
                                    }
                                    TuiAction::CancelRun => {
                                        if let Some(c) = &active_cancel {
                                            c.cancel();
                                        }
                                        needs_redraw = true;
                                    }
                                    TuiAction::RespondApproval { request_id, action_digest, approved } => {
                                        if let Some(handle) = &active_approval {
                                            handle.respond(request_id, action_digest, approved);
                                        }
                                        needs_redraw = true;
                                    }
                                    TuiAction::RunPrompt(prompt) => {
                                        let cancel = Cancel::default();
                                        let (gate, handle) = ApprovalGate::interactive();
                                        active_cancel = Some(cancel.clone());
                                        active_approval = Some(handle);
                                        let tx = run_tx.clone();
                                        let srv = cached_server.take();
                                        let turn_cfg = state.current_as_config();
                                        let prior = build_prior_messages(state.active_chat());
                                        let limits = state.limits.clone();
                                        std::thread::spawn(move || {
                                            match tokio::runtime::Builder::new_current_thread()
                                                .enable_all()
                                                .build()
                                            {
                                                Ok(worker_rt) => worker_rt.block_on(
                                                    execute_agent_turn(
                                                        turn_cfg, prompt, prior, limits, srv,
                                                        cancel, gate, tx.clone(),
                                                    ),
                                                ),
                                                Err(e) => {
                                                    let _ = tx.send(RunnerMsg::Finished {
                                                        result: Err(e.into()),
                                                        server: srv,
                                                    });
                                                }
                                            }
                                        });
                                        needs_redraw = true;
                                    }
                                }
                            }
                        }
                    }
                }
                Some(rmsg) = run_rx.recv() => {
                    match rmsg {
                        RunnerMsg::Stream(sev) => {
                            state.apply_stream_event(sev);
                            needs_redraw = true;
                        }
                        RunnerMsg::Finished { result, server } => {
                            cached_server = server;
                            active_cancel = None;
                            active_approval = None;
                            state.finish_run(result);
                            needs_redraw = true;
                        }
                    }
                }
            }
        }

        // При выходе стираем временный композер и оставляем диалог в нативном scrollback терминала.
        {
            let (cols, _) = terminal_size();
            let tail_lines = render::render_active_turn(&state, cols, color);
            let mut out = io::stdout().lock();
            let _ = viewport.commit_turn(&mut out, &tail_lines, color);
        }

        stop_reader.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = reader_handle.join();
        Ok::<(), anyhow::Error>(())
    })
}

fn build_prior_messages(chat: &Chat) -> Vec<Message> {
    let mut prior = Vec::new();
    for m in &chat.messages {
        if m.role == "user" && !m.content.trim().is_empty() {
            prior.push(Message::user(m.content.clone()));
        } else if m.role == "assistant" && !m.content.trim().is_empty() {
            let mut am = Message::assistant();
            am.content = m.content.clone();
            prior.push(am);
        }
    }
    if prior.last().is_some_and(|m| m.role == "user") {
        prior.pop();
    }
    let _ = chat.options.messages(Vec::new()).map(|sys| {
        for s in sys.into_iter().rev() {
            prior.insert(0, s);
        }
    });
    prior
}

#[allow(clippy::too_many_arguments)]
async fn execute_agent_turn(
    cfg: TuiConfig,
    prompt: String,
    prior_messages: Vec<Message>,
    limits: AgentLimits,
    mut server: Option<ManagedServer>,
    cancel: Cancel,
    gate: ApprovalGate,
    tx: tokio::sync::mpsc::UnboundedSender<RunnerMsg>,
) {
    let mut sink = |ev: AgentStreamEvent| {
        let _ = tx.send(RunnerMsg::Stream(ev));
    };
    let result = match cfg.provider {
        ProviderKind::LocalLlama => {
            let (models, _) = catalog::discover(Path::new(catalog::MODEL_ROOT));
            let found = models.iter().find(|m| {
                m.loadable()
                    && m.kind() == catalog::ModelKind::Text
                    && (m.alias() == cfg.model
                        || m.repo == cfg.model
                        || m.path.to_str() == Some(cfg.model.as_str())
                        || m.filename == cfg.model)
            });
            match found {
                None => Err(anyhow::anyhow!(
                    "Проверенная GGUF-модель «{}» не найдена (откройте /model для выбора)",
                    cfg.model
                )),
                Some(model) => {
                    let effective_ctx = catalog::live_profile(&model.repo)
                        .map(|p| p.context)
                        .unwrap_or(8192);
                    if !server
                        .as_mut()
                        .is_some_and(|s| s.matches(model, effective_ctx))
                    {
                        server.take();
                        let _ = tx.send(RunnerMsg::Stream(AgentStreamEvent::Status(format!(
                            "Загрузка {}…",
                            model.alias()
                        ))));
                        match ManagedServer::start(model, 0, &cancel).await {
                            Ok(s) => server = Some(s),
                            Err(e) => {
                                let _ = tx.send(RunnerMsg::Finished {
                                    result: Err(e),
                                    server: None,
                                });
                                return;
                            }
                        }
                    }
                    let mut backend =
                        ManagedServerBackend::new(server.as_mut().unwrap(), model.alias().into());
                    run_agent_with_policies(
                        &mut backend,
                        &cfg.workspace,
                        &prompt,
                        &prior_messages,
                        cfg.policy,
                        cfg.exec_policy,
                        limits,
                        &cancel,
                        &gate,
                        &mut sink,
                    )
                    .await
                }
            }
        }
        ProviderKind::OllamaLocal => {
            let ep = if cfg.endpoint.trim().is_empty() {
                "http://127.0.0.1:11434"
            } else {
                cfg.endpoint.trim()
            };
            match OllamaBackend::new(ep, cfg.model.clone(), 8192) {
                Ok(mut backend) => {
                    run_agent_with_policies(
                        &mut backend,
                        &cfg.workspace,
                        &prompt,
                        &prior_messages,
                        cfg.policy,
                        cfg.exec_policy,
                        limits,
                        &cancel,
                        &gate,
                        &mut sink,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
        ProviderKind::OpenAiCompat => {
            let api_key = env_secret_tui(&["KAIC_OPENAI_API_KEY", "OPENAI_API_KEY"]);
            match OpenAiCompatBackend::new(
                cfg.endpoint.trim(),
                cfg.model.clone(),
                api_key,
                cfg.allow_external,
            ) {
                Ok(mut backend) => {
                    run_agent_with_policies(
                        &mut backend,
                        &cfg.workspace,
                        &prompt,
                        &prior_messages,
                        cfg.policy,
                        cfg.exec_policy,
                        limits,
                        &cancel,
                        &gate,
                        &mut sink,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
        ProviderKind::AnthropicApi => {
            let ep = if cfg.endpoint.trim().is_empty() {
                "https://api.anthropic.com"
            } else {
                cfg.endpoint.trim()
            };
            let api_key = env_secret_tui(&["KAIC_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"])
                .unwrap_or_default();
            let model_id = if cfg.model == "gemma26" {
                "claude-sonnet-4-5".to_string()
            } else {
                cfg.model.clone()
            };
            match AnthropicApiBackend::new(ep, model_id, api_key, cfg.allow_external) {
                Ok(mut backend) => {
                    run_agent_with_policies(
                        &mut backend,
                        &cfg.workspace,
                        &prompt,
                        &prior_messages,
                        cfg.policy,
                        cfg.exec_policy,
                        limits,
                        &cancel,
                        &gate,
                        &mut sink,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
        ProviderKind::CodexCli | ProviderKind::ClaudeCli | ProviderKind::AgyCli => {
            let cli_cfg = CodexCliConfig {
                workspace: cfg.workspace.clone(),
                prompt,
                policy: cfg.policy,
                external_consent: cfg.allow_external,
                model: (cfg.model != "gemma26").then(|| cfg.model.clone()),
            };
            match cfg.provider {
                ProviderKind::CodexCli => run_codex_cli(cli_cfg, &cancel, &gate, &mut sink).await,
                ProviderKind::ClaudeCli => run_claude_cli(cli_cfg, &cancel, &gate, &mut sink).await,
                ProviderKind::AgyCli => run_agy_cli(cli_cfg, &cancel, &gate, &mut sink).await,
                _ => unreachable!(),
            }
        }
    };
    let _ = tx.send(RunnerMsg::Finished { result, server });
}

fn run_piped_fallback(rt: &tokio::runtime::Runtime, state: &mut TuiState) -> Result<()> {
    let stdin = io::stdin();
    let mut line = String::new();
    let mut cached_server: Option<ManagedServer> = None;
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut actions = Vec::new();
        if trimmed.starts_with('/') {
            state.execute_slash_command(trimmed, &mut actions);
            if let ActiveMenu::ModelPicker { items, .. } = &state.active_menu {
                for opt in items {
                    println!("{} · {} ({})", opt.model, opt.badge_ru, opt.detail_ru);
                }
                state.active_menu = ActiveMenu::None;
            }
            for item in state.transcript.drain(..) {
                if let TranscriptItem::Notice { text, .. } = item {
                    println!("{text}");
                }
            }
            state.turn_start_idx = 0;
            if state.should_quit {
                break;
            }
            continue;
        }
        state.prepare_prompt_turn(trimmed.to_string());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::default();
        let gate = ApprovalGate::deny_all();
        let cfg = state.current_as_config();
        let prior = build_prior_messages(state.active_chat());
        let limits = state.limits.clone();
        let srv = cached_server.take();
        rt.block_on(async move {
            execute_agent_turn(
                cfg,
                trimmed.to_string(),
                prior,
                limits,
                srv,
                cancel,
                gate,
                tx,
            )
            .await;
        });
        while let Ok(msg) = rx.try_recv() {
            match msg {
                RunnerMsg::Stream(AgentStreamEvent::TextDelta(d)) => print!("{d}"),
                RunnerMsg::Finished { result, server } => {
                    cached_server = server;
                    state.finish_run(result);
                    println!();
                }
                RunnerMsg::Stream(sev) => state.apply_stream_event(sev),
            }
        }
    }
    state.persist();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::IsolationLevel;
    use crate::tui::term::strip_ansi_visible;

    fn test_state(temp: &Path) -> TuiState {
        let data = temp.join("data");
        let ws = temp.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        TuiState::load(&TuiConfig {
            data_dir: data,
            workspace: ws,
            provider: ProviderKind::LocalLlama,
            model: "gemma26".into(),
            endpoint: String::new(),
            policy: AgentPolicy::WorkspaceWrite,
            exec_policy: ExecPolicy::ConfirmUserCode,
            allow_external: false,
            dynamic_interaction: false,
            resume_session: None,
            color_override: Some(ColorMode::Plain),
        })
    }

    #[test]
    fn inline_welcome_uses_official_kaic_logo_without_duplication_or_cryptic_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = test_state(tmp.path());
        assert!(!st.dynamic_interaction_enabled);

        for (cols, rows) in [(120u16, 36u16), (80, 24), (50, 18)] {
            for mode in [ColorMode::Plain, ColorMode::Ansi16, ColorMode::TrueColor] {
                let view = render::render_inline_view(&st, cols, rows, mode);
                // Компактная компоновка без растягивания на всю высоту окна!
                assert!(
                    view.len() <= 10,
                    "view too tall ({} lines) for empty state",
                    view.len()
                );
                let joined_raw = view.join("\n");
                let joined_plain = strip_ansi_visible(&joined_raw);

                // Единственная метка бренда KAIC в приветствии (без дублирования в подвале/композере)
                assert_eq!(joined_plain.matches("KAIC").count(), 1);
                // Отсутствуют криптичные сокращения старого fullscreen TUI
                assert!(!joined_plain.contains("f:rw"));
                assert!(!joined_plain.contains("e:ask"));
                assert!(!joined_plain.contains("[DYN:OFF]"));
                assert!(!joined_plain.contains("ctx:—"));
                // Поле ввода (❯) расположено сразу после приветственной секции
                assert!(joined_plain.contains("❯"));

                if mode == ColorMode::Plain {
                    assert!(!joined_raw.contains('\x1b'));
                }
                if mode == ColorMode::TrueColor && cols >= 80 {
                    // Фирменный kiln-500 (#D2602C -> 210;96;44) и model-local (#5FC2B0 -> 95;194;176)
                    assert!(joined_raw.contains("210;96;44"));
                    assert!(joined_raw.contains("95;194;176"));
                }
            }
        }

        // Официальная светлая тема Bone из Desige/kaic-palette.json
        st.input = "/theme bone".into();
        st.cursor = st.input.len();
        st.handle_key(KeyEvent::Enter);
        assert_eq!(st.theme, term::TuiTheme::Bone);
        let bone_view = render::render_inline_view(&st, 100, 30, ColorMode::TrueColor).join("\n");
        // Bone ink (#0B0B0C -> 11;11;12) и kiln-500 (#D2602C -> 210;96;44)
        assert!(bone_view.contains("11;11;12"));
        assert!(bone_view.contains("210;96;44"));
    }

    #[test]
    fn interactive_model_picker_and_slash_menu_navigate_filter_and_select() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = test_state(tmp.path());

        // Ввод `/mo` открывает интерактивное меню slash-команд, Enter открывает Model Picker!
        for ch in "/mo".chars() {
            st.handle_key(KeyEvent::Char(ch));
        }
        assert!(st.is_slash_menu_open());
        let slash_chrome = render::render_live_chrome(&st, 100, ColorMode::TrueColor).join("\n");
        assert!(strip_ansi_visible(&slash_chrome).contains("/model"));
        // Подсветка выбранного пункта тоном selection Carbon (#2D1A10 -> 45;26;16)
        assert!(slash_chrome.contains("45;26;16"));

        st.handle_key(KeyEvent::Enter);
        assert!(matches!(st.active_menu, ActiveMenu::ModelPicker { .. }));

        // Живая фильтрация по "qwen2.5-coder:7b" и выбор через Enter
        for ch in "7b".chars() {
            st.handle_key(KeyEvent::Char(ch));
        }
        let picker_chrome = render::render_live_chrome(&st, 100, ColorMode::Plain).join("\n");
        assert!(picker_chrome.contains("Выбор модели"));
        assert!(picker_chrome.contains("qwen2.5-coder:7b"));

        st.handle_key(KeyEvent::Enter);
        assert_eq!(st.active_menu, ActiveMenu::None);
        assert_eq!(st.provider, ProviderKind::OllamaLocal);
        assert_eq!(st.model, "qwen2.5-coder:7b");
    }

    #[test]
    fn multiline_input_history_persistence_and_slash_autocomplete_work() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut st = test_state(tmp.path());
            for ch in "/pr".chars() {
                st.handle_key(KeyEvent::Char(ch));
            }
            st.handle_key(KeyEvent::Tab);
            assert_eq!(st.input, "/provider ");
            st.input.clear();
            st.cursor = 0;

            for ch in "первая строка".chars() {
                st.handle_key(KeyEvent::Char(ch));
            }
            st.handle_key(KeyEvent::Newline);
            for ch in "вторая".chars() {
                st.handle_key(KeyEvent::Char(ch));
            }
            let acts = st.handle_key(KeyEvent::Enter);
            assert!(acts.iter().any(
                |a| matches!(a, TuiAction::RunPrompt(p) if p.contains("первая") && p.contains('\n'))
            ));
            st.finish_run(Ok(AgentRunResult {
                final_text: "Готово".into(),
                ..Default::default()
            }));
        }

        let data = tmp.path().join("data");
        let ws = tmp.path().join("ws");
        let saved: SavedState = storage::load_json(&data.join("state.json")).unwrap();
        let id_prefix: String = saved.chats[0].id.to_string().chars().take(8).collect();
        let mut reloaded = TuiState::load(&TuiConfig {
            data_dir: data,
            workspace: ws,
            provider: ProviderKind::LocalLlama,
            model: "gemma26".into(),
            endpoint: String::new(),
            policy: AgentPolicy::WorkspaceWrite,
            exec_policy: ExecPolicy::ConfirmUserCode,
            allow_external: false,
            dynamic_interaction: false,
            resume_session: Some(id_prefix),
            color_override: Some(ColorMode::Plain),
        });
        assert!(!reloaded.history.is_empty());
        assert!(!reloaded.transcript.is_empty());
        reloaded.handle_key(KeyEvent::Up);
        assert!(reloaded.input.contains("первая строка"));
    }

    #[test]
    fn collapsible_tools_inline_diff_syntax_highlight_and_spinner_render() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = test_state(tmp.path());
        st.busy = true;
        st.anim_tick = 2;
        st.status_line = "Анализ кода…".into();
        st.apply_stream_event(AgentStreamEvent::ContextUsage(ContextMetrics {
            used_tokens: 1200,
            context_limit: 8192,
            utilization_pct: 15,
            compactions: 0,
        }));
        st.apply_stream_event(AgentStreamEvent::Event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "read_file".into(),
            args_summary: "src/lib.rs".into(),
            policy_note: "ws".into(),
        }));
        st.apply_stream_event(AgentStreamEvent::Event(AgentEvent::ToolResult {
            id: "t1".into(),
            name: "read_file".into(),
            ok: true,
            summary: "src/lib.rs · 10 строк".into(),
            output_preview: "secret_read_preview_line".into(),
            changed_files: vec![],
            diff_preview: None,
        }));

        let collapsed = render::render_inline_view(&st, 100, 30, ColorMode::Plain).join("\n");
        assert!(collapsed.contains("▸ ✓ read_file"));
        assert!(!collapsed.contains("secret_read_preview_line"));

        st.handle_key(KeyEvent::Ctrl('o'));
        let expanded = render::render_inline_view(&st, 100, 30, ColorMode::Plain).join("\n");
        assert!(expanded.contains("▾ ✓ read_file"));
        assert!(expanded.contains("secret_read_preview_line"));

        st.apply_stream_event(AgentStreamEvent::Event(AgentEvent::ToolResult {
            id: "t2".into(),
            name: "apply_patch".into(),
            ok: true,
            summary: "calc.rs".into(),
            output_preview: "patched".into(),
            changed_files: vec!["calc.rs".into()],
            diff_preview: Some("--- a/calc.rs\n+++ b/calc.rs\n-let a = 1;\n+let a = 2;".into()),
        }));
        st.apply_stream_event(AgentStreamEvent::TextDelta(
            "```rust\nfn add(a: i32) -> i32 { a + 2 }\n```\n".into(),
        ));
        let rendered_tc = render::render_inline_view(&st, 100, 36, ColorMode::TrueColor).join("\n");
        assert!(strip_ansi_visible(&rendered_tc).contains("Контекст: 15%"));
        assert!(strip_ansi_visible(&rendered_tc).contains("+let a = 2;"));
        // Цвет добавления signal-ok (#5DB57A -> 93;181;122) и удаления signal-error (#F2645F -> 242;100;95)
        assert!(rendered_tc.contains("93;181;122"));
        assert!(rendered_tc.contains("242;100;95"));
    }

    #[test]
    fn hitl_approval_ignores_bare_enter_and_esc_cancels_dynamically() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = test_state(tmp.path());
        st.busy = true;
        st.dynamic_interaction_enabled = true;
        st.pending_approval = Some(PendingApproval {
            request_id: "req-7".into(),
            action_digest: "deadbeef1234".into(),
            tool: "run_check".into(),
            isolation: IsolationLevel::NoneUserPrivileges,
            summary: "cargo test".into(),
            canonical_target: "ws".into(),
            risk_note: "runs cargo".into(),
        });

        let enter_acts = st.handle_key(KeyEvent::Enter);
        assert!(enter_acts.is_empty());
        assert!(st.pending_approval.is_some());

        let esc_acts = st.handle_key(KeyEvent::Esc);
        assert!(!st.dynamic_interaction_enabled);
        assert!(esc_acts.contains(&TuiAction::CancelRun));
        assert!(esc_acts.contains(&TuiAction::RespondApproval {
            request_id: "req-7".into(),
            action_digest: "deadbeef1234".into(),
            approved: false,
        }));

        st.pending_approval = Some(PendingApproval {
            request_id: "req-8".into(),
            action_digest: "cafebabe5678".into(),
            tool: "write_file".into(),
            isolation: IsolationLevel::NoneUserPrivileges,
            summary: "write x.rs".into(),
            canonical_target: "x.rs".into(),
            risk_note: "writes".into(),
        });
        let y_acts = st.handle_key(KeyEvent::Char('y'));
        assert!(y_acts.contains(&TuiAction::RespondApproval {
            request_id: "req-8".into(),
            action_digest: "cafebabe5678".into(),
            approved: true,
        }));
    }

    #[test]
    fn slash_commands_switch_providers_policies_and_keep_dynamic_mode_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = test_state(tmp.path());
        assert!(!st.dynamic_interaction_enabled);
        for line in [
            "/provider ollama",
            "/model qwen2.5-coder:7b",
            "/endpoint http://127.0.0.1:11434",
            "/policy read-only",
            "/exec safe-inspect",
            "/consent on",
            "/dynamic on",
        ] {
            st.input = line.into();
            st.cursor = st.input.len();
            st.handle_key(KeyEvent::Enter);
        }
        assert_eq!(st.provider, ProviderKind::OllamaLocal);
        assert_eq!(st.model, "qwen2.5-coder:7b");
        assert_eq!(st.policy, AgentPolicy::ReadOnly);
        assert_eq!(st.exec_policy, ExecPolicy::SafeInspectOnly);
        assert!(st.allow_external);
        assert!(st.dynamic_interaction_enabled);

        st.input = "/dynamic stop".into();
        st.cursor = st.input.len();
        st.handle_key(KeyEvent::Enter);
        assert!(!st.dynamic_interaction_enabled);
    }
}
