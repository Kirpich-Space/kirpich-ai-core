use kaic_studio::{
    agent::{
        AgentLimits, AgentPolicy, AgentRunResult, AgentStreamEvent, AnthropicApiBackend,
        ApprovalGate, CodexCliConfig, ExecPolicy, ManagedServerBackend, OllamaBackend,
        OpenAiCompatBackend, ProviderKind, WorkspaceGuard, probe_providers,
        run_agent_with_policies, run_agy_cli, run_claude_cli, run_codex_cli,
    },
    kaic::Secret,
    llama::ManagedServer,
    network::Cancel,
    service, storage,
    tui::{TuiConfig, term::ColorMode},
};
use serde_json::json;
use std::{path::PathBuf, time::Instant};

fn main() {
    if let Err(e) = run() {
        eprintln!("KAIC Studio: {e:#}");
        std::process::exit(1);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    InstallLocal,
    Providers {
        json: bool,
    },
    Models {
        json: bool,
    },
    SmokeChat {
        alias: String,
        report: Option<PathBuf>,
    },
    AgentRun(AgentCliArgs),
    Tui(TuiCliArgs),
    Gui {
        data_dir: Option<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct TuiCliArgs {
    workspace: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    provider: ProviderKind,
    model: String,
    endpoint: String,
    policy: AgentPolicy,
    exec_policy: ExecPolicy,
    allow_external: bool,
    dynamic_interaction: bool,
    resume_session: Option<String>,
    no_color: bool,
}

impl Default for TuiCliArgs {
    fn default() -> Self {
        Self {
            workspace: None,
            data_dir: None,
            provider: ProviderKind::LocalLlama,
            model: "gemma26".into(),
            endpoint: String::new(),
            policy: AgentPolicy::WorkspaceWrite,
            exec_policy: ExecPolicy::ConfirmUserCode,
            allow_external: false,
            dynamic_interaction: false,
            resume_session: None,
            no_color: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AgentCliArgs {
    workspace: PathBuf,
    model: String,
    policy: AgentPolicy,
    exec_policy: ExecPolicy,
    provider: ProviderKind,
    endpoint: String,
    allow_external: bool,
    dynamic_interaction: bool,
    task: String,
    ndjson: bool,
    report: Option<PathBuf>,
}

fn parse_cli(args: &[String]) -> anyhow::Result<CliAction> {
    let has_display = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    parse_cli_with_display(args, has_display)
}

fn parse_cli_with_display(args: &[String], has_display: bool) -> anyhow::Result<CliAction> {
    if args.is_empty() {
        if has_display {
            return Ok(CliAction::Gui { data_dir: None });
        }
        return Ok(CliAction::Tui(TuiCliArgs::default()));
    }
    if args[0] == "--help" || args[0] == "-h" || args[0] == "help" {
        return Ok(CliAction::Help);
    }
    if args.len() == 1 && args[0] == "--install-local" {
        return Ok(CliAction::InstallLocal);
    }
    if args.len() == 1 && args[0] == "--probe-providers" {
        return Ok(CliAction::Providers { json: true });
    }
    if args[0] == "providers" {
        let json = match args.get(1).map(String::as_str) {
            None => false,
            Some("--json") if args.len() == 2 => true,
            _ => anyhow::bail!("Использование: kaic-studio providers [--json]"),
        };
        return Ok(CliAction::Providers { json });
    }
    if args.len() == 1 && args[0] == "--list-models" {
        return Ok(CliAction::Models { json: false });
    }
    if args[0] == "models" {
        let json = match args.get(1).map(String::as_str) {
            None => false,
            Some("--json") if args.len() == 2 => true,
            _ => anyhow::bail!("Использование: kaic-studio models [--json]"),
        };
        return Ok(CliAction::Models { json });
    }
    if args[0] == "--smoke-chat" {
        if args.len() != 2 && !(args.len() == 4 && args[2] == "--report") {
            anyhow::bail!("Использование: --smoke-chat <модель> [--report файл.json]");
        }
        return Ok(CliAction::SmokeChat {
            alias: args[1].clone(),
            report: args.get(3).map(PathBuf::from),
        });
    }
    if args[0] == "--smoke-agent"
        || (args[0] == "agent" && args.get(1).map(String::as_str) == Some("run"))
    {
        let start_idx = if args[0] == "agent" { 2 } else { 1 };
        return Ok(CliAction::AgentRun(parse_agent_args(&args[start_idx..])?));
    }
    if args[0] == "tui" {
        return Ok(CliAction::Tui(parse_tui_args(&args[1..])?));
    }
    if args[0] == "gui" {
        if args.len() == 1 {
            return Ok(CliAction::Gui { data_dir: None });
        }
        if args.len() == 3 && args[1] == "--data-dir" {
            return Ok(CliAction::Gui {
                data_dir: Some(PathBuf::from(&args[2])),
            });
        }
        anyhow::bail!("Использование: kaic-studio gui [--data-dir <каталог>]");
    }
    if args.len() == 2 && args[0] == "--data-dir" {
        return Ok(CliAction::Gui {
            data_dir: Some(PathBuf::from(&args[1])),
        });
    }
    anyhow::bail!("Неизвестные параметры. Используйте --help");
}

fn parse_tui_args(args: &[String]) -> anyhow::Result<TuiCliArgs> {
    let mut cfg = TuiCliArgs::default();
    let mut exec_explicit: Option<ExecPolicy> = None;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "-C" => {
                i += 1;
                cfg.workspace =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("Укажите путь после --workspace")
                    })?));
            }
            "--data-dir" => {
                i += 1;
                cfg.data_dir =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("Укажите каталог после --data-dir")
                    })?));
            }
            "--provider" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите провайдер после --provider"))?;
                cfg.provider = ProviderKind::parse_str(raw)?;
            }
            "--model" | "-m" => {
                i += 1;
                cfg.model = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите модель после --model"))?
                    .clone();
            }
            "--endpoint" => {
                i += 1;
                cfg.endpoint = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите URL после --endpoint"))?
                    .clone();
            }
            "--policy" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите политику после --policy"))?;
                cfg.policy = AgentPolicy::parse_str(raw)?;
            }
            "--exec-policy" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите политику после --exec-policy"))?;
                exec_explicit = Some(ExecPolicy::parse_str(raw)?);
            }
            "--allow-external" => cfg.allow_external = true,
            "--dynamic-interaction" => cfg.dynamic_interaction = true,
            "--resume" => {
                i += 1;
                cfg.resume_session = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("Укажите префикс id после --resume"))?
                        .clone(),
                );
            }
            "--no-color" => cfg.no_color = true,
            other => anyhow::bail!("Неизвестный параметр TUI: {other}"),
        }
        i += 1;
    }
    cfg.exec_policy = exec_explicit.unwrap_or(match cfg.policy {
        AgentPolicy::ReadOnly => ExecPolicy::SafeInspectOnly,
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => ExecPolicy::ConfirmUserCode,
    });
    Ok(cfg)
}

