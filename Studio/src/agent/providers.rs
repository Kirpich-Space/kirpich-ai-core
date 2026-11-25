//! Провайдеры исполнения: нативный runtime KAIC (llama.cpp / Ollama / OpenAI-compat / Anthropic API)
//! и внешне управляемые авторизованные CLI (Codex CLI / Claude Code CLI / Antigravity CLI),
//! без чтения чужих файлов токенов (`~/.codex/auth.json`, `~/.claude`, keyring).
use super::{
    AgentEvent, AgentPolicy, AgentRunResult, AgentStreamEvent, ApprovalGate, PendingApproval,
    WorkspaceGuard,
};
use crate::network::Cancel;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOrigin {
    /// Агентный цикл, валидация, политики и инструменты выполняются нативно внутри KAIC.
    KaicNativeAgent,
    /// Задача передаётся внешнему управляемому CLI-агенту со своим циклом и политикой.
    ExternallyManagedCli,
}

impl ExecutionOrigin {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::KaicNativeAgent => "KAIC native runtime",
            Self::ExternallyManagedCli => "Внешний CLI",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    LocalLlama,
    OllamaLocal,
    OpenAiCompat,
    AnthropicApi,
    CodexCli,
    ClaudeCli,
    AgyCli,
}

impl ProviderKind {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::LocalLlama => "Локальная модель (llama.cpp)",
            Self::OllamaLocal => "Ollama локально (loopback)",
            Self::OpenAiCompat => "OpenAI-совместимый API",
            Self::AnthropicApi => "Anthropic Messages API",
            Self::CodexCli => "Codex CLI (подписка)",
            Self::ClaudeCli => "Claude Code CLI (подписка)",
            Self::AgyCli => "Antigravity CLI / agy (подписка)",
        }
    }

    pub fn label_en(self) -> &'static str {
        match self {
            Self::LocalLlama => "Local model (llama.cpp)",
            Self::OllamaLocal => "Ollama local (loopback)",
            Self::OpenAiCompat => "OpenAI-compatible API",
            Self::AnthropicApi => "Anthropic Messages API",
            Self::CodexCli => "Codex CLI (subscription)",
            Self::ClaudeCli => "Claude Code CLI (subscription)",
            Self::AgyCli => "Antigravity CLI / agy (subscription)",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::LocalLlama => "local",
            Self::OllamaLocal => "ollama",
            Self::OpenAiCompat => "openai",
            Self::AnthropicApi => "anthropic",
            Self::CodexCli => "codex",
            Self::ClaudeCli => "claude",
            Self::AgyCli => "agy",
        }
    }

    pub fn execution_origin(self) -> ExecutionOrigin {
        match self {
            Self::LocalLlama | Self::OllamaLocal | Self::OpenAiCompat | Self::AnthropicApi => {
                ExecutionOrigin::KaicNativeAgent
            }
            Self::CodexCli | Self::ClaudeCli | Self::AgyCli => {
                ExecutionOrigin::ExternallyManagedCli
            }
        }
    }

    pub fn is_external(self) -> bool {
        !matches!(self, Self::LocalLlama | Self::OllamaLocal)
    }

    pub fn parse_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "llama" | "llama.cpp" | "local_llama" => Ok(Self::LocalLlama),
            "ollama" | "ollama_local" => Ok(Self::OllamaLocal),
            "openai" | "openai-compat" | "openai_compat" | "lmstudio" => Ok(Self::OpenAiCompat),
            "anthropic" | "anthropic-api" | "anthropic_api" => Ok(Self::AnthropicApi),
            "codex" | "codex_cli" => Ok(Self::CodexCli),
            "claude" | "claude_cli" => Ok(Self::ClaudeCli),
            "agy" | "antigravity" | "agy_cli" => Ok(Self::AgyCli),
            other => bail!(
                "Неизвестный провайдер: {other} (поддерживаются: local, ollama, openai, anthropic, codex, claude, agy)"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalGranularity {
    PerAction,
    LaunchConsentAndSandbox,
    Unavailable,
}

impl ApprovalGranularity {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::PerAction => "Подтверждение каждого действия",
            Self::LaunchConsentAndSandbox => "Подтверждение запуска + sandbox CLI",
            Self::Unavailable => "Недоступно",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    Implemented,
    Tested,
    Experimental,
    Unavailable,
}

impl FeatureStatus {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::Implemented => "implemented",
            Self::Tested => "tested",
            Self::Experimental => "experimental",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider: ProviderKind,
    pub execution_origin: ExecutionOrigin,
    pub label: String,
    pub available: bool,
    pub binary_path: Option<String>,
    pub status: FeatureStatus,
    pub streaming: bool,
    pub tool_calling: bool,
    pub cancellation: bool,
    pub multi_session: bool,
    pub approval_granularity: ApprovalGranularity,
    pub linux_status: FeatureStatus,
    pub freebsd_status: FeatureStatus,
    pub notes: String,
}

pub fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn ollama_loopback_listening() -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], 11434).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(35)).is_ok()
}

