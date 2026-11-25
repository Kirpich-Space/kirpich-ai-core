//! Динамичный агентный runtime KAIC Studio:
//! - Раздельные политики файлов (`AgentPolicy`) и исполнения кода (`ExecPolicy`)
//! - Одноразовые подтверждения с привязкой к `(request_id, SHA-256 action_digest)` и честным `IsolationLevel`
//! - Версионно-привязанная и целевая верификация подзадач с аннулированием после любых правок.
pub mod asm_accel;
pub mod context;
pub mod policy;
pub mod providers;
pub mod remote;
pub mod subagents;
pub mod tool_parser;
pub mod tools;
pub mod workspace_live;

pub use asm_accel::{AtomicSeatGate, accel_backend_label};
pub use context::{CompactionSummary, ContextMetrics};
pub use policy::{
    AgentPolicy, CommandClass, ExecPolicy, IsolationLevel, ValidatedCommand, WorkspaceGuard,
};
pub use providers::{
    ApprovalGranularity, CodexCliConfig, ExecutionOrigin, ExternalCliConfig, FeatureStatus,
    ProviderCapabilities, ProviderKind, probe_providers, run_agy_cli, run_agy_cli_with_binary,
    run_claude_cli, run_claude_cli_with_binary, run_codex_cli, run_codex_cli_with_binary,
};
pub use remote::{AnthropicApiBackend, OllamaBackend, OpenAiCompatBackend};
pub use subagents::{MAX_SUBTASKS, SubtaskRecord, SubtaskSpec};
pub use tools::{ToolExecution, tool_definitions};
pub use workspace_live::{
    AgentSeat, LiveEditOp, LiveEditResult, MAX_AGENT_SEATS, OsInputBackendCapabilities,
    SeatManager, VirtualKeyboard, VirtualPointer, probe_os_input_backends,
};