fn parse_agent_args(args: &[String]) -> anyhow::Result<AgentCliArgs> {
    let mut workspace: Option<PathBuf> = None;
    let mut model = String::from("gemma26");
    let mut policy = AgentPolicy::WorkspaceWrite;
    let mut exec_policy: Option<ExecPolicy> = None;
    let mut provider = ProviderKind::LocalLlama;
    let mut endpoint = String::new();
    let mut allow_external = false;
    let mut dynamic_interaction = false;
    let mut ndjson = false;
    let mut task = String::from(
        "Прочитай файлы в workspace, исправь ошибку если она есть и запусти проверку через run_check. Кратко сообщи итог.",
    );
    let mut report: Option<PathBuf> = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "-C" => {
                i += 1;
                workspace =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("Укажите путь после --workspace")
                    })?));
            }
            "--model" | "-m" => {
                i += 1;
                model = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите модель после --model"))?
                    .clone();
            }
            "--policy" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите политику после --policy"))?;
                policy = AgentPolicy::parse_str(raw)?;
            }
            "--exec-policy" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите политику после --exec-policy"))?;
                exec_policy = Some(ExecPolicy::parse_str(raw)?);
            }
            "--provider" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите провайдер после --provider"))?;
                provider = ProviderKind::parse_str(raw)?;
            }
            "--endpoint" => {
                i += 1;
                endpoint = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите URL после --endpoint"))?
                    .clone();
            }
            "--allow-external" => allow_external = true,
            "--dynamic-interaction" => dynamic_interaction = true,
            "--json" => ndjson = true,
            "--task" | "-t" => {
                i += 1;
                task = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Укажите задачу после --task"))?
                    .clone();
            }
            "--report" => {
                i += 1;
                report =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("Укажите файл после --report")
                    })?));
            }
            other => anyhow::bail!("Неизвестный параметр агента: {other}"),
        }
        i += 1;
    }

    let ws = workspace
        .ok_or_else(|| anyhow::anyhow!("Обязателен параметр --workspace <абсолютный_путь>"))?;
    let resolved_exec = exec_policy.unwrap_or(match policy {
        AgentPolicy::ReadOnly => ExecPolicy::SafeInspectOnly,
        AgentPolicy::ApprovalRequired | AgentPolicy::WorkspaceWrite => ExecPolicy::ConfirmUserCode,
    });
    Ok(AgentCliArgs {
        workspace: ws,
        model,
        policy,
        exec_policy: resolved_exec,
        provider,
        endpoint,
        allow_external,
        dynamic_interaction,
        task,
        ndjson,
        report,
    })
}