/// Фактическая проверка наличия провайдеров на хосте без чтения токенов и приватных файлов.
pub fn probe_providers() -> Vec<ProviderCapabilities> {
    let llama_bin = PathBuf::from(format!("{}/llama-server", crate::llama::BINDIR));
    let llama_found = if llama_bin.is_file() {
        Some(llama_bin)
    } else {
        find_in_path("llama-server")
    };
    let ollama_bin = find_in_path("ollama");
    let ollama_listening = ollama_loopback_listening();
    let openai_key_set = std::env::var_os("OPENAI_API_KEY")
        .or_else(|| std::env::var_os("KAIC_OPENAI_API_KEY"))
        .is_some_and(|v| !v.is_empty());
    let anthropic_key_set = std::env::var_os("ANTHROPIC_API_KEY")
        .or_else(|| std::env::var_os("KAIC_ANTHROPIC_API_KEY"))
        .is_some_and(|v| !v.is_empty());
    let codex_found = find_in_path("codex");
    let claude_found = find_in_path("claude");
    let agy_found = find_in_path("agy");

    vec![
        ProviderCapabilities {
            provider: ProviderKind::LocalLlama,
            execution_origin: ExecutionOrigin::KaicNativeAgent,
            label: ProviderKind::LocalLlama.label_ru().into(),
            available: llama_found.is_some(),
            binary_path: llama_found.as_ref().map(|p| p.display().to_string()),
            status: if llama_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: false,
            approval_granularity: ApprovalGranularity::PerAction,
            linux_status: FeatureStatus::Implemented,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Нативный runtime KAIC через loopback llama-server (SSE + GBNF/fallback парсер). FreeBSD не проверена."
                .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::OllamaLocal,
            execution_origin: ExecutionOrigin::KaicNativeAgent,
            label: ProviderKind::OllamaLocal.label_ru().into(),
            available: ollama_bin.is_some() || ollama_listening,
            binary_path: ollama_bin.as_ref().map(|p| p.display().to_string()),
            status: FeatureStatus::Implemented,
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::PerAction,
            linux_status: FeatureStatus::Implemented,
            freebsd_status: FeatureStatus::Unavailable,
            notes: if ollama_listening {
                "Обнаружен локальный демон Ollama на 127.0.0.1:11434; нативный цикл KAIC работает поверх /api/chat (без отправки в облако)."
            } else {
                "Адаптер /api/chat (loopback 127.0.0.1:11434) реализован; демон Ollama сейчас не запущен."
            }
            .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::OpenAiCompat,
            execution_origin: ExecutionOrigin::KaicNativeAgent,
            label: ProviderKind::OpenAiCompat.label_ru().into(),
            available: true,
            binary_path: None,
            status: FeatureStatus::Implemented,
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::PerAction,
            linux_status: FeatureStatus::Implemented,
            freebsd_status: FeatureStatus::Unavailable,
            notes: if openai_key_set {
                "Нативный цикл KAIC через /v1/chat/completions; API-ключ задан в env. Удалённые URL требуют HTTPS и явного согласия."
            } else {
                "Поддерживает локальные OpenAI-endpoints (LM Studio/vLLM на 127.0.0.1) и удалённые HTTPS API (ключ OPENAI_API_KEY + согласие)."
            }
            .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::AnthropicApi,
            execution_origin: ExecutionOrigin::KaicNativeAgent,
            label: ProviderKind::AnthropicApi.label_ru().into(),
            available: anthropic_key_set,
            binary_path: None,
            status: FeatureStatus::Implemented,
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::PerAction,
            linux_status: FeatureStatus::Implemented,
            freebsd_status: FeatureStatus::Unavailable,
            notes: if anthropic_key_set {
                "Нативный /v1/messages клиент (ANTHROPIC_API_KEY найден в env); требуется явное согласие на внешнюю отправку."
            } else {
                "Клиент /v1/messages реализован; задайте ANTHROPIC_API_KEY (или KAIC_ANTHROPIC_API_KEY) и явное согласие."
            }
            .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::CodexCli,
            execution_origin: ExecutionOrigin::ExternallyManagedCli,
            label: ProviderKind::CodexCli.label_ru().into(),
            available: codex_found.is_some(),
            binary_path: codex_found.as_ref().map(|p| p.display().to_string()),
            status: if codex_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::LaunchConsentAndSandbox,
            linux_status: FeatureStatus::Implemented,
            freebsd_status: FeatureStatus::Unavailable,
            notes: "Внешний `codex exec --json --ephemeral`; KAIC не читает ~/.codex/auth.json, требуется явное согласие."
                .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::ClaudeCli,
            execution_origin: ExecutionOrigin::ExternallyManagedCli,
            label: ProviderKind::ClaudeCli.label_ru().into(),
            available: claude_found.is_some(),
            binary_path: claude_found.as_ref().map(|p| p.display().to_string()),
            status: if claude_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::LaunchConsentAndSandbox,
            linux_status: if claude_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            freebsd_status: FeatureStatus::Unavailable,
            notes: if claude_found.is_some() {
                "Внешний `claude -p --output-format stream-json --verbose`; KAIC не читает токены ~/.claude, требуется согласие и --permission-mode (plan/acceptEdits)."
            } else {
                "CLI `claude` не найден в PATH; установите и авторизуйте локально."
            }
            .into(),
        },
        ProviderCapabilities {
            provider: ProviderKind::AgyCli,
            execution_origin: ExecutionOrigin::ExternallyManagedCli,
            label: ProviderKind::AgyCli.label_ru().into(),
            available: agy_found.is_some(),
            binary_path: agy_found.as_ref().map(|p| p.display().to_string()),
            status: if agy_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            streaming: true,
            tool_calling: true,
            cancellation: true,
            multi_session: true,
            approval_granularity: ApprovalGranularity::LaunchConsentAndSandbox,
            linux_status: if agy_found.is_some() {
                FeatureStatus::Implemented
            } else {
                FeatureStatus::Unavailable
            },
            freebsd_status: FeatureStatus::Unavailable,
            notes: if agy_found.is_some() {
                "Внешний `agy --print --output-format stream-json --sandbox`; KAIC не читает токены Antigravity, требуется согласие."
            } else {
                "CLI `agy` не найден в PATH; установите и авторизуйте локально."
            }
            .into(),
        },
    ]
}