use crate::{
    llama::{AgentTurnOutput, ManagedServer, ToolCall, stream_agent_turn_with_grammar},
    network::{Cancel, LocalEndpoint},
    sse::StreamEvent,
    storage::Message,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    io::Read,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLimits {
    pub max_steps: usize,
    pub max_wall_secs: u64,
    pub max_repeat_failures: usize,
    pub max_tool_output_bytes: usize,
    pub max_context_tokens: usize,
    pub max_total_tool_calls: usize,
    pub enable_grammar_fallback: bool,
    pub dynamic_interaction_enabled: bool,
    pub checkpoint_dir: Option<std::path::PathBuf>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_steps: 12,
            max_wall_secs: 300,
            max_repeat_failures: 3,
            max_tool_output_bytes: 24 * 1024,
            max_context_tokens: 16_384,
            max_total_tool_calls: 40,
            enable_grammar_fallback: false,
            dynamic_interaction_enabled: false,
            checkpoint_dir: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentTaskCheckpoint {
    pub task_id: String,
    pub workspace: String,
    pub provider: ProviderKind,
    pub model: String,
    pub step: usize,
    pub status: String,
    pub changed_files: Vec<String>,
    pub context_metrics: ContextMetrics,
    pub events_count: usize,
    pub updated_epoch_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    Step {
        step: usize,
        max_steps: usize,
        provider: ProviderKind,
        model: String,
        summary: String,
    },
    ToolCall {
        id: String,
        name: String,
        args_summary: String,
        policy_note: String,
    },
    ApprovalRequested {
        request_id: String,
        tool: String,
        summary: String,
        canonical_target: String,
    },
    ApprovalResolved {
        request_id: String,
        approved: bool,
        reason: String,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        summary: String,
        output_preview: String,
        #[serde(default)]
        changed_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_preview: Option<String>,
    },
    Blocked {
        reason: String,
    },
    Cancelled {
        summary: String,
        #[serde(default)]
        changed_files: Vec<String>,
    },
    Completed {
        summary: String,
        #[serde(default)]
        changed_files: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingApproval {
    pub request_id: String,
    pub action_digest: String,
    pub tool: String,
    #[serde(default)]
    pub isolation: IsolationLevel,
    pub summary: String,
    pub canonical_target: String,
    pub risk_note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub action_digest: String,
    pub approved: bool,
}

#[derive(Clone, Debug)]
pub enum AgentStreamEvent {
    Status(String),
    TextDelta(String),
    Event(AgentEvent),
    Subtask(SubtaskRecord),
    SeatUpdate(AgentSeat),
    ContextUsage(ContextMetrics),
    ApprovalNeeded(PendingApproval),
    Metrics { tokens: u32, tps: f32, seconds: f32 },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub final_text: String,
    pub events: Vec<AgentEvent>,
    pub changed_files: Vec<String>,
    pub subtasks: Vec<SubtaskRecord>,
    #[serde(default)]
    pub unverified_subtasks: usize,
    pub blocked: bool,
    pub cancelled: bool,
}

/// Вычисляет детерминированный SHA-256 digest конкретного действия (task + tool + канонические аргументы).
pub fn compute_action_digest(task_id: &str, tool: &str, args: &Value) -> String {
    let canonical = canonical_json_string(args);
    let mut h = Sha256::new();
    h.update(task_id.as_bytes());
    h.update(b"\0");
    h.update(tool.as_bytes());
    h.update(b"\0");
    h.update(canonical.as_bytes());
    let bytes = h.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn canonical_json_string(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            let mut s = String::from("{");
            for (i, (k, val)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&serde_json::to_string(k).unwrap_or_default());
                s.push(':');
                s.push_str(&canonical_json_string(val));
            }
            s.push('}');
            s
        }
        Value::Array(arr) => {
            let mut s = String::from("[");
            for (i, val) in arr.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&canonical_json_string(val));
            }
            s.push(']');
            s
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

enum ApprovalMode {
    DenyAll,
    Callback(Arc<dyn Fn(PendingApproval) -> ApprovalDecision + Send + Sync>),
    Interactive {
        req_tx: Mutex<mpsc::Sender<PendingApproval>>,
        resp_rx: Mutex<mpsc::Receiver<ApprovalDecision>>,
    },
}

#[derive(Clone)]
pub struct ApprovalGate {
    mode: Arc<ApprovalMode>,
    consumed: Arc<Mutex<HashSet<String>>>,
}

pub struct ApprovalHandle {
    req_rx: mpsc::Receiver<PendingApproval>,
    resp_tx: mpsc::Sender<ApprovalDecision>,
}

impl ApprovalHandle {
    pub fn wait_request(&self, timeout: Duration) -> Option<PendingApproval> {
        self.req_rx.recv_timeout(timeout).ok()
    }

    pub fn try_recv_request(&self) -> Option<PendingApproval> {
        self.req_rx.try_recv().ok()
    }

    pub fn respond(&self, request_id: String, action_digest: String, approved: bool) {
        let _ = self.resp_tx.send(ApprovalDecision {
            request_id,
            action_digest,
            approved,
        });
    }
}

impl ApprovalGate {
    pub fn deny_all() -> Self {
        Self {
            mode: Arc::new(ApprovalMode::DenyAll),
            consumed: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn callback(
        f: impl Fn(PendingApproval) -> ApprovalDecision + Send + Sync + 'static,
    ) -> Self {
        Self {
            mode: Arc::new(ApprovalMode::Callback(Arc::new(f))),
            consumed: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn interactive() -> (Self, ApprovalHandle) {
        let (req_tx, req_rx) = mpsc::channel();
        let (resp_tx, resp_rx) = mpsc::channel();
        let gate = Self {
            mode: Arc::new(ApprovalMode::Interactive {
                req_tx: Mutex::new(req_tx),
                resp_rx: Mutex::new(resp_rx),
            }),
            consumed: Arc::new(Mutex::new(HashSet::new())),
        };
        (gate, ApprovalHandle { req_rx, resp_tx })
    }

    /// Проверяет точную привязку решения к `(request_id, action_digest)` и одноразово
    /// потребляет `request_id`, исключая повторное использование старых подтверждений.
    pub fn verify_and_consume(
        &self,
        req: &PendingApproval,
        dec: &ApprovalDecision,
    ) -> (bool, String) {
        let mut ledger = self.consumed.lock().unwrap_or_else(|p| p.into_inner());
        if ledger.contains(&dec.request_id) || ledger.contains(&req.request_id) {
            return (
                false,
                "Отклонено: попытка повторно использовать уже потреблённое подтверждение".into(),
            );
        }
        if dec.request_id != req.request_id {
            ledger.insert(req.request_id.clone());
            return (
                false,
                "Отклонено: несоответствие request_id подтверждения".into(),
            );
        }
        if dec.action_digest != req.action_digest {
            ledger.insert(req.request_id.clone());
            return (
                false,
                "Отклонено: несоответствие digest действия/аргументов подтверждения".into(),
            );
        }
        ledger.insert(req.request_id.clone());
        if dec.approved {
            (
                true,
                "Подтверждено пользователем (одноразовый токен действия)".into(),
            )
        } else {
            (false, "Отклонено пользователем".into())
        }
    }

    /// Ждёт решения по конкретному `request_id`; отмена сразу прекращает ожидание.
    pub async fn wait_for(
        &self,
        req: PendingApproval,
        cancel: &Cancel,
    ) -> Option<ApprovalDecision> {
        if cancel.is_cancelled() {
            return None;
        }
        match self.mode.as_ref() {
            ApprovalMode::DenyAll => Some(ApprovalDecision {
                request_id: req.request_id,
                action_digest: req.action_digest,
                approved: false,
            }),
            ApprovalMode::Callback(f) => Some(f(req)),
            ApprovalMode::Interactive { req_tx, resp_rx } => {
                if let Ok(tx) = req_tx.lock() {
                    let _ = tx.send(req.clone());
                }
                loop {
                    if cancel.is_cancelled() {
                        return None;
                    }
                    if let Ok(rx) = resp_rx.lock() {
                        match rx.try_recv() {
                            Ok(dec) => return Some(dec),
                            Err(mpsc::TryRecvError::Disconnected) => return None,
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    }
}

pub trait AgentModelBackend: Send {
    fn provider_kind(&self) -> ProviderKind;
    fn model_label(&self) -> &str;
    fn turn<'a>(
        &'a mut self,
        messages: &'a [Value],
        tools: &'a [Value],
        max_tokens: u32,
        cancel: &'a Cancel,
        on_event: &'a mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'a>>;
}

pub struct ManagedServerBackend<'a> {
    server: &'a mut ManagedServer,
    label: String,
}

impl<'a> ManagedServerBackend<'a> {
    pub fn new(server: &'a mut ManagedServer, label: String) -> Self {
        Self { server, label }
    }
}

impl<'a> AgentModelBackend for ManagedServerBackend<'a> {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::LocalLlama
    }
    fn model_label(&self) -> &str {
        &self.label
    }
    fn turn<'b>(
        &'b mut self,
        messages: &'b [Value],
        tools: &'b [Value],
        max_tokens: u32,
        cancel: &'b Cancel,
        on_event: &'b mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'b>> {
        Box::pin(async move {
            self.server
                .agent_turn(messages, tools, max_tokens, cancel, |ev| on_event(ev))
                .await
        })
    }
}

pub struct LocalHttpBackend {
    endpoint: LocalEndpoint,
    context: u32,
    label: String,
    grammar: Option<String>,
}

impl LocalHttpBackend {
    pub fn new(endpoint: LocalEndpoint, context: u32, label: String) -> Self {
        Self {
            endpoint,
            context,
            label,
            grammar: None,
        }
    }

    pub fn with_grammar(mut self, grammar: String) -> Self {
        self.grammar = Some(grammar);
        self
    }
}

impl AgentModelBackend for LocalHttpBackend {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::LocalLlama
    }
    fn model_label(&self) -> &str {
        &self.label
    }
    fn turn<'b>(
        &'b mut self,
        messages: &'b [Value],
        tools: &'b [Value],
        max_tokens: u32,
        cancel: &'b Cancel,
        on_event: &'b mut dyn FnMut(StreamEvent),
    ) -> Pin<Box<dyn Future<Output = Result<AgentTurnOutput>> + 'b>> {
        Box::pin(async move {
            stream_agent_turn_with_grammar(
                &self.endpoint,
                self.context,
                messages,
                tools,
                max_tokens,
                self.grammar.as_deref(),
                cancel,
                |ev| on_event(ev),
            )
            .await
        })
    }
}

/// Удобный вход с безопасной политикой исполнения по умолчанию:
/// - для `ReadOnly` → `ExecPolicy::SafeInspectOnly`
/// - для `ApprovalRequired` и `WorkspaceWrite` → `ExecPolicy::ConfirmUserCode`
///   (запись в workspace НЕ разрешает молчаливый запуск кода с правами пользователя).
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_with_backend(
    backend: &mut dyn AgentModelBackend,
    workspace: &Path,
    user_task: &str,
    prior_messages: &[Message],
    file_policy: AgentPolicy,
    limits: AgentLimits,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let exec_policy = match file_policy {
        AgentPolicy::ReadOnly => ExecPolicy::SafeInspectOnly,
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => ExecPolicy::ConfirmUserCode,
    };
    run_agent_with_policies(
        backend,
        workspace,
        user_task,
        prior_messages,
        file_policy,
        exec_policy,
        limits,
        cancel,
        approvals,
        emit,
    )
    .await
}

/// Явный вход с двумя раздельными политиками: файловой (`file_policy`) и исполнения кода (`exec_policy`).
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_with_policies(
    backend: &mut dyn AgentModelBackend,
    workspace: &Path,
    user_task: &str,
    prior_messages: &[Message],
    file_policy: AgentPolicy,
    exec_policy: ExecPolicy,
    limits: AgentLimits,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    mut emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let guard = WorkspaceGuard::new(workspace)?;
    let root_id = format!("task-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    run_agent_inner(
        backend,
        &guard,
        &root_id,
        0,
        "agent-1 (оркестратор)",
        user_task,
        prior_messages,
        file_policy,
        exec_policy,
        limits,
        true,
        None,
        cancel,
        approvals,
        &mut emit,
    )
    .await
}

#[derive(Clone, Debug)]
struct PassedCheckEvidence {
    summary: String,
    covered_paths: HashSet<String>,
    covered_stems: HashSet<String>,
    covers_rust_crate: bool,
    covers_all_python_tests: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_agent_inner<'a>(
    backend: &'a mut dyn AgentModelBackend,
    guard: &'a WorkspaceGuard,
    task_id: &'a str,
    seat_slot: u8,
    seat_label: &'a str,
    user_task: &'a str,
    prior_messages: &'a [Message],
    file_policy: AgentPolicy,
    exec_policy: ExecPolicy,
    limits: AgentLimits,
    allow_subagents: bool,
    owned_paths: Option<&'a [String]>,
    cancel: &'a Cancel,
    approvals: &'a ApprovalGate,
    emit: &'a mut dyn FnMut(AgentStreamEvent),
) -> Pin<Box<dyn Future<Output = Result<AgentRunResult>> + 'a>> {
    Box::pin(async move {
        let seat_num = (seat_slot as usize) + 1;
        let dyn_enabled = limits.dynamic_interaction_enabled;
        let mut live_seat = AgentSeat {
            seat_id: format!("seat-{seat_num}"),
            agent_name: seat_label.to_string(),
            color_slot: seat_slot % (MAX_AGENT_SEATS as u8),
            owned_paths: owned_paths.map(|p| p.to_vec()).unwrap_or_default(),
            pointer: VirtualPointer {
                enabled: dyn_enabled,
                ..VirtualPointer::default()
            },
            keyboard: VirtualKeyboard {
                keyboard_id: format!("kbd-{seat_num}"),
                enabled: dyn_enabled && file_policy != AgentPolicy::ReadOnly,
                typing: false,
                last_keys: String::new(),
                ops_count: 0,
            },
            control_live: false,
            status: "запущен".into(),
            last_op_tsc: asm_accel::asm_read_tsc(),
        };
        emit(AgentStreamEvent::SeatUpdate(live_seat.clone()));
        let mut wire_messages = build_initial_messages(
            guard.root(),
            file_policy,
            exec_policy,
            allow_subagents,
            owned_paths,
            prior_messages,
            user_task,
        );
        let tools_schema = tool_definitions(allow_subagents);
        let allowed_tool_names: Vec<&str> = tools_schema
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        let start = Instant::now();
        let mut events = Vec::new();
        let mut changed_files = Vec::new();
        let mut subtasks: Vec<SubtaskRecord> = Vec::new();
        // Карта: относительный путь -> точный SHA-256, который оркестратор прочитал ПОСЛЕ делегации.
        let mut post_subtask_reads: HashMap<String, String> = HashMap::new();
        // Реальные проверки сборки/тестов, прошедшие ПОСЛЕ делегации и после последних правок.
        let mut post_subtask_checks: Vec<PassedCheckEvidence> = Vec::new();
        let mut failure_counts: HashMap<String, usize> = HashMap::new();
        let mut call_repeat_counts: HashMap<String, usize> = HashMap::new();
        let mut recent_call_sigs: Vec<String> = Vec::new();
        let mut total_tool_calls: usize = 0;
        let mut compactions: u32 = 0;
        let mut last_check_summary: Option<String> = None;
        let mut final_text = String::new();

        for step in 1..=limits.max_steps {
            if let Some(compacted) = context::compact_wire_messages(
                &mut wire_messages,
                limits.max_context_tokens,
                &changed_files,
                last_check_summary.as_deref(),
                false,
            ) {
                compactions += 1;
                emit(AgentStreamEvent::Status(format!(
                    "Контекст сжат: −{} сообщений ({}→{} ток)",
                    compacted.dropped_messages, compacted.tokens_before, compacted.tokens_after
                )));
            }
            let ctx_metrics =
                context::context_metrics(&wire_messages, limits.max_context_tokens, compactions);
            emit(AgentStreamEvent::ContextUsage(ctx_metrics));
            save_checkpoint(
                limits.checkpoint_dir.as_deref(),
                task_id,
                guard,
                backend.provider_kind(),
                backend.model_label(),
                step,
                "running",
                &changed_files,
                ctx_metrics,
                events.len(),
            );

            if cancel.is_cancelled() {
                let ev = AgentEvent::Cancelled {
                    summary: format!("Задача остановлена на шаге {step}"),
                    changed_files: changed_files.clone(),
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
                revalidate_disk_hashes(guard, &mut subtasks, emit);
                let unverified = subtasks.iter().filter(|s| !s.verified).count();
                return Ok(AgentRunResult {
                    final_text: if final_text.is_empty() {
                        "Агент остановлен пользователем; уже выполненные изменения сохранены."
                            .into()
                    } else {
                        final_text
                    },
                    events,
                    changed_files,
                    subtasks,
                    unverified_subtasks: unverified,
                    blocked: false,
                    cancelled: true,
                });
            }
            if start.elapsed() > Duration::from_secs(limits.max_wall_secs) {
                let reason = format!(
                    "Достигнут лимит времени работы агента ({} с); выполнение остановлено.",
                    limits.max_wall_secs
                );
                let ev = AgentEvent::Blocked {
                    reason: reason.clone(),
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
                revalidate_disk_hashes(guard, &mut subtasks, emit);
                let unverified = subtasks.iter().filter(|s| !s.verified).count();
                return Ok(AgentRunResult {
                    final_text: reason,
                    events,
                    changed_files,
                    subtasks,
                    unverified_subtasks: unverified,
                    blocked: true,
                    cancelled: false,
                });
            }

            let step_ev = AgentEvent::Step {
                step,
                max_steps: limits.max_steps,
                provider: backend.provider_kind(),
                model: backend.model_label().to_string(),
                summary: format!("Шаг {step}/{}", limits.max_steps),
            };
            events.push(step_ev.clone());
            emit(AgentStreamEvent::Event(step_ev));
            emit(AgentStreamEvent::Status(format!(
                "Агент · шаг {step}/{}…",
                limits.max_steps
            )));

            let mut step_text = String::new();
            let mut on_stream = |ev: StreamEvent| match ev {
                StreamEvent::Text(s) => {
                    step_text.push_str(&s);
                    if allow_subagents {
                        emit(AgentStreamEvent::TextDelta(s));
                    }
                }
                StreamEvent::Timings {
                    tokens,
                    tps,
                    seconds,
                } => {
                    if allow_subagents {
                        emit(AgentStreamEvent::Metrics {
                            tokens,
                            tps,
                            seconds,
                        });
                    }
                }
                StreamEvent::Reasoning(_)
                | StreamEvent::ToolCallDelta { .. }
                | StreamEvent::ToolCallsDone
                | StreamEvent::Limit
                | StreamEvent::Done => {}
            };

            let mut turn = match backend
                .turn(&wire_messages, &tools_schema, 1536, cancel, &mut on_stream)
                .await
            {
                Ok(t) => t,
                Err(e) if cancel.is_cancelled() => {
                    let ev = AgentEvent::Cancelled {
                        summary: format!("Остановлено пользователем: {e:#}"),
                        changed_files: changed_files.clone(),
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                    revalidate_disk_hashes(guard, &mut subtasks, emit);
                    let unverified = subtasks.iter().filter(|s| !s.verified).count();
                    return Ok(AgentRunResult {
                        final_text: if final_text.is_empty() {
                            "Агент остановлен пользователем.".into()
                        } else {
                            final_text
                        },
                        events,
                        changed_files,
                        subtasks,
                        unverified_subtasks: unverified,
                        blocked: false,
                        cancelled: true,
                    });
                }
                Err(e) => return Err(e),
            };

            if turn.tool_calls.is_empty() && !turn.text.trim().is_empty() {
                let recovered = tool_parser::extract_fallback_tool_calls(
                    backend.model_label(),
                    &turn.text,
                    &allowed_tool_names,
                );
                if !recovered.is_empty() {
                    turn.tool_calls = recovered;
                }
            }

            if turn.tool_calls.is_empty() {
                final_text = turn.text.trim().to_string();
                live_seat.control_live = false;
                live_seat.pointer.active = false;
                live_seat.keyboard.typing = false;
                live_seat.status = "завершено".into();
                live_seat.last_op_tsc = asm_accel::asm_read_tsc();
                emit(AgentStreamEvent::SeatUpdate(live_seat.clone()));
                save_checkpoint(
                    limits.checkpoint_dir.as_deref(),
                    task_id,
                    guard,
                    backend.provider_kind(),
                    backend.model_label(),
                    step,
                    "completed",
                    &changed_files,
                    ctx_metrics,
                    events.len() + 1,
                );
                // Перед финальным ответом перепроверяем, что ни один подтверждённый файл не изменился на диске.
                revalidate_disk_hashes(guard, &mut subtasks, emit);
                let unverified = subtasks.iter().filter(|s| !s.verified).count();
                let summary = if unverified > 0 {
                    format!(
                        "Завершено за {step} шаг(а/ов); изменено файлов: {}; неподтверждённых подзадач: {unverified}",
                        changed_files.len()
                    )
                } else {
                    format!(
                        "Завершено за {step} шаг(а/ов); изменено файлов: {}",
                        changed_files.len()
                    )
                };
                let done_ev = AgentEvent::Completed {
                    summary,
                    changed_files: changed_files.clone(),
                };
                events.push(done_ev.clone());
                emit(AgentStreamEvent::Event(done_ev));
                return Ok(AgentRunResult {
                    final_text,
                    events,
                    changed_files,
                    subtasks,
                    unverified_subtasks: unverified,
                    blocked: false,
                    cancelled: false,
                });
            }

            let tc_json: Vec<Value> = turn
                .tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                })
                .collect();
            let mut assistant_msg = json!({
                "role": "assistant",
                "tool_calls": tc_json,
            });
            if !turn.text.trim().is_empty() {
                assistant_msg["content"] = json!(turn.text);
            } else {
                assistant_msg["content"] = Value::Null;
            }
            wire_messages.push(assistant_msg);

            for tc in &turn.tool_calls {
                total_tool_calls += 1;
                if total_tool_calls > limits.max_total_tool_calls {
                    let reason = format!(
                        "Превышен бюджет числа вызовов инструментов ({}). Изменено файлов: {}.",
                        limits.max_total_tool_calls,
                        changed_files.len()
                    );
                    let ev = AgentEvent::Blocked {
                        reason: reason.clone(),
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                    revalidate_disk_hashes(guard, &mut subtasks, emit);
                    let unverified = subtasks.iter().filter(|s| !s.verified).count();
                    return Ok(AgentRunResult {
                        final_text: reason,
                        events,
                        changed_files,
                        subtasks,
                        unverified_subtasks: unverified,
                        blocked: true,
                        cancelled: false,
                    });
                }
                if cancel.is_cancelled() {
                    let ev = AgentEvent::Cancelled {
                        summary: "Остановлено пользователем перед вызовом инструмента".into(),
                        changed_files: changed_files.clone(),
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                    revalidate_disk_hashes(guard, &mut subtasks, emit);
                    let unverified = subtasks.iter().filter(|s| !s.verified).count();
                    return Ok(AgentRunResult {
                        final_text: "Агент остановлен пользователем.".into(),
                        events,
                        changed_files,
                        subtasks,
                        unverified_subtasks: unverified,
                        blocked: false,
                        cancelled: true,
                    });
                }

                let prev_subtasks_len = subtasks.len();
                let exec = execute_tool_call(
                    tc,
                    &tools_schema,
                    backend,
                    guard,
                    task_id,
                    file_policy,
                    exec_policy,
                    allow_subagents,
                    owned_paths,
                    limits.max_tool_output_bytes,
                    cancel,
                    approvals,
                    &mut events,
                    &mut subtasks,
                    emit,
                )
                .await?;

                if exec.cancelled {
                    let ev = AgentEvent::Cancelled {
                        summary: format!("Остановлено во время инструмента {}", tc.name),
                        changed_files: changed_files.clone(),
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                    revalidate_disk_hashes(guard, &mut subtasks, emit);
                    let unverified = subtasks.iter().filter(|s| !s.verified).count();
                    return Ok(AgentRunResult {
                        final_text:
                            "Агент остановлен пользователем; уже записанные изменения сохранены."
                                .into(),
                        events,
                        changed_files,
                        subtasks,
                        unverified_subtasks: unverified,
                        blocked: false,
                        cancelled: true,
                    });
                }

                for f in &exec.execution.changed_files {
                    if !changed_files.iter().any(|x| x == f) {
                        changed_files.push(f.clone());
                    }
                }

                // Обновляем курсор/клавиатуру текущего агентного места (динамический live-режим активен только при explicit opt-in).
                if exec.execution.ok {
                    if let Some((read_rel, _)) = &exec.inspected_file {
                        live_seat.pointer.target_file = Some(read_rel.clone());
                        live_seat.pointer.active = dyn_enabled;
                        live_seat.keyboard.typing = false;
                        live_seat.control_live = false;
                        live_seat.status = format!("читает {read_rel}");
                        live_seat.last_op_tsc = asm_accel::asm_read_tsc();
                        emit(AgentStreamEvent::SeatUpdate(live_seat.clone()));
                    } else if let Some(edited_rel) = exec.execution.changed_files.first() {
                        live_seat.pointer.target_file = Some(edited_rel.clone());
                        live_seat.pointer.active = dyn_enabled;
                        live_seat.keyboard.typing = dyn_enabled;
                        live_seat.keyboard.ops_count += 1;
                        live_seat.keyboard.last_keys = clip_utf8(&exec.execution.summary, 64);
                        live_seat.control_live = dyn_enabled;
                        live_seat.status = format!("правит {edited_rel}");
                        live_seat.last_op_tsc = asm_accel::asm_read_tsc();
                        emit(AgentStreamEvent::SeatUpdate(live_seat.clone()));
                    }
                }

                // 1) Новая делегация сбрасывает прежние свидетельства.
                if subtasks.len() > prev_subtasks_len {
                    post_subtask_reads.clear();
                    post_subtask_checks.clear();
                } else if allow_subagents && !subtasks.is_empty() {
                    // 2) Любая последующая правка файла оркестратором немедленно аннулирует
                    //    старые свидетельства и верификацию затронутых подзадач!
                    if exec.execution.ok && !exec.execution.changed_files.is_empty() {
                        for mutated in &exec.execution.changed_files {
                            let norm = subagents::normalize_rel(mutated);
                            post_subtask_reads.remove(&norm);
                            invalidate_subtasks_for_file(&mut subtasks, &norm, emit);
                        }
                        // Старые прогоны проверок могли опираться на старое содержимое диска.
                        post_subtask_checks.clear();
                    }
                    // 3) Фиксируем точный SHA-256 файла при чтении и область покрытия при успешной проверке.
                    if exec.execution.ok {
                        if let Some((read_rel, read_sha)) = &exec.inspected_file {
                            post_subtask_reads.insert(read_rel.clone(), read_sha.clone());
                        }
                        if let Some(check_ev) = exec.passed_check {
                            post_subtask_checks.push(check_ev);
                        }
                        reconcile_subtask_verification(
                            guard,
                            &mut subtasks,
                            &post_subtask_reads,
                            &post_subtask_checks,
                            emit,
                        );
                    }
                }

                if tc.name == "run_check" {
                    last_check_summary = Some(exec.execution.summary.clone());
                }

                let mut tool_text = exec.execution.output.clone();
                if tc.name == "run_check" && !exec.execution.ok {
                    tool_text.push_str("\n[KAIC] Проверка завершилась с ошибкой. Прочитай проблемный файл через read_file, исправь причину через edit_file/apply_patch и повтори run_check.");
                }
                let clipped_output = clip_utf8(&tool_text, limits.max_tool_output_bytes);
                let res_ev = AgentEvent::ToolResult {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    ok: exec.execution.ok,
                    summary: exec.execution.summary.clone(),
                    output_preview: clip_utf8(&clipped_output, 800),
                    changed_files: exec.execution.changed_files.clone(),
                    diff_preview: exec.execution.diff_preview.clone(),
                };
                events.push(res_ev.clone());
                emit(AgentStreamEvent::Event(res_ev));

                wire_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": clipped_output,
                }));

                let sig = format!("{}:{}", tc.name, tc.arguments.trim());
                recent_call_sigs.push(sig.clone());
                let rep = call_repeat_counts.entry(sig.clone()).or_insert(0);
                *rep += 1;
                let oscillating = recent_call_sigs.len() >= 6
                    && recent_call_sigs[recent_call_sigs.len() - 1]
                        == recent_call_sigs[recent_call_sigs.len() - 3]
                    && recent_call_sigs[recent_call_sigs.len() - 2]
                        == recent_call_sigs[recent_call_sigs.len() - 4]
                    && recent_call_sigs[recent_call_sigs.len() - 3]
                        == recent_call_sigs[recent_call_sigs.len() - 5]
                    && recent_call_sigs[recent_call_sigs.len() - 4]
                        == recent_call_sigs[recent_call_sigs.len() - 6];

                if oscillating || *rep >= (limits.max_repeat_failures * 2).max(5) {
                    let reason = format!(
                        "Обнаружен зацикленный повтор вызовов инструмента «{}»; выполнение остановлено.",
                        tc.name
                    );
                    let ev = AgentEvent::Blocked {
                        reason: reason.clone(),
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                    revalidate_disk_hashes(guard, &mut subtasks, emit);
                    let unverified = subtasks.iter().filter(|s| !s.verified).count();
                    return Ok(AgentRunResult {
                        final_text: reason,
                        events,
                        changed_files,
                        subtasks,
                        unverified_subtasks: unverified,
                        blocked: true,
                        cancelled: false,
                    });
                }

                if exec.execution.ok {
                    failure_counts.remove(&sig);
                } else {
                    let count = failure_counts.entry(sig).or_insert(0);
                    *count += 1;
                    if *count >= limits.max_repeat_failures {
                        let reason = format!(
                            "Остановлен повторяющийся безрезультатный цикл: инструмент «{}» завершился ошибкой {} раз подряд ({})",
                            tc.name, *count, exec.execution.summary
                        );
                        let ev = AgentEvent::Blocked {
                            reason: reason.clone(),
                        };
                        events.push(ev.clone());
                        emit(AgentStreamEvent::Event(ev));
                        revalidate_disk_hashes(guard, &mut subtasks, emit);
                        let unverified = subtasks.iter().filter(|s| !s.verified).count();
                        return Ok(AgentRunResult {
                            final_text: reason,
                            events,
                            changed_files,
                            subtasks,
                            unverified_subtasks: unverified,
                            blocked: true,
                            cancelled: false,
                        });
                    }
                }
            }
        }

        let reason = format!(
            "Достигнут лимит шагов агента ({}). Изменено файлов: {}.",
            limits.max_steps,
            changed_files.len()
        );
        let ev = AgentEvent::Blocked {
            reason: reason.clone(),
        };
        events.push(ev.clone());
        emit(AgentStreamEvent::Event(ev));
        revalidate_disk_hashes(guard, &mut subtasks, emit);
        let unverified = subtasks.iter().filter(|s| !s.verified).count();
        Ok(AgentRunResult {
            final_text: reason,
            events,
            changed_files,
            subtasks,
            unverified_subtasks: unverified,
            blocked: true,
            cancelled: false,
        })
    })
}

/// Аннулирует верификацию любой подзадачи, связанной с изменённым файлом.
fn invalidate_subtasks_for_file(
    subtasks: &mut [SubtaskRecord],
    mutated_rel: &str,
    emit: &mut dyn FnMut(AgentStreamEvent),
) {
    for st in subtasks.iter_mut() {
        let touches = st
            .changed_files
            .iter()
            .any(|f| subagents::normalize_rel(f) == mutated_rel)
            || st.verified_file_hashes.contains_key(mutated_rel)
            || st.artifact_hashes.contains_key(mutated_rel)
            || st
                .owned_paths
                .iter()
                .any(|p| subagents::is_path_owned(p, mutated_rel));
        if touches && (st.verified || st.status == "verified") {
            st.verified = false;
            st.status = "invalidated_after_edit".into();
            st.verification_evidence = None;
            st.verified_file_hashes.clear();
            emit(AgentStreamEvent::Subtask(st.clone()));
        }
    }
}

/// Перед возвратом итога проверяет, что на диске сохранились именно те SHA-256 версии файлов,
/// к которым относилось `verified = true`. Любое расхождение аннулирует верификацию.
fn revalidate_disk_hashes(
    guard: &WorkspaceGuard,
    subtasks: &mut [SubtaskRecord],
    emit: &mut dyn FnMut(AgentStreamEvent),
) {
    for st in subtasks.iter_mut() {
        if !st.verified {
            continue;
        }
        let still_matches = st.verified_file_hashes.iter().all(|(rel, expected_sha)| {
            guard
                .hash_file_no_follow(rel)
                .is_ok_and(|(_, cur_sha)| &cur_sha == expected_sha)
        });
        if !still_matches {
            st.verified = false;
            st.status = "invalidated_after_edit".into();
            st.verification_evidence = None;
            st.verified_file_hashes.clear();
            emit(AgentStreamEvent::Subtask(st.clone()));
        }
    }
}

/// Определяет, какие конкретно файлы/модули реально покрывает прошедшая проверка,
/// чтобы случайный успешный тест не подтверждал чужие подзадачи.
fn compute_check_coverage(
    guard: &WorkspaceGuard,
    validated: &ValidatedCommand,
    summary: String,
) -> Option<PassedCheckEvidence> {
    if !validated.is_verification_check {
        return None;
    }
    let mut covered_paths = HashSet::new();
    let mut covered_stems = HashSet::new();
    let mut covers_rust_crate = false;
    let mut covers_all_python_tests = false;

    match validated.program.as_str() {
        "python3" => {
            if validated.raw_args.get(1).map(String::as_str) == Some("py_compile") {
                for f in validated.raw_args.iter().skip(2) {
                    let norm = subagents::normalize_rel(f);
                    if let Some(stem) = Path::new(&norm).file_stem().and_then(|s| s.to_str()) {
                        covered_stems.insert(stem.to_string());
                    }
                    covered_paths.insert(norm);
                }
            } else if matches!(
                validated.raw_args.get(1).map(String::as_str),
                Some("unittest" | "pytest")
            ) {
                let targets: Vec<&String> = validated
                    .raw_args
                    .iter()
                    .skip(2)
                    .filter(|a| !a.starts_with('-'))
                    .collect();
                if targets.is_empty() || targets.iter().any(|t| t.as_str() == "discover") {
                    covers_all_python_tests = true;
                } else {
                    for t in targets {
                        extract_python_test_coverage(
                            guard,
                            t,
                            &mut covered_paths,
                            &mut covered_stems,
                        );
                    }
                }
            }
        }
        "pytest" => {
            let targets: Vec<&String> = validated
                .raw_args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .collect();
            if targets.is_empty() {
                covers_all_python_tests = true;
            } else {
                for t in targets {
                    extract_python_test_coverage(guard, t, &mut covered_paths, &mut covered_stems);
                }
            }
        }
        "cargo" => {
            covers_rust_crate = true;
        }
        "make" | "cmake" | "ctest" | "ninja" | "npm" | "pnpm" | "yarn" | "rustc" => {
            for a in &validated.raw_args {
                if !a.starts_with('-') {
                    let norm = subagents::normalize_rel(a);
                    if let Some(stem) = Path::new(&norm).file_stem().and_then(|s| s.to_str()) {
                        covered_stems.insert(stem.to_string());
                    }
                    covered_paths.insert(norm);
                }
            }
            // Если в workspace есть Makefile/package.json, считываем упомянутые там целевые файлы.
            for build_file in ["Makefile", "GNUmakefile", "package.json", "CMakeLists.txt"] {
                if let Ok((f, _)) = guard.open_read_no_follow(build_file) {
                    let mut txt = String::new();
                    let _ = f.take(64 * 1024).read_to_string(&mut txt);
                    for tok in txt.split(|c: char| {
                        !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '/' && c != '-'
                    }) {
                        if tok.contains('.') && !tok.starts_with('.') {
                            covered_paths.insert(subagents::normalize_rel(tok));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    Some(PassedCheckEvidence {
        summary,
        covered_paths,
        covered_stems,
        covers_rust_crate,
        covers_all_python_tests,
    })
}

fn extract_python_test_coverage(
    guard: &WorkspaceGuard,
    target: &str,
    covered_paths: &mut HashSet<String>,
    covered_stems: &mut HashSet<String>,
) {
    let candidate_file = if target.ends_with(".py") {
        subagents::normalize_rel(target)
    } else {
        format!("{}.py", target.replace('.', "/"))
    };
    covered_paths.insert(candidate_file.clone());
    if let Ok((f, _)) = guard.open_read_no_follow(&candidate_file) {
        let mut txt = String::new();
        let _ = f.take(64 * 1024).read_to_string(&mut txt);
        for line in txt.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("from ")
                && let Some((module, _)) = rest.split_once(" import ")
            {
                let stem = module
                    .trim()
                    .trim_start_matches('.')
                    .split('.')
                    .next()
                    .unwrap_or("");
                if !stem.is_empty() {
                    covered_stems.insert(stem.to_string());
                }
            } else if let Some(rest) = t.strip_prefix("import ") {
                for part in rest.split(',') {
                    let module = part.trim().split_whitespace().next().unwrap_or("");
                    let stem = module.split('.').next().unwrap_or("");
                    if !stem.is_empty() {
                        covered_stems.insert(stem.to_string());
                    }
                }
            }
        }
    }
}

fn check_covers_file(check: &PassedCheckEvidence, rel_file: &str) -> bool {
    let norm = subagents::normalize_rel(rel_file);
    if check.covered_paths.contains(&norm) {
        return true;
    }
    let ext = Path::new(&norm)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext == "rs" && check.covers_rust_crate {
        return true;
    }
    if ext == "py" {
        if check.covers_all_python_tests {
            return true;
        }
        if let Some(stem) = Path::new(&norm).file_stem().and_then(|s| s.to_str())
            && check.covered_stems.contains(stem)
        {
            return true;
        }
    }
    false
}

/// Переводит подзадачу в `verified = true` ТОЛЬКО при одновременном выполнении:
/// 1) Текущие SHA-256 на диске совпадают с версией, созданной подзадачей (`artifact_hashes`).
/// 2) Оркестратор сам прочитал через `read_file` именно эту SHA-256 версию каждого файла.
/// 3) Для пишущих подзадач прошла реальная целевая проверка (`PassedCheckEvidence`),
///    которая фактически покрывает КАЖДЫЙ изменённый файл этой подзадачи.
fn reconcile_subtask_verification(
    guard: &WorkspaceGuard,
    subtasks: &mut [SubtaskRecord],
    post_reads: &HashMap<String, String>,
    post_checks: &[PassedCheckEvidence],
    emit: &mut dyn FnMut(AgentStreamEvent),
) {
    for st in subtasks.iter_mut() {
        if st.verified || st.status != "completed_unverified" {
            continue;
        }
        if !st.changed_files.is_empty() {
            let mut verified_hashes = BTreeMap::new();
            let mut all_version_matched = true;
            for f in &st.changed_files {
                let norm = subagents::normalize_rel(f);
                let Ok((_, disk_sha)) = guard.hash_file_no_follow(&norm) else {
                    all_version_matched = false;
                    break;
                };
                if let Some(expected_sha) = st.artifact_hashes.get(&norm)
                    && expected_sha != &disk_sha
                {
                    all_version_matched = false;
                    break;
                }
                if post_reads.get(&norm) != Some(&disk_sha) {
                    all_version_matched = false;
                    break;
                }
                verified_hashes.insert(norm, disk_sha);
            }
            if !all_version_matched {
                continue;
            }
            let matching_check = post_checks
                .iter()
                .rev()
                .find(|chk| st.changed_files.iter().all(|f| check_covers_file(chk, f)));
            if let Some(chk) = matching_check {
                st.verified = true;
                st.status = "verified".into();
                st.verified_file_hashes = verified_hashes;
                st.verification_evidence = Some(format!(
                    "оркестратор прочитал текущие SHA-256 {:?} и подтвердил целевой проверкой: {}",
                    st.changed_files, chk.summary
                ));
                emit(AgentStreamEvent::Subtask(st.clone()));
            }
        } else if !st.owned_paths.is_empty() {
            let mut verified_hashes = BTreeMap::new();
            let mut all_read = true;
            for p in &st.owned_paths {
                let norm = subagents::normalize_rel(p);
                let Ok((_, disk_sha)) = guard.hash_file_no_follow(&norm) else {
                    all_read = false;
                    break;
                };
                if post_reads.get(&norm) != Some(&disk_sha) {
                    all_read = false;
                    break;
                }
                verified_hashes.insert(norm, disk_sha);
            }
            if all_read {
                st.verified = true;
                st.status = "verified".into();
                st.verified_file_hashes = verified_hashes;
                st.verification_evidence = Some(format!(
                    "оркестратор независимо перечитал текущие SHA-256 {:?}",
                    st.owned_paths
                ));
                emit(AgentStreamEvent::Subtask(st.clone()));
            }
        }
    }
}

struct DispatchedTool {
    execution: ToolExecution,
    cancelled: bool,
    inspected_file: Option<(String, String)>,
    passed_check: Option<PassedCheckEvidence>,
}

#[allow(clippy::too_many_arguments)]
fn save_checkpoint(
    dir: Option<&Path>,
    task_id: &str,
    guard: &WorkspaceGuard,
    provider: ProviderKind,
    model: &str,
    step: usize,
    status: &str,
    changed_files: &[String],
    context_metrics: ContextMetrics,
    events_count: usize,
) {
    let Some(dir) = dir else {
        return;
    };
    let clean_id: String = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean_id.is_empty() {
        return;
    }
    let path = dir.join(format!("{clean_id}.json"));
    let cp = AgentTaskCheckpoint {
        task_id: task_id.to_string(),
        workspace: guard.root().display().to_string(),
        provider,
        model: model.to_string(),
        step,
        status: status.to_string(),
        changed_files: changed_files.to_vec(),
        context_metrics,
        events_count,
        updated_epoch_secs: crate::storage::epoch_now().unwrap_or(0),
    };
    let _ = crate::storage::save_json(&path, &cp);
}

#[allow(clippy::too_many_arguments)]
async fn execute_tool_call(
    tc: &ToolCall,
    tools_schema: &[Value],
    backend: &mut dyn AgentModelBackend,
    guard: &WorkspaceGuard,
    task_id: &str,
    file_policy: AgentPolicy,
    exec_policy: ExecPolicy,
    allow_subagents: bool,
    owned_paths: Option<&[String]>,
    _max_out: usize,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    events: &mut Vec<AgentEvent>,
    subtasks_out: &mut Vec<SubtaskRecord>,
    emit: &mut dyn FnMut(AgentStreamEvent),
) -> Result<DispatchedTool> {
    let args: Value =
        match tool_parser::repair_and_validate_args(&tc.name, &tc.arguments, tools_schema) {
            Ok(v) => v,
            Err(msg) => {
                return Ok(DispatchedTool {
                    execution: ToolExecution {
                        ok: false,
                        summary: format!("Ошибка аргументов {}", tc.name),
                        output: msg,
                        changed_files: vec![],
                        diff_preview: None,
                    },
                    cancelled: false,
                    inspected_file: None,
                    passed_check: None,
                });
            }
        };

    let args_preview = clip_utf8(&tc.arguments, 220);
    let call_ev = AgentEvent::ToolCall {
        id: tc.id.clone(),
        name: tc.name.clone(),
        args_summary: args_preview,
        policy_note: format!(
            "files:{} · exec:{}",
            file_policy.label_en(),
            exec_policy.label_en()
        ),
    };
    events.push(call_ev.clone());
    emit(AgentStreamEvent::Event(call_ev));
    emit(AgentStreamEvent::Status(format!(
        "Инструмент: {}…",
        tc.name
    )));

    match tc.name.as_str() {
        "list_files" => Ok(DispatchedTool {
            execution: unwrap_tool(tools::exec_list_files(guard, &args), "list_files"),
            cancelled: false,
            inspected_file: None,
            passed_check: None,
        }),
        "read_file" => {
            let execution = unwrap_tool(tools::exec_read_file(guard, &args), "read_file");
            let inspected = if execution.ok {
                args["path"]
                    .as_str()
                    .and_then(|raw| guard.hash_file_no_follow(raw).ok())
                    .map(|(rel, sha)| (subagents::normalize_rel(&rel), sha))
            } else {
                None
            };
            Ok(DispatchedTool {
                execution,
                cancelled: false,
                inspected_file: inspected,
                passed_check: None,
            })
        }
        "search_text" => Ok(DispatchedTool {
            execution: unwrap_tool(tools::exec_search_text(guard, &args), "search_text"),
            cancelled: false,
            inspected_file: None,
            passed_check: None,
        }),
        "write_file" => {
            let raw_path = args["path"].as_str().unwrap_or("");
            let target_check = match guard.resolve_write_target(raw_path) {
                Ok((_, rel)) => rel,
                Err(e) => return Ok(denied_tool("write_file", format!("{e:#}"))),
            };
            if let Err(msg) = check_owned_write(owned_paths, &target_check) {
                return Ok(denied_tool("write_file", msg));
            }
            if file_policy == AgentPolicy::ReadOnly {
                return Ok(denied_tool(
                    "write_file",
                    format!("Файловая политика read-only запрещает запись файла {target_check}"),
                ));
            }
            if file_policy == AgentPolicy::ApprovalRequired {
                let canonical_args = json!({
                    "path": target_check,
                    "content": args["content"].as_str().unwrap_or(""),
                });
                let digest = compute_action_digest(task_id, "write_file", &canonical_args);
                match request_approval(
                    approvals,
                    cancel,
                    events,
                    emit,
                    "write_file",
                    digest,
                    IsolationLevel::WorkspaceFdNoFollow,
                    format!("Запись файла {target_check}"),
                    target_check.clone(),
                    format!(
                        "Будет создан или перезаписан файл {target_check} внутри workspace (kernel openat O_NOFOLLOW)."
                    ),
                )
                .await
                {
                    ApprovalOutcome::Approved => {}
                    ApprovalOutcome::Denied(reason) => {
                        return Ok(denied_tool("write_file", reason));
                    }
                    ApprovalOutcome::Cancelled => {
                        return Ok(cancelled_tool());
                    }
                }
            }
            Ok(DispatchedTool {
                execution: unwrap_tool(tools::exec_write_file(guard, &args), "write_file"),
                cancelled: false,
                inspected_file: None,
                passed_check: None,
            })
        }
        "edit_file" => {
            let raw_path = args["path"].as_str().unwrap_or("");
            let target_check = match guard.relative_segments(raw_path, true) {
                Ok(segs) if !segs.is_empty() => segs.join("/"),
                Ok(_) => return Ok(denied_tool("edit_file", "Путь к файлу пуст".into())),
                Err(e) => return Ok(denied_tool("edit_file", format!("{e:#}"))),
            };
            if let Err(msg) = check_owned_write(owned_paths, &target_check) {
                return Ok(denied_tool("edit_file", msg));
            }
            if file_policy == AgentPolicy::ReadOnly {
                return Ok(denied_tool(
                    "edit_file",
                    format!("Файловая политика read-only запрещает правку файла {target_check}"),
                ));
            }
            if file_policy == AgentPolicy::ApprovalRequired {
                let canonical_args = json!({
                    "path": target_check,
                    "old_text": args["old_text"].as_str().unwrap_or(""),
                    "new_text": args["new_text"].as_str().unwrap_or(""),
                });
                let digest = compute_action_digest(task_id, "edit_file", &canonical_args);
                match request_approval(
                    approvals,
                    cancel,
                    events,
                    emit,
                    "edit_file",
                    digest,
                    IsolationLevel::WorkspaceFdNoFollow,
                    format!("Правка файла {target_check}"),
                    target_check.clone(),
                    format!(
                        "Будет выполнена точечная замена текста в {target_check} внутри workspace (kernel openat O_NOFOLLOW)."
                    ),
                )
                .await
                {
                    ApprovalOutcome::Approved => {}
                    ApprovalOutcome::Denied(reason) => {
                        return Ok(denied_tool("edit_file", reason));
                    }
                    ApprovalOutcome::Cancelled => {
                        return Ok(cancelled_tool());
                    }
                }
            }
            Ok(DispatchedTool {
                execution: unwrap_tool(tools::exec_edit_file(guard, &args), "edit_file"),
                cancelled: false,
                inspected_file: None,
                passed_check: None,
            })
        }
        "apply_patch" => {
            if file_policy == AgentPolicy::ReadOnly {
                return Ok(denied_tool(
                    "apply_patch",
                    "Файловая политика read-only запрещает применение патчей".into(),
                ));
            }
            if file_policy == AgentPolicy::ApprovalRequired {
                let patch_str = args["patch"].as_str().unwrap_or("");
                let canonical_args = json!({ "patch": patch_str });
                let digest = compute_action_digest(task_id, "apply_patch", &canonical_args);
                let target_preview: String = patch_str
                    .lines()
                    .find(|l| l.starts_with("+++ "))
                    .unwrap_or("patch")
                    .chars()
                    .take(120)
                    .collect();
                match request_approval(
                    approvals,
                    cancel,
                    events,
                    emit,
                    "apply_patch",
                    digest,
                    IsolationLevel::WorkspaceFdNoFollow,
                    format!("Применение unified diff патча ({target_preview})"),
                    target_preview,
                    "Патч будет применён внутри workspace с проверкой границ и kernel openat O_NOFOLLOW."
                        .into(),
                )
                .await
                {
                    ApprovalOutcome::Approved => {}
                    ApprovalOutcome::Denied(reason) => {
                        return Ok(denied_tool("apply_patch", reason));
                    }
                    ApprovalOutcome::Cancelled => {
                        return Ok(cancelled_tool());
                    }
                }
            }
            Ok(DispatchedTool {
                execution: unwrap_tool(
                    tools::exec_apply_patch(guard, &args, owned_paths),
                    "apply_patch",
                ),
                cancelled: false,
                inspected_file: None,
                passed_check: None,
            })
        }
        "list_skills" => Ok(DispatchedTool {
            execution: unwrap_tool(tools::exec_list_skills(guard), "list_skills"),
            cancelled: false,
            inspected_file: None,
            passed_check: None,
        }),
        "read_skill" => Ok(DispatchedTool {
            execution: unwrap_tool(tools::exec_read_skill(guard, &args), "read_skill"),
            cancelled: false,
            inspected_file: None,
            passed_check: None,
        }),
        "run_check" => {
            let program = args["program"].as_str().unwrap_or("");
            let cmd_args: Vec<String> = args["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let validated = match guard.validate_command(program, &cmd_args) {
                Ok(v) => v,
                Err(e) => return Ok(denied_tool("run_check", format!("{e:#}"))),
            };

            // Отдельная политика исполнения кода (`exec_policy`), независимая от файловой политики!
            match (exec_policy, validated.class) {
                (ExecPolicy::Disabled, _) => {
                    return Ok(denied_tool(
                        "run_check",
                        "Политика исполнения команд (exec_policy = disabled) запрещает запуск любых команд".into(),
                    ));
                }
                (ExecPolicy::SafeInspectOnly, CommandClass::UserCodeExecution) => {
                    return Ok(denied_tool(
                        "run_check",
                        format!(
                            "Команда «{} {}» исполняет код/скрипты проекта с правами пользователя; политика exec_policy = safe_inspect_only запрещает запуск кода",
                            validated.program,
                            validated.raw_args.join(" ")
                        ),
                    ));
                }
                (ExecPolicy::ConfirmUserCode, CommandClass::UserCodeExecution) => {
                    let cmdline = format!("{} {}", validated.program, validated.raw_args.join(" "));
                    let canonical_args = json!({
                        "program": validated.program,
                        "args": validated.args,
                    });
                    let digest = compute_action_digest(task_id, "run_check", &canonical_args);
                    let factors = if validated.risk_factors.is_empty() {
                        String::new()
                    } else {
                        format!(" Факторы риска: {}.", validated.risk_factors.join("; "))
                    };
                    let risk = format!(
                        "ВНИМАНИЕ: реальный sandbox ОС не активен. Команда выполнит код/скрипты проекта с правами текущего пользователя (не ограничено только workspace) в {}: {cmdline}.{factors}",
                        guard.root().display()
                    );
                    match request_approval(
                        approvals,
                        cancel,
                        events,
                        emit,
                        "run_check",
                        digest,
                        IsolationLevel::NoneUserPrivileges,
                        format!("Запуск кода с правами пользователя: {cmdline}"),
                        cmdline,
                        risk,
                    )
                    .await
                    {
                        ApprovalOutcome::Approved => {}
                        ApprovalOutcome::Denied(reason) => {
                            return Ok(denied_tool("run_check", reason));
                        }
                        ApprovalOutcome::Cancelled => {
                            return Ok(cancelled_tool());
                        }
                    }
                }
                (ExecPolicy::SafeInspectOnly, CommandClass::SafeInspect)
                | (ExecPolicy::ConfirmUserCode, CommandClass::SafeInspect)
                | (ExecPolicy::AllowWorkspaceBuilds, _) => {}
            }

            let exec = tools::exec_run_check(guard, program, &cmd_args, cancel);
            if cancel.is_cancelled() {
                return Ok(cancelled_tool());
            }
            let execution = unwrap_tool(exec, "run_check");
            let passed_check = if execution.ok {
                compute_check_coverage(guard, &validated, execution.summary.clone())
            } else {
                None
            };
            Ok(DispatchedTool {
                execution,
                cancelled: false,
                inspected_file: None,
                passed_check,
            })
        }
        "delegate_subtasks" => {
            if !allow_subagents {
                return Ok(denied_tool(
                    "delegate_subtasks",
                    "Дочерней задаче запрещено рекурсивное делегирование подзадач".into(),
                ));
            }
            let specs = match subagents::parse_subtasks(&args) {
                Ok(s) => s,
                Err(e) => return Ok(denied_tool("delegate_subtasks", format!("{e:#}"))),
            };
            if let Err(e) = subagents::validate_subtask_ownership(&specs) {
                return Ok(denied_tool("delegate_subtasks", format!("{e:#}")));
            }
            let (recs, report_json) = subagents::run_subtasks(
                backend,
                guard,
                task_id,
                file_policy,
                exec_policy,
                specs,
                cancel,
                approvals,
                emit,
            )
            .await?;
            let changed: Vec<String> = recs
                .iter()
                .flat_map(|r| r.changed_files.iter().cloned())
                .collect();
            let count = recs.len();
            subtasks_out.extend(recs);
            Ok(DispatchedTool {
                execution: ToolExecution {
                    ok: true,
                    summary: format!(
                        "delegate_subtasks → выполнено {count} подзадач(и) (не подтверждено)"
                    ),
                    output: serde_json::to_string_pretty(&report_json).unwrap_or_default(),
                    changed_files: changed,
                    diff_preview: None,
                },
                cancelled: cancel.is_cancelled(),
                inspected_file: None,
                passed_check: None,
            })
        }
        unknown => Ok(denied_tool(
            unknown,
            format!("Неизвестный инструмент «{unknown}» отклонён policy gateway"),
        )),
    }
}

enum ApprovalOutcome {
    Approved,
    Denied(String),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn request_approval(
    approvals: &ApprovalGate,
    cancel: &Cancel,
    events: &mut Vec<AgentEvent>,
    emit: &mut dyn FnMut(AgentStreamEvent),
    tool: &str,
    action_digest: String,
    isolation: IsolationLevel,
    summary: String,
    canonical_target: String,
    risk_note: String,
) -> ApprovalOutcome {
    let req = PendingApproval {
        request_id: uuid::Uuid::new_v4().to_string(),
        action_digest,
        tool: tool.to_string(),
        isolation,
        summary: summary.clone(),
        canonical_target: canonical_target.clone(),
        risk_note,
    };
    let ev = AgentEvent::ApprovalRequested {
        request_id: req.request_id.clone(),
        tool: req.tool.clone(),
        summary: req.summary.clone(),
        canonical_target: req.canonical_target.clone(),
    };
    events.push(ev.clone());
    emit(AgentStreamEvent::Event(ev));
    emit(AgentStreamEvent::Status(format!(
        "Ожидание подтверждения: {summary}"
    )));
    emit(AgentStreamEvent::ApprovalNeeded(req.clone()));

    match approvals.wait_for(req.clone(), cancel).await {
        None => ApprovalOutcome::Cancelled,
        Some(dec) => {
            let (approved, reason) = approvals.verify_and_consume(&req, &dec);
            let ev = AgentEvent::ApprovalResolved {
                request_id: req.request_id,
                approved,
                reason: reason.clone(),
            };
            events.push(ev.clone());
            emit(AgentStreamEvent::Event(ev));
            if approved {
                ApprovalOutcome::Approved
            } else {
                ApprovalOutcome::Denied(reason)
            }
        }
    }
}

fn check_owned_write(owned_paths: Option<&[String]>, rel_target: &str) -> Result<(), String> {
    let Some(owned) = owned_paths else {
        return Ok(());
    };
    let norm = subagents::normalize_rel(rel_target);
    if owned.iter().any(|o| subagents::is_path_owned(o, &norm)) {
        Ok(())
    } else {
        Err(format!(
            "Запись в «{rel_target}» запрещена: файл не входит в owned_paths подзадачи ({owned:?})"
        ))
    }
}

fn cancelled_tool() -> DispatchedTool {
    DispatchedTool {
        execution: ToolExecution {
            ok: false,
            summary: "Отменено".into(),
            output: "Отменено".into(),
            changed_files: vec![],
            diff_preview: None,
        },
        cancelled: true,
        inspected_file: None,
        passed_check: None,
    }
}

fn denied_tool(name: &str, reason: String) -> DispatchedTool {
    DispatchedTool {
        execution: ToolExecution {
            ok: false,
            summary: format!("{name}: отклонено"),
            output: reason,
            changed_files: vec![],
            diff_preview: None,
        },
        cancelled: false,
        inspected_file: None,
        passed_check: None,
    }
}

fn unwrap_tool(res: Result<ToolExecution>, name: &str) -> ToolExecution {
    match res {
        Ok(ok) => ok,
        Err(e) => ToolExecution {
            ok: false,
            summary: format!("{name}: ошибка"),
            output: format!("{e:#}"),
            changed_files: vec![],
            diff_preview: None,
        },
    }
}

fn build_initial_messages(
    workspace: &Path,
    file_policy: AgentPolicy,
    exec_policy: ExecPolicy,
    allow_subagents: bool,
    owned_paths: Option<&[String]>,
    prior_messages: &[Message],
    user_task: &str,
) -> Vec<Value> {
    let mut sys = format!(
        "Ты — динамичный инженерный агент KAIC Studio.\n\
Рабочий каталог (workspace): {}\n\
Политика файлов: {}; политика исполнения кода: {}\n\
Правила:\n\
1. Действуй пошагово через реальные инструменты (list_files, read_file, search_text, write_file, edit_file, apply_patch, list_skills, read_skill, run_check{}).\n\
2. Не выдумывай содержимое файлов, изменения и результаты тестов: сначала прочитай реальные файлы и проверь изменения через целевой run_check.\n\
3. Если инструмент отклонён политикой или пользователем — честно учти это и не пытайся обойти ограничения.\n\
4. Итоги дочерних задач (delegate_subtasks) считаются неподтверждёнными, пока ты сам не проверишь текущие версии файлов через read_file и целевой run_check (любая последующая правка аннулирует старую проверку).\n\
5. В финальном ответе кратко сообщи фактические действия, изменённые файлы и результаты проверок.",
        workspace.display(),
        file_policy.label_en(),
        exec_policy.label_en(),
        if allow_subagents {
            ", delegate_subtasks"
        } else {
            ""
        }
    );
    if let Some(paths) = owned_paths {
        sys.push_str(&format!(
            "\nРазрешённые пути для записи в этой подзадаче (owned_paths): {paths:?}"
        ));
    }

    let mut out = Vec::new();
    let mut extra_sys = String::new();
    for m in prior_messages {
        if m.role == "system" && !m.content.trim().is_empty() {
            if !extra_sys.is_empty() {
                extra_sys.push_str("\n\n");
            }
            extra_sys.push_str(m.content.trim());
        }
    }
    if !extra_sys.is_empty() {
        sys.push_str("\n\nДополнительные инструкции диалога:\n");
        sys.push_str(&extra_sys);
    }
    out.push(json!({"role": "system", "content": sys}));

    for m in prior_messages {
        if matches!(m.role.as_str(), "user" | "assistant") && !m.content.trim().is_empty() {
            out.push(json!({"role": m.role, "content": m.content}));
        }
    }
    out.push(json!({"role": "user", "content": user_task}));
    out
}

pub fn clip_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…\n[обрезано по лимиту {} байт]", &s[..end], max_bytes)
}

/// Удобная обёртка для headless `--smoke-agent`: использует тот же ManagedServer и 2-осевые политики.
pub async fn smoke_agent_local(
    alias: &str,
    workspace: &Path,
    task: &str,
    file_policy: AgentPolicy,
    exec_policy: ExecPolicy,
    cancel: &Cancel,
) -> Result<AgentRunResult> {
    let (models, _) = crate::catalog::discover(Path::new(crate::catalog::MODEL_ROOT));
    let model = models
        .iter()
        .find(|m| {
            m.loadable()
                && (m.alias() == alias
                    || m.repo == alias
                    || m.path.to_str() == Some(alias)
                    || m.filename == alias)
        })
        .ok_or_else(|| anyhow::anyhow!("Проверенная модель «{alias}» не найдена"))?;
    let mut server = ManagedServer::start(model, 0, cancel).await?;
    let label = model.alias().to_string();
    let mut backend = ManagedServerBackend::new(&mut server, label);
    let gate = ApprovalGate::deny_all();
    let result = run_agent_with_policies(
        &mut backend,
        workspace,
        task,
        &[],
        file_policy,
        exec_policy,
        AgentLimits::default(),
        cancel,
        &gate,
        |_| {},
    )
    .await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_cap_never_escalates_child_privileges() {
        assert_eq!(
            AgentPolicy::ReadOnly.cap(AgentPolicy::WorkspaceWrite),
            AgentPolicy::ReadOnly
        );
        assert_eq!(
            AgentPolicy::ApprovalRequired.cap(AgentPolicy::WorkspaceWrite),
            AgentPolicy::ApprovalRequired
        );
        assert_eq!(
            ExecPolicy::SafeInspectOnly.cap(ExecPolicy::AllowWorkspaceBuilds),
            ExecPolicy::SafeInspectOnly
        );
        assert_eq!(
            ExecPolicy::ConfirmUserCode.cap(ExecPolicy::AllowWorkspaceBuilds),
            ExecPolicy::ConfirmUserCode
        );
    }
}