fn print_help() {
    println!(
        "KAIC Studio — нативный Rust-агент и студия (TUI + CLI + GUI, без Electron)\n\n\
Команды:\n\
  kaic-studio tui [--workspace <каталог>] [--provider <...>] [--model <...>] [опции]\n\
      Интерактивный нативный терминальный интерфейс (TUI: TrueColor / SSH / TTY / --no-color).\n\
      Опции: --data-dir, --endpoint, --policy, --exec-policy, --allow-external, --dynamic-interaction, --resume <id>.\n\
  kaic-studio [gui] [--data-dir <каталог>]\n\
      Открыть нативное окно GUI (eframe/egui). При отсутствии DISPLAY/WAYLAND_DISPLAY открывается TUI.\n\
  kaic-studio agent run --workspace <каталог> --task <текст> [опции]\n\
      Динамичный агентный цикл через единый runtime и 2-осевой policy/tool gateway.\n\
      Опции:\n\
        --provider <local|ollama|openai|anthropic|codex|claude|agy>\n\
                                               Провайдер (по умолчанию local)\n\
        --model <алиас|gguf|имя>               Модель (по умолчанию gemma26)\n\
        --endpoint <url>                       Endpoint для ollama/openai/anthropic\n\
        --policy <read-only|approval-required|workspace-write>\n\
                                               Файловая политика (kernel openat O_NOFOLLOW)\n\
        --exec-policy <disabled|safe-inspect|confirm-user-code|allow-builds>\n\
                                               Политика исполнения кода (без OS-sandbox)\n\
        --allow-external                       Явное согласие на внешнюю передачу данных\n\
        --dynamic-interaction                  Включить динамическое взаимодействие (по умолчанию выкл.)\n\
        --json                                 Потоковый NDJSON (для скриптов и мостов мессенджеров)\n\
        --report <файл.json>                   Сохранить структурированный отчёт\n\
  kaic-studio providers [--json]\n\
      Фактические возможности и доступность провайдеров (Linux / FreeBSD).\n\
  kaic-studio models [--json]\n\
      Локальный каталог GGUF и сохранённые измерения скорости.\n\
  kaic-studio --smoke-chat <gemma26|glm30|путь.gguf> [--report <файл.json>]\n\
      Реальный локальный чат через тот же ManagedServer, что и GUI.\n\
  kaic-studio --install-local\n\
      Установить эту сборку в ~/.local/bin и добавить ярлык меню.\n\n\
Совместимость: поддерживаются --list-models, --probe-providers и --smoke-agent."
    );
}