#[derive(Clone, Debug)]
pub struct CodexCliConfig {
    pub workspace: PathBuf,
    pub prompt: String,
    pub policy: AgentPolicy,
    pub external_consent: bool,
    pub model: Option<String>,
}

pub type ExternalCliConfig = CodexCliConfig;

struct ExternalCliSpec<'a> {
    provider: ProviderKind,
    tool_id: &'static str,
    display_name: &'static str,
    sandbox_label: &'a str,
    args: Vec<String>,
    model_label: String,
    prompt: &'a str,
}

pub async fn run_codex_cli(
    config: CodexCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let binary = find_in_path("codex").context(
        "Официальный CLI `codex` не найден в PATH. Установите и авторизуйте Codex CLI локально.",
    )?;
    run_codex_cli_with_binary(&binary, config, cancel, approvals, emit).await
}

pub async fn run_codex_cli_with_binary(
    binary: &Path,
    config: CodexCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let sandbox_mode = match config.policy {
        AgentPolicy::ReadOnly => "read-only",
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => "workspace-write",
    };
    let guard = WorkspaceGuard::new(&config.workspace)?;
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--sandbox".to_string(),
        sandbox_mode.to_string(),
        "-C".to_string(),
        guard.root().to_string_lossy().into_owned(),
        "--skip-git-repo-check".to_string(),
        "--ephemeral".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if let Some(m) = &config.model
        && !m.trim().is_empty()
    {
        args.push("-m".to_string());
        args.push(m.trim().to_string());
    }
    args.push("-".to_string());

    let model_label = config
        .model
        .clone()
        .unwrap_or_else(|| "codex-subscription-default".into());
    run_external_cli_subprocess(
        binary,
        &guard,
        config.policy,
        config.external_consent,
        ExternalCliSpec {
            provider: ProviderKind::CodexCli,
            tool_id: "codex_cli",
            display_name: "Codex CLI",
            sandbox_label: sandbox_mode,
            args,
            model_label,
            prompt: &config.prompt,
        },
        cancel,
        approvals,
        emit,
    )
    .await
}