fn emit_ndjson(ev: &AgentStreamEvent) {
    let payload = match ev {
        AgentStreamEvent::Status(s) => json!({"type": "status", "text": s}),
        AgentStreamEvent::TextDelta(s) => json!({"type": "delta", "text": s}),
        AgentStreamEvent::Event(e) => json!({"type": "event", "event": e}),
        AgentStreamEvent::Subtask(st) => json!({"type": "subtask", "subtask": st}),
        AgentStreamEvent::SeatUpdate(seat) => json!({"type": "seat", "seat": seat}),
        AgentStreamEvent::ContextUsage(m) => json!({"type": "context", "metrics": m}),
        AgentStreamEvent::ApprovalNeeded(req) => json!({"type": "approval_needed", "request": req}),
        AgentStreamEvent::Metrics {
            tokens,
            tps,
            seconds,
        } => json!({"type": "metrics", "tokens": tokens, "tps": tps, "seconds": seconds}),
    };
    println!("{payload}");
}

fn env_secret_cli(names: &[&str]) -> Option<Secret> {
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

fn run_agent_cli(cfg: AgentCliArgs) -> anyhow::Result<()> {
    WorkspaceGuard::new(&cfg.workspace)?;
    let start = Instant::now();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let cancel = Cancel::default();
    let ndjson = cfg.ndjson;
    let limits = AgentLimits {
        dynamic_interaction_enabled: cfg.dynamic_interaction,
        ..AgentLimits::default()
    };
    let mut emit = |ev: AgentStreamEvent| {
        if ndjson {
            emit_ndjson(&ev);
        }
    };
    let result: anyhow::Result<AgentRunResult> = match cfg.provider {
        ProviderKind::LocalLlama => rt.block_on(async {
            let (models, _) = kaic_studio::catalog::discover(std::path::Path::new(
                kaic_studio::catalog::MODEL_ROOT,
            ));
            let model = models
                .iter()
                .find(|m| {
                    m.loadable()
                        && (m.alias() == cfg.model
                            || m.repo == cfg.model
                            || m.path.to_str() == Some(cfg.model.as_str())
                            || m.filename == cfg.model)
                })
                .ok_or_else(|| anyhow::anyhow!("Проверенная модель «{}» не найдена", cfg.model))?;
            let mut server = ManagedServer::start(model, 0, &cancel).await?;
            let label = model.alias().to_string();
            let mut backend = ManagedServerBackend::new(&mut server, label);
            let gate = ApprovalGate::deny_all();
            run_agent_with_policies(
                &mut backend,
                &cfg.workspace,
                &cfg.task,
                &[],
                cfg.policy,
                cfg.exec_policy,
                limits,
                &cancel,
                &gate,
                &mut emit,
            )
            .await
        }),
        ProviderKind::OllamaLocal => rt.block_on(async {
            let ep = if cfg.endpoint.trim().is_empty() {
                "http://127.0.0.1:11434"
            } else {
                cfg.endpoint.trim()
            };
            let mut backend = OllamaBackend::new(ep, cfg.model.clone(), 8192)?;
            let gate = ApprovalGate::deny_all();
            run_agent_with_policies(
                &mut backend,
                &cfg.workspace,
                &cfg.task,
                &[],
                cfg.policy,
                cfg.exec_policy,
                limits,
                &cancel,
                &gate,
                &mut emit,
            )
            .await
        }),
        ProviderKind::OpenAiCompat => rt.block_on(async {
            if cfg.endpoint.trim().is_empty() {
                anyhow::bail!("Для openai-совместимого провайдера обязателен --endpoint <url>");
            }
            let api_key = env_secret_cli(&["KAIC_OPENAI_API_KEY", "OPENAI_API_KEY"]);
            let mut backend = OpenAiCompatBackend::new(
                cfg.endpoint.trim(),
                cfg.model.clone(),
                api_key,
                cfg.allow_external,
            )?;
            let gate = ApprovalGate::deny_all();
            run_agent_with_policies(
                &mut backend,
                &cfg.workspace,
                &cfg.task,
                &[],
                cfg.policy,
                cfg.exec_policy,
                limits,
                &cancel,
                &gate,
                &mut emit,
            )
            .await
        }),
        ProviderKind::AnthropicApi => rt.block_on(async {
            let ep = if cfg.endpoint.trim().is_empty() {
                "https://api.anthropic.com"
            } else {
                cfg.endpoint.trim()
            };
            let api_key = env_secret_cli(&["KAIC_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"])
                .unwrap_or_default();
            let model_id = if cfg.model == "gemma26" {
                "claude-sonnet-4-5".to_string()
            } else {
                cfg.model.clone()
            };
            let mut backend = AnthropicApiBackend::new(ep, model_id, api_key, cfg.allow_external)?;
            let gate = ApprovalGate::deny_all();
            run_agent_with_policies(
                &mut backend,
                &cfg.workspace,
                &cfg.task,
                &[],
                cfg.policy,
                cfg.exec_policy,
                limits,
                &cancel,
                &gate,
                &mut emit,
            )
            .await
        }),
        ProviderKind::CodexCli | ProviderKind::ClaudeCli | ProviderKind::AgyCli => {
            let cli_cfg = CodexCliConfig {
                workspace: cfg.workspace.clone(),
                prompt: cfg.task.clone(),
                policy: cfg.policy,
                external_consent: cfg.allow_external,
                model: (cfg.model != "gemma26").then(|| cfg.model.clone()),
            };
            let gate = ApprovalGate::deny_all();
            match cfg.provider {
                ProviderKind::CodexCli => {
                    rt.block_on(run_codex_cli(cli_cfg, &cancel, &gate, &mut emit))
                }
                ProviderKind::ClaudeCli => {
                    rt.block_on(run_claude_cli(cli_cfg, &cancel, &gate, &mut emit))
                }
                ProviderKind::AgyCli => {
                    rt.block_on(run_agy_cli(cli_cfg, &cancel, &gate, &mut emit))
                }
                _ => unreachable!(),
            }
        }
    };

    let report_model = if cfg.provider.is_external() && cfg.model == "gemma26" {
        cfg.provider.short_label()
    } else {
        cfg.model.as_str()
    };
    if let Some(rp) = cfg.report {
        let report = match &result {
            Ok(r) => json!({
                "kind": cfg.provider.label_ru(),
                "origin": cfg.provider.execution_origin(),
                "provider": cfg.provider,
                "model": report_model,
                "workspace": cfg.workspace,
                "policy": cfg.policy,
                "exec_policy": cfg.exec_policy,
                "success": !r.blocked && !r.cancelled,
                "blocked": r.blocked,
                "cancelled": r.cancelled,
                "changed_files": r.changed_files,
                "subtasks": r.subtasks,
                "unverified_subtasks": r.unverified_subtasks,
                "events_count": r.events.len(),
                "events": r.events,
                "final_text": r.final_text,
                "seconds": start.elapsed().as_secs_f64(),
            }),
            Err(e) => json!({
                "kind": "ошибка агентного запуска",
                "origin": cfg.provider.execution_origin(),
                "provider": cfg.provider,
                "model": report_model,
                "workspace": cfg.workspace,
                "policy": cfg.policy,
                "exec_policy": cfg.exec_policy,
                "success": false,
                "error": format!("{e:#}"),
                "seconds": start.elapsed().as_secs_f64(),
            }),
        };
        storage::save_json(&rp, &report)?;
    }

    let run = result?;
    if cfg.ndjson {
        println!(
            "{}",
            json!({
                "type": "final",
                "provider": cfg.provider,
                "origin": cfg.provider.execution_origin(),
                "model": report_model,
                "blocked": run.blocked,
                "cancelled": run.cancelled,
                "changed_files": run.changed_files,
                "unverified_subtasks": run.unverified_subtasks,
                "final_text": run.final_text,
                "seconds": start.elapsed().as_secs_f64(),
            })
        );
    } else {
        println!("{}", run.final_text);
    }
    Ok(())
}

fn run() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match parse_cli(&args)? {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::InstallLocal => kaic_studio::install::install_local(),
        CliAction::Providers { json: as_json } => {
            let caps = probe_providers();
            if as_json {
                println!("{}", serde_json::to_string_pretty(&caps)?);
            } else {
                for c in caps {
                    println!(
                        "{} [{} · {}]\n  binary: {}\n  approval: {}\n  OS: linux={} · freebsd={}\n  {}",
                        c.label,
                        c.execution_origin.label_ru(),
                        c.status.label_ru(),
                        c.binary_path.as_deref().unwrap_or("не найден"),
                        c.approval_granularity.label_ru(),
                        c.linux_status.label_ru(),
                        c.freebsd_status.label_ru(),
                        c.notes
                    );
                }
            }
            Ok(())
        }
        CliAction::Models { json: as_json } => {
            let (models, warnings) = kaic_studio::catalog::discover(std::path::Path::new(
                kaic_studio::catalog::MODEL_ROOT,
            ));
            for warning in warnings {
                eprintln!("{warning}");
            }
            if as_json {
                let items: Vec<_> = models
                    .iter()
                    .map(|m| {
                        json!({
                            "alias": m.alias(),
                            "repo": m.repo,
                            "filename": m.filename,
                            "path": m.path,
                            "size": m.size,
                            "status": m.status,
                            "loadable": m.loadable(),
                            "sha256": m.sha256,
                            "generation_tps": m.generation_tps,
                            "meets_10_tps": m.meets_10_tps,
                            "chat_exit": m.chat_exit,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                for m in models {
                    println!(
                        "{}\n  {}\n  {} байт · {} · {}\n  SHA256: {}\n  {}",
                        m.alias(),
                        m.path.display(),
                        m.size,
                        m.status,
                        if m.loadable() {
                            "доступна"
                        } else {
                            "недоступна"
                        },
                        m.sha256,
                        m.measurement()
                    );
                }
            }
            Ok(())
        }
        CliAction::SmokeChat { alias, report } => {
            let start = Instant::now();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = rt.block_on(service::smoke_chat(&alias));
            if let Some(rp) = report {
                let rep = match &result {
                    Ok(s) => {
                        json!({"kind":"реальный локальный инференс","model":alias,"success":true,"output":s,"seconds":start.elapsed().as_secs_f64()})
                    }
                    Err(e) => {
                        json!({"kind":"реальный локальный инференс","model":alias,"success":false,"error":format!("{e:#}"),"seconds":start.elapsed().as_secs_f64()})
                    }
                };
                storage::save_json(&rp, &rep)?;
            }
            println!("{}", result?);
            Ok(())
        }
        CliAction::AgentRun(cfg) => run_agent_cli(cfg),
        CliAction::Tui(cfg) => {
            let data = resolve_validated_data_dir(cfg.data_dir)?;
            let workspace = match cfg.workspace {
                Some(w) => w,
                None => std::env::current_dir()?,
            };
            kaic_studio::tui::run_tui(TuiConfig {
                data_dir: data,
                workspace,
                provider: cfg.provider,
                model: cfg.model,
                endpoint: cfg.endpoint,
                policy: cfg.policy,
                exec_policy: cfg.exec_policy,
                allow_external: cfg.allow_external,
                dynamic_interaction: cfg.dynamic_interaction,
                resume_session: cfg.resume_session,
                color_override: cfg.no_color.then_some(ColorMode::Plain),
            })
        }
        CliAction::Gui { data_dir } => {
            let data = resolve_validated_data_dir(data_dir)?;
            kaic_studio::ui::run(data)
        }
    }
}

fn resolve_validated_data_dir(data_dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let data = match data_dir {
        Some(p) => p,
        None => storage::data_dir()?,
    };
    if !data.is_absolute() {
        anyhow::bail!(
            "Каталог данных должен быть абсолютным; переход к пользовательским данным не выполняется"
        );
    }
    if data.exists() && !data.is_dir() {
        anyhow::bail!("Явный каталог данных является файлом; резервный каталог не используется");
    }
    Ok(data)
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn cli_parses_subcommands_ndjson_and_legacy_flags() {
        assert_eq!(
            parse_cli_with_display(&s(&[]), true).unwrap(),
            CliAction::Gui { data_dir: None }
        );
        assert!(matches!(
            parse_cli_with_display(&s(&[]), false).unwrap(),
            CliAction::Tui(ref t) if !t.dynamic_interaction
        ));
        match parse_cli(&s(&[
            "tui",
            "--workspace",
            "/tmp/ws",
            "--provider",
            "ollama",
            "--model",
            "qwen2.5-coder",
            "--no-color",
            "--resume",
            "abc123",
        ]))
        .unwrap()
        {
            CliAction::Tui(t) => {
                assert_eq!(t.workspace, Some(PathBuf::from("/tmp/ws")));
                assert_eq!(t.provider, ProviderKind::OllamaLocal);
                assert_eq!(t.model, "qwen2.5-coder");
                assert!(t.no_color);
                assert!(!t.dynamic_interaction);
                assert_eq!(t.resume_session.as_deref(), Some("abc123"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(
            parse_cli(&s(&["gui", "--data-dir", "/tmp/qa"])).unwrap(),
            CliAction::Gui {
                data_dir: Some(PathBuf::from("/tmp/qa"))
            }
        );
        assert_eq!(
            parse_cli(&s(&["providers", "--json"])).unwrap(),
            CliAction::Providers { json: true }
        );
        assert_eq!(
            parse_cli(&s(&["--probe-providers"])).unwrap(),
            CliAction::Providers { json: true }
        );
        assert_eq!(
            parse_cli(&s(&["models", "--json"])).unwrap(),
            CliAction::Models { json: true }
        );
        let parsed = parse_cli(&s(&[
            "agent",
            "run",
            "--workspace",
            "/tmp/ws",
            "--task",
            "Check",
            "--policy",
            "workspace-write",
            "--exec-policy",
            "safe-inspect",
            "--json",
        ]))
        .unwrap();
        match parsed {
            CliAction::AgentRun(a) => {
                assert_eq!(a.workspace, PathBuf::from("/tmp/ws"));
                assert_eq!(a.policy, AgentPolicy::WorkspaceWrite);
                assert_eq!(a.exec_policy, ExecPolicy::SafeInspectOnly);
                assert!(a.ndjson);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // WorkspaceWrite without explicit --exec-policy defaults to ConfirmUserCode (safe 2-axis separation!)
        let def_exec = parse_cli(&s(&[
            "agent",
            "run",
            "-C",
            "/tmp/ws",
            "-t",
            "Check",
            "--policy",
            "workspace-write",
        ]))
        .unwrap();
        match def_exec {
            CliAction::AgentRun(a) => assert_eq!(a.exec_policy, ExecPolicy::ConfirmUserCode),
            other => panic!("unexpected: {other:?}"),
        }
        // Missing workspace must error
        assert!(parse_cli(&s(&["agent", "run", "--task", "x"])).is_err());
    }
}