pub async fn run_claude_cli(
    config: ExternalCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let binary = find_in_path("claude").context(
        "Официальный CLI `claude` не найден в PATH. Установите и авторизуйте Claude Code CLI локально.",
    )?;
    run_claude_cli_with_binary(&binary, config, cancel, approvals, emit).await
}

pub async fn run_claude_cli_with_binary(
    binary: &Path,
    config: ExternalCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let guard = WorkspaceGuard::new(&config.workspace)?;
    let mode = match config.policy {
        AgentPolicy::ReadOnly => "plan",
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => "acceptEdits",
    };
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--no-session-persistence".to_string(),
        "--permission-mode".to_string(),
        mode.to_string(),
    ];
    if config.policy == AgentPolicy::ReadOnly {
        args.push("--restricted".to_string());
    }
    if let Some(m) = &config.model
        && !m.trim().is_empty()
    {
        args.push("--model".to_string());
        args.push(m.trim().to_string());
    }
    let model_label = config
        .model
        .clone()
        .unwrap_or_else(|| "claude-subscription-default".into());
    run_external_cli_subprocess(
        binary,
        &guard,
        config.policy,
        config.external_consent,
        ExternalCliSpec {
            provider: ProviderKind::ClaudeCli,
            tool_id: "claude_cli",
            display_name: "Claude Code CLI",
            sandbox_label: mode,
            args,
            model_label,
            prompt: &config.prompt,
        },
        cancel,
        approvals,
        emit,
    )
    .await
}

pub async fn run_agy_cli(
    config: ExternalCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let binary = find_in_path("agy").context(
        "Официальный CLI `agy` не найден в PATH. Установите и авторизуйте Antigravity CLI локально.",
    )?;
    run_agy_cli_with_binary(&binary, config, cancel, approvals, emit).await
}

pub async fn run_agy_cli_with_binary(
    binary: &Path,
    config: ExternalCliConfig,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    let guard = WorkspaceGuard::new(&config.workspace)?;
    let mode = match config.policy {
        AgentPolicy::ReadOnly => "plan",
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => "accept-edits",
    };
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--sandbox".to_string(),
        "--mode".to_string(),
        mode.to_string(),
    ];
    if let Some(m) = &config.model
        && !m.trim().is_empty()
    {
        args.push("--model".to_string());
        args.push(m.trim().to_string());
    }
    let model_label = config
        .model
        .clone()
        .unwrap_or_else(|| "agy-subscription-default".into());
    run_external_cli_subprocess(
        binary,
        &guard,
        config.policy,
        config.external_consent,
        ExternalCliSpec {
            provider: ProviderKind::AgyCli,
            tool_id: "agy_cli",
            display_name: "Antigravity CLI (agy)",
            sandbox_label: mode,
            args,
            model_label,
            prompt: &config.prompt,
        },
        cancel,
        approvals,
        emit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_external_cli_subprocess(
    binary: &Path,
    guard: &WorkspaceGuard,
    policy: AgentPolicy,
    external_consent: bool,
    spec: ExternalCliSpec<'_>,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    mut emit: impl FnMut(AgentStreamEvent),
) -> Result<AgentRunResult> {
    if !external_consent {
        bail!(
            "Передача контекста внешнему провайдеру ({}) заблокирована: включите явное согласие на внешнюю отправку workspace.",
            spec.display_name
        );
    }
    let mut events = Vec::new();
    if policy == AgentPolicy::ApprovalRequired {
        let canonical_args = serde_json::json!({
            "workspace": guard.root().display().to_string(),
            "mode": spec.sandbox_label,
            "model": spec.model_label,
            "prompt": spec.prompt,
        });
        let action_digest =
            super::compute_action_digest("external_cli_launch", spec.tool_id, &canonical_args);
        let req = PendingApproval {
            request_id: uuid::Uuid::new_v4().to_string(),
            action_digest,
            tool: spec.tool_id.into(),
            isolation: super::IsolationLevel::ExternalCliSandbox,
            summary: format!(
                "Запуск {} (режим {}) в {}",
                spec.display_name,
                spec.sandbox_label,
                guard.root().display()
            ),
            canonical_target: guard.root().display().to_string(),
            risk_note: format!(
                "{} управляет собственными инструментами внутри CLI (режим {}). Подтверждение действует на запуск и отправку контекста внешнему провайдеру.",
                spec.display_name, spec.sandbox_label
            ),
        };
        let ev = AgentEvent::ApprovalRequested {
            request_id: req.request_id.clone(),
            tool: req.tool.clone(),
            summary: req.summary.clone(),
            canonical_target: req.canonical_target.clone(),
        };
        events.push(ev.clone());
        emit(AgentStreamEvent::Event(ev));
        emit(AgentStreamEvent::ApprovalNeeded(req.clone()));

        match approvals.wait_for(req.clone(), cancel).await {
            None => {
                let ev = AgentEvent::Cancelled {
                    summary: format!(
                        "Запуск {} отменён во время ожидания подтверждения",
                        spec.display_name
                    ),
                    changed_files: vec![],
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
                return Ok(AgentRunResult {
                    final_text: format!("Операция {} отменена до запуска.", spec.display_name),
                    events,
                    changed_files: vec![],
                    subtasks: vec![],
                    unverified_subtasks: 0,
                    blocked: false,
                    cancelled: true,
                });
            }
            Some(dec) => {
                let (ok, note) = approvals.verify_and_consume(&req, &dec);
                let ev = AgentEvent::ApprovalResolved {
                    request_id: req.request_id,
                    approved: ok,
                    reason: note.clone(),
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
                if !ok {
                    return Ok(AgentRunResult {
                        final_text: note,
                        events,
                        changed_files: vec![],
                        subtasks: vec![],
                        unverified_subtasks: 0,
                        blocked: true,
                        cancelled: false,
                    });
                }
            }
        }
    }

    let step_ev = AgentEvent::Step {
        step: 1,
        max_steps: 1,
        provider: spec.provider,
        model: spec.model_label.clone(),
        summary: format!(
            "Запуск внешнего {} (режим {})",
            spec.display_name, spec.sandbox_label
        ),
    };
    events.push(step_ev.clone());
    emit(AgentStreamEvent::Event(step_ev));
    emit(AgentStreamEvent::Status(format!(
        "{} ({}) выполняет задачу…",
        spec.display_name, spec.sandbox_label
    )));

    let mut cmd = Command::new(binary);
    cmd.args(&spec.args)
        .current_dir(guard.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = crate::platform::set_parent_death_signal();
            Ok(())
        });
    }

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Не удалось запустить {} ({})",
            spec.display_name,
            binary.display()
        )
    })?;
    let pgid = child.id() as libc::pid_t;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec.prompt.as_bytes());
    }

    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("Нет stdout {}", spec.display_name))?;
    let stderr = child
        .stderr
        .take()
        .with_context(|| format!("Нет stderr {}", spec.display_name))?;
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let mut out_reader = Some(std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    }));
    let mut err_reader = Some(std::thread::spawn(move || {
        let mut buf = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if buf.len() < 40 {
                buf.push(line);
            }
        }
        buf
    }));

    let mut final_text = String::new();
    let mut changed_files = Vec::new();
    let mut cancelled = false;
    let start = Instant::now();
    let timeout = Duration::from_secs(300);

    loop {
        while let Ok(line) = line_rx.try_recv() {
            process_external_jsonl_line(
                spec.provider,
                &line,
                &mut final_text,
                &mut changed_files,
                &mut events,
                &mut emit,
            );
        }
        if cancel.is_cancelled() {
            super::tools::kill_process_group(pgid, &mut child);
            cancelled = true;
            break;
        }
        if let Some(status) = child.try_wait()? {
            if let Some(h) = out_reader.take() {
                let _ = h.join();
            }
            while let Ok(line) = line_rx.try_recv() {
                process_external_jsonl_line(
                    spec.provider,
                    &line,
                    &mut final_text,
                    &mut changed_files,
                    &mut events,
                    &mut emit,
                );
            }
            let err_lines = err_reader
                .take()
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            if !status.success() {
                let detail = err_lines.join("\n");
                bail!(
                    "{} завершился с ошибкой ({status}){}",
                    spec.display_name,
                    if detail.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                );
            }
            break;
        }
        if start.elapsed() > timeout {
            super::tools::kill_process_group(pgid, &mut child);
            bail!(
                "{} превысил таймаут 300 с и был остановлен",
                spec.display_name
            );
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    if cancelled {
        drop(out_reader.take());
        drop(err_reader.take());
        let ev = AgentEvent::Cancelled {
            summary: format!(
                "{} остановлен пользователем; группа дочерних процессов завершена",
                spec.display_name
            ),
            changed_files: changed_files.clone(),
        };
        events.push(ev.clone());
        emit(AgentStreamEvent::Event(ev));
        return Ok(AgentRunResult {
            final_text: if final_text.is_empty() {
                format!(
                    "Выполнение {} остановлено пользователем.",
                    spec.display_name
                )
            } else {
                final_text
            },
            events,
            changed_files,
            subtasks: vec![],
            unverified_subtasks: 0,
            blocked: false,
            cancelled: true,
        });
    }

    let done_ev = AgentEvent::Completed {
        summary: format!("{} завершил задачу", spec.display_name),
        changed_files: changed_files.clone(),
    };
    events.push(done_ev.clone());
    emit(AgentStreamEvent::Event(done_ev));

    Ok(AgentRunResult {
        final_text: if final_text.trim().is_empty() {
            format!("{} завершился без текстового сообщения.", spec.display_name)
        } else {
            final_text
        },
        events,
        changed_files,
        subtasks: vec![],
        unverified_subtasks: 0,
        blocked: false,
        cancelled: false,
    })
}

fn process_external_jsonl_line(
    provider: ProviderKind,
    line: &str,
    final_text: &mut String,
    changed_files: &mut Vec<String>,
    events: &mut Vec<AgentEvent>,
    emit: &mut impl FnMut(AgentStreamEvent),
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };
    if provider == ProviderKind::CodexCli {
        process_codex_value(&v, final_text, changed_files, events, emit);
        return;
    }
    // Claude / Agy stream-json (assistant content blocks, result, error)
    let kind = v["type"].as_str().unwrap_or("");
    match kind {
        "assistant" => {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            if let Some(txt) = b["text"].as_str()
                                && !txt.is_empty()
                            {
                                if !final_text.is_empty() {
                                    final_text.push('\n');
                                    emit(AgentStreamEvent::TextDelta("\n".into()));
                                }
                                final_text.push_str(txt);
                                emit(AgentStreamEvent::TextDelta(txt.to_string()));
                            }
                        }
                        "tool_use" => {
                            let name = b["name"].as_str().unwrap_or("tool");
                            let args = serde_json::to_string(&b["input"]).unwrap_or_default();
                            let ev = AgentEvent::ToolCall {
                                id: b["id"].as_str().unwrap_or("ext-call").into(),
                                name: format!("{}.{name}", provider.short_label()),
                                args_summary: args.chars().take(200).collect(),
                                policy_note: provider.execution_origin().label_ru().into(),
                            };
                            events.push(ev.clone());
                            emit(AgentStreamEvent::Event(ev));
                        }
                        _ => {}
                    }
                }
            } else if let Some(txt) = v["text"].as_str()
                && !txt.is_empty()
            {
                final_text.push_str(txt);
                emit(AgentStreamEvent::TextDelta(txt.to_string()));
            }
        }
        "content_block_delta" => {
            if let Some(txt) = v["delta"]["text"].as_str()
                && !txt.is_empty()
            {
                final_text.push_str(txt);
                emit(AgentStreamEvent::TextDelta(txt.to_string()));
            }
        }
        "user" => {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    if b["type"].as_str() == Some("tool_result") {
                        let out = b["content"].as_str().unwrap_or("");
                        let ev = AgentEvent::ToolResult {
                            id: b["tool_use_id"].as_str().unwrap_or("ext-res").into(),
                            name: format!("{}.tool", provider.short_label()),
                            ok: !b["is_error"].as_bool().unwrap_or(false),
                            summary: "Внешний инструмент CLI".into(),
                            output_preview: out.chars().take(400).collect(),
                            changed_files: vec![],
                            diff_preview: None,
                        };
                        events.push(ev.clone());
                        emit(AgentStreamEvent::Event(ev));
                    }
                }
            }
        }
        "result" => {
            if let Some(txt) = v["result"].as_str()
                && final_text.trim().is_empty()
                && !txt.trim().is_empty()
            {
                final_text.push_str(txt);
                emit(AgentStreamEvent::TextDelta(txt.to_string()));
            }
        }
        "error" => {
            if let Some(msg) = v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
            {
                let ev = AgentEvent::Blocked {
                    reason: format!("{}: {msg}", provider.label_en()),
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
            }
        }
        _ => {}
    }
}

fn process_codex_value(
    v: &Value,
    final_text: &mut String,
    changed_files: &mut Vec<String>,
    events: &mut Vec<AgentEvent>,
    emit: &mut impl FnMut(AgentStreamEvent),
) {
    let kind = v["type"].as_str().unwrap_or("");
    match kind {
        "item.completed" | "item.updated" => {
            let item = &v["item"];
            let item_type = item["type"].as_str().unwrap_or("");
            match item_type {
                "agent_message" => {
                    if let Some(txt) = item["text"].as_str()
                        && !txt.is_empty()
                    {
                        if !final_text.is_empty() {
                            final_text.push('\n');
                            emit(AgentStreamEvent::TextDelta("\n".into()));
                        }
                        final_text.push_str(txt);
                        emit(AgentStreamEvent::TextDelta(txt.to_string()));
                    }
                }
                "command_execution" => {
                    let cmd = item["command"].as_str().unwrap_or("command");
                    let code = item["exit_code"].as_i64();
                    let out = item["aggregated_output"].as_str().unwrap_or("");
                    let ev = AgentEvent::ToolResult {
                        id: item["id"].as_str().unwrap_or("codex-cmd").into(),
                        name: "codex.command".into(),
                        ok: code.unwrap_or(0) == 0,
                        summary: format!("{cmd} (exit {:?})", code.unwrap_or(0)),
                        output_preview: out.chars().take(400).collect(),
                        changed_files: vec![],
                        diff_preview: None,
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                }
                "file_change" => {
                    let mut paths = Vec::new();
                    if let Some(changes) = item["changes"].as_array() {
                        for c in changes {
                            if let Some(p) = c["path"].as_str() {
                                paths.push(p.to_string());
                                if !changed_files.iter().any(|x| x == p) {
                                    changed_files.push(p.to_string());
                                }
                            }
                        }
                    }
                    let ev = AgentEvent::ToolResult {
                        id: item["id"].as_str().unwrap_or("codex-file").into(),
                        name: "codex.file_change".into(),
                        ok: true,
                        summary: format!("Изменено файлов: {}", paths.len()),
                        output_preview: paths.join(", "),
                        changed_files: paths,
                        diff_preview: None,
                    };
                    events.push(ev.clone());
                    emit(AgentStreamEvent::Event(ev));
                }
                _ => {}
            }
        }
        "error" => {
            if let Some(msg) = v["message"].as_str() {
                let ev = AgentEvent::Blocked {
                    reason: format!("Codex CLI: {msg}"),
                };
                events.push(ev.clone());
                emit(AgentStreamEvent::Event(ev));
            }
        }
        _ => {}
    }
}
