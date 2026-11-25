//! Единственный фоновый исполнитель. GUI и --smoke-chat используют ManagedServer.
use crate::{
    agent::{
        AgentEvent, AgentLimits, AgentPolicy, AgentStreamEvent, AnthropicApiBackend,
        ApprovalDecision, ApprovalGate, CodexCliConfig, ContextMetrics, ExecPolicy,
        ManagedServerBackend, OllamaBackend, OpenAiCompatBackend, PendingApproval, ProviderKind,
        SubtaskRecord, run_agent_with_policies, run_agy_cli, run_claude_cli, run_codex_cli,
    },
    catalog::{Model, ModelKind},
    kaic::{KaicClient, Secret, Task, TaskPhase, task_phase},
    llama::ManagedServer,
    media::{ComfyClient, MediaRecord},
    network::{Cancel, LocalEndpoint},
    sd_cpp,
    sse::StreamEvent,
    storage::{GeneratedMedia, MediaGenOptions, Message},
};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    thread,
    time::Duration,
};
use uuid::Uuid;

pub enum Job {
    Direct {
        chat: Uuid,
        model: Model,
        context: u32,
        messages: Vec<Message>,
        max_tokens: u32,
    },
    AgentLocal {
        chat: Uuid,
        model: Model,
        context: u32,
        workspace: PathBuf,
        policy: AgentPolicy,
        exec_policy: ExecPolicy,
        task: String,
        prior_messages: Vec<Message>,
        limits: AgentLimits,
    },
    AgentRemoteApi {
        chat: Uuid,
        provider: ProviderKind,
        endpoint: String,
        model: String,
        context: u32,
        workspace: PathBuf,
        policy: AgentPolicy,
        exec_policy: ExecPolicy,
        external_consent: bool,
        task: String,
        prior_messages: Vec<Message>,
        limits: AgentLimits,
    },
    AgentExternalCli {
        chat: Uuid,
        provider: ProviderKind,
        workspace: PathBuf,
        policy: AgentPolicy,
        task: String,
        external_consent: bool,
        model: Option<String>,
    },
    GenerateMedia {
        chat: Uuid,
        sd_cli: String,
        model: Model,
        kind: ModelKind,
        prompt: String,
        options: MediaGenOptions,
        outputs: PathBuf,
    },
    Kaic {
        chat: Uuid,
        url: String,
        token: Secret,
        text: String,
        resume: Option<String>,
    },
    InspectKaic {
        url: String,
        token: Secret,
    },
    InspectComfy {
        url: String,
    },
    Media {
        url: String,
        workflow: Value,
        prompt: String,
        outputs: PathBuf,
    },
    Preview(PathBuf),
    Unload,
    Quit,
}
#[derive(Debug)]
pub enum Event {
    Status(String),
    Delta(Uuid, String, bool),
    Metrics {
        chat: Uuid,
        tokens: u32,
        tps: f32,
        seconds: f32,
    },
    Task(Uuid, Task),
    AgentEvent(Uuid, AgentEvent),
    Subtask(Uuid, SubtaskRecord),
    SeatUpdate(Uuid, crate::agent::AgentSeat),
    ContextUsage(Uuid, ContextMetrics),
    ApprovalNeeded(Uuid, PendingApproval),
    ApprovalCleared(Uuid),
    Loaded(Option<String>),
    InspectedKaic(Value),
    InspectedComfy(Value),
    Media(MediaRecord),
    GeneratedMedia(Uuid, GeneratedMedia),
    Preview(PathBuf, usize, usize, Vec<u8>),
    Error(String),
    Finished,
}
#[derive(Default)]
struct Admission {
    busy: bool,
    closing: bool,
}
pub struct Worker {
    admission: Arc<Mutex<Admission>>,
    tx: tokio::sync::mpsc::UnboundedSender<Job>,
    approval_tx: mpsc::Sender<ApprovalDecision>,
    pub rx: mpsc::Receiver<Event>,
    pub cancel: Cancel,
    join: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        Self::spawn(repaint, || {})
    }
    pub fn respond_approval(&self, request_id: String, action_digest: String, approved: bool) {
        let _ = self.approval_tx.send(ApprovalDecision {
            request_id,
            action_digest,
            approved,
        });
    }
    fn spawn(repaint: impl Fn() + Send + 'static, before: impl FnOnce() + Send + 'static) -> Self {
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let (events, rx) = mpsc::channel();
        let (approval_tx, approval_rx) = mpsc::channel::<ApprovalDecision>();
        let approval_rx = Arc::new(Mutex::new(approval_rx));
        let admission = Arc::new(Mutex::new(Admission::default()));
        let control = admission.clone();
        let cancel = Cancel::default();
        let c = cancel.clone();
        let join = thread::spawn(move || {
            before();
            let emit = |event| {
                let _ = events.send(event);
                repaint();
            };
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    emit(Event::Error(
                        "Не удалось создать исполнитель запросов".into(),
                    ));
                    return;
                }
            };
            rt.block_on(async {
            let mut server: Option<ManagedServer> = None;
            while let Some(job) = commands.recv().await {
                if matches!(job, Job::Quit)||control.lock().unwrap_or_else(|p|p.into_inner()).closing {
                    break;
                }
                // Сбрасываем старые непотреблённые решения подтверждений перед новой задачей.
                if let Ok(rx) = approval_rx.lock() {
                    while rx.try_recv().is_ok() {}
                }
                let result:Result<()>=async {
          if c.is_cancelled(){bail!("Операция остановлена до отправки");}
          match job {
            Job::Direct{chat,model,context,messages,max_tokens}=>{
              let effective=if context==0{crate::catalog::live_profile(&model.repo)?.context}else{context};
              if !server.as_mut().is_some_and(|s|s.matches(&model,effective)) {
                server.take();emit(Event::Loaded(None));emit(Event::Status(format!("Загрузка {}…",model.alias())));
                server=Some(ManagedServer::start(&model,context,&c).await?);
                emit(Event::Loaded(Some(model.alias().into())));
              }
              emit(Event::Status("Модель отвечает…".into()));
              let mut limited=false;
              let mut first_token: Option<std::time::Instant> = None;
              let mut last_metric = std::time::Instant::now();
              let mut token_count: u32 = 0;
              let mut server_timings: Option<(u32, f32, f32)> = None;
              server.as_mut().unwrap().chat(&messages,max_tokens,&c,|e|match e {
                StreamEvent::Text(s)=>{
                  let started = *first_token.get_or_insert_with(std::time::Instant::now);
                  token_count += 1;
                  emit(Event::Delta(chat,s,false));
                  if last_metric.elapsed().as_millis() >= 80 {
                    last_metric = std::time::Instant::now();
                    let secs = started.elapsed().as_secs_f32().max(0.01);
                    emit(Event::Metrics{chat, tokens: token_count, tps: token_count as f32 / secs, seconds: secs});
                  }
                }
                StreamEvent::Reasoning(s)=>{
                  let started = *first_token.get_or_insert_with(std::time::Instant::now);
                  token_count += 1;
                  emit(Event::Delta(chat,s,true));
                  if last_metric.elapsed().as_millis() >= 80 {
                    last_metric = std::time::Instant::now();
                    let secs = started.elapsed().as_secs_f32().max(0.01);
                    emit(Event::Metrics{chat, tokens: token_count, tps: token_count as f32 / secs, seconds: secs});
                  }
                }
                StreamEvent::Timings{tokens, tps, seconds}=>{
                  server_timings = Some((tokens, tps, seconds));
                }
                StreamEvent::Limit=>limited=true,
                StreamEvent::ToolCallDelta{..} | StreamEvent::ToolCallsDone | StreamEvent::Done=>{}
              }).await?;
              if let Some((tokens, tps, seconds)) = server_timings {
                emit(Event::Metrics{chat, tokens, tps, seconds});
              } else if let Some(started) = first_token && token_count > 0 {
                let secs = started.elapsed().as_secs_f32().max(0.01);
                emit(Event::Metrics{chat, tokens: token_count, tps: token_count as f32 / secs, seconds: secs});
              }
              emit(Event::Status(if limited{"Достигнут лимит токенов; сохранён неполный ответ"}else{"Ответ завершён"}.into()));
            },
            Job::AgentLocal{chat,model,context,workspace,policy,exec_policy,task,prior_messages,limits}=>{
              let effective=if context==0{crate::catalog::live_profile(&model.repo)?.context}else{context};
              if !server.as_mut().is_some_and(|s|s.matches(&model,effective)) {
                server.take();emit(Event::Loaded(None));emit(Event::Status(format!("Загрузка {}…",model.alias())));
                server=Some(ManagedServer::start(&model,context,&c).await?);
                emit(Event::Loaded(Some(model.alias().into())));
              }
              let (gate, pump_done, pump) = spawn_approval_pump(approval_rx.clone(), c.clone());
              let mut backend = ManagedServerBackend::new(server.as_mut().unwrap(), model.alias().into());
              let res = run_agent_with_policies(
                &mut backend,
                &workspace,
                &task,
                &prior_messages,
                policy,
                exec_policy,
                limits,
                &c,
                &gate,
                |ev| forward_agent_stream(chat, ev, &emit),
              ).await;
              pump_done.store(true, Ordering::SeqCst);
              let _ = pump.join();
              emit(Event::ApprovalCleared(chat));
              let run = res?;
              emit_agent_run_status(None, &run, &emit);
            },
            Job::AgentRemoteApi{chat,provider,endpoint,model,context,workspace,policy,exec_policy,external_consent,task,prior_messages,limits}=>{
              let (gate, pump_done, pump) = spawn_approval_pump(approval_rx.clone(), c.clone());
              let mut sink = |ev| forward_agent_stream(chat, ev, &emit);
              let res = match provider {
                ProviderKind::OllamaLocal => {
                  let mut backend = OllamaBackend::new(&endpoint, model, context)?;
                  run_agent_with_policies(&mut backend, &workspace, &task, &prior_messages, policy, exec_policy, limits, &c, &gate, &mut sink).await
                }
                ProviderKind::OpenAiCompat => {
                  let api_key = env_secret(&["KAIC_OPENAI_API_KEY", "OPENAI_API_KEY"]);
                  let mut backend = OpenAiCompatBackend::new(&endpoint, model, api_key, external_consent)?;
                  run_agent_with_policies(&mut backend, &workspace, &task, &prior_messages, policy, exec_policy, limits, &c, &gate, &mut sink).await
                }
                ProviderKind::AnthropicApi => {
                  let api_key = env_secret(&["KAIC_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"]).unwrap_or_default();
                  let mut backend = AnthropicApiBackend::new(&endpoint, model, api_key, external_consent)?;
                  run_agent_with_policies(&mut backend, &workspace, &task, &prior_messages, policy, exec_policy, limits, &c, &gate, &mut sink).await
                }
                other => bail!("Неожиданный HTTP-провайдер агента: {}", other.label_ru()),
              };
              pump_done.store(true, Ordering::SeqCst);
              let _ = pump.join();
              emit(Event::ApprovalCleared(chat));
              let run = res?;
              emit_agent_run_status(None, &run, &emit);
            },
            Job::AgentExternalCli{chat,provider,workspace,policy,task,external_consent,model}=>{
              let (gate, pump_done, pump) = spawn_approval_pump(approval_rx.clone(), c.clone());
              let cfg = CodexCliConfig {
                workspace,
                prompt: task,
                policy,
                external_consent,
                model,
              };
              let mut sink = |ev| forward_agent_stream(chat, ev, &emit);
              let res = match provider {
                ProviderKind::CodexCli => run_codex_cli(cfg, &c, &gate, &mut sink).await,
                ProviderKind::ClaudeCli => run_claude_cli(cfg, &c, &gate, &mut sink).await,
                ProviderKind::AgyCli => run_agy_cli(cfg, &c, &gate, &mut sink).await,
                other => bail!("Неожиданный внешний CLI-провайдер: {}", other.label_ru()),
              };
              pump_done.store(true, Ordering::SeqCst);
              let _ = pump.join();
              emit(Event::ApprovalCleared(chat));
              let run = res?;
              emit_agent_run_status(Some(provider.short_label()), &run, &emit);
            },
            Job::GenerateMedia{chat,sd_cli,model,kind,prompt,options,outputs}=>{
              // Освобождаем всю VRAM RTX 4060, выгружая llama-server перед запуском sd-cli
              if server.take().is_some() {
                emit(Event::Loaded(None));
                emit(Event::Status("Unloading llama-server to free VRAM…".into()));
              }
              let generated = sd_cpp::generate(&sd_cli,&model,kind,&prompt,&options,&outputs,&c,|s|emit(Event::Status(s))).await?;
              let thumb_path = generated.preview_path.clone().unwrap_or_else(|| generated.path.clone());
              if let Ok((w,h,p)) = preview(&thumb_path) {
                emit(Event::Preview(thumb_path,w,h,p));
              }
              let summary = match kind {
                ModelKind::Video => format!(
                  "Video ready · {}×{} · {}f · {:.1}s",
                  generated.width,
                  generated.height,
                  generated.frames.unwrap_or(0),
                  generated.elapsed_secs.unwrap_or(0.0)
                ),
                _ => format!(
                  "Image ready · {}×{} · {:.1}s",
                  generated.width,
                  generated.height,
                  generated.elapsed_secs.unwrap_or(0.0)
                ),
              };
              emit(Event::GeneratedMedia(chat, generated));
              emit(Event::Status(summary));
            },
            Job::Kaic{chat,url,token,text,resume}=>{
              let client=KaicClient::new(LocalEndpoint::parse(&url)?,token)?;
              emit(Event::Status("Соединение с KAIC…".into()));
              let task=if let Some(id)=resume {tokio::select!{_=c.cancelled()=>bail!("Обновление задачи остановлено"),r=client.get(&id)=>r?}} else {client.submit_cancellable(&text,&c).await?};
              let task=client.follow(task,&c,|t|emit(Event::Task(chat,t.clone()))).await?;
              emit(Event::Task(chat,task.clone()));
              if task.pending_tool_approval.is_none(){match task_phase(&task.status) {
                TaskPhase::Done=>emit(Event::Status("KAIC: задача завершена".into())),
                TaskPhase::Failed=>bail!("KAIC: задача завершилась ошибкой"),
                TaskPhase::Cancelled=>emit(Event::Status("KAIC: задача отменена".into())),
                TaskPhase::Waiting=>emit(Event::Status("KAIC ожидает человека. Подтверждение инструментов и продолжение здесь не поддерживаются.".into())),
                TaskPhase::Active=>emit(Event::Status("KAIC: запрос отмены отправлен; завершение ещё не подтверждено. Обновите задачу.".into()))
              }
              }
              if task.pending_tool_approval.is_some(){emit(Event::Status("KAIC ожидает подтверждения инструмента. Автоматическое разрешение запрещено; используйте интерфейс KAIC.".into()));}
            },
            Job::InspectKaic{url,token}=>{
              let client=KaicClient::new(LocalEndpoint::parse(&url)?,token.clone())?;
              let mut v=tokio::select!{_=c.cancelled()=>bail!("Проверка остановлена"),v=client.inspect()=>v?};
              redact(&mut v,token.expose());emit(Event::InspectedKaic(v));emit(Event::Status("KAIC доступен; статус и каталоги получены".into()));
            },
            Job::InspectComfy{url}=>{
              let client=ComfyClient::new(LocalEndpoint::parse(&url)?)?;
              let v=tokio::select!{_=c.cancelled()=>bail!("Проверка остановлена"),v=client.inspect()=>v?};
              emit(Event::InspectedComfy(v));emit(Event::Status("ComfyUI доступен; узлы и модели получены".into()));
            },
            Job::Media{url,workflow,prompt,outputs}=>{
              let client=ComfyClient::new(LocalEndpoint::parse(&url)?)?;
              let record=client.run(workflow,prompt,&outputs,&c,|s|emit(Event::Status(s))).await?;
              emit(Event::Media(record.clone()));
              if let Some(a)=record.outputs.iter().find(|a|is_image(&a.path)) {match preview(&a.path){Ok((w,h,p))=>emit(Event::Preview(a.path.clone(),w,h,p)),Err(_)=>emit(Event::Status("Результат скачан; предпросмотр этого формата недоступен".into()))}}
              emit(Event::Status(format!("Медиа получено: {} файлов · {:.1} с",record.outputs.len(),record.elapsed_seconds)));
            },
            Job::Preview(path)=>{let (w,h,p)=preview(&path)?;emit(Event::Preview(path,w,h,p));},
            Job::Unload=>{server.take();emit(Event::Loaded(None));emit(Event::Status("Собственный сервер выгружен".into()));},
            Job::Quit=>{}
          }Ok(())
        }.await;
                if let Err(e) = result {
                    emit(Event::Error(format!("{e:#}")));
                }
                control.lock().unwrap_or_else(|p|p.into_inner()).busy=false;
                emit(Event::Finished);
            }
            drop(server); // kill + wait strictly for our own child.
            });
        });
        Self {
            admission,
            tx,
            approval_tx,
            rx,
            cancel,
            join: Some(join),
        }
    }
    pub fn submit(&self, job: Job) -> Result<()> {
        let mut state = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing {
            bail!("Исполнитель завершает работу; новые задания не принимаются");
        }
        if state.busy {
            bail!("Текущая операция ещё не завершилась; дождитесь остановки");
        }
        state.busy = true;
        self.cancel.0.store(false, Ordering::SeqCst);
        if self.tx.send(job).is_err() {
            state.busy = false;
            bail!("Фоновый исполнитель завершился");
        }
        Ok(())
    }
    pub fn finish(&mut self) {
        self.shutdown();
        if let Some(t) = self.join.take() {
            let _ = t.join();
        }
    }
    pub fn shutdown(&self) {
        let mut state = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        state.closing = true;
        self.cancel.cancel();
        let _ = self.tx.send(Job::Quit);
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(t) = self.join.take() {
            let _ = t.join();
        }
    }
}
fn redact(v: &mut Value, token: &str) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                if k.to_lowercase().contains("token")
                    || k.to_lowercase().contains("authorization")
                    || k.to_lowercase().contains("secret")
                {
                    *v = Value::String("[скрыто]".into());
                } else {
                    redact(v, token)
                }
            }
        }
        Value::Array(a) => {
            for v in a {
                redact(v, token)
            }
        }
        Value::String(s) if !token.is_empty() => *s = s.replace(token, "[скрыто]"),
        _ => {}
    }
}
pub fn is_image(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["png", "jpg", "jpeg", "webp", "gif", "avif", "svg"]
            .contains(&e.to_ascii_lowercase().as_str())
    })
}
fn preview(path: &std::path::Path) -> Result<(usize, usize, Vec<u8>)> {
    if crate::attachments::is_svg_path(path) {
        let bytes = std::fs::read(path)?;
        let im = crate::attachments::render_svg_dynamic_image(&bytes, 1400)?.to_rgba8();
        return Ok((im.width() as usize, im.height() as usize, im.into_raw()));
    }
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(96 * 1024 * 1024);
    reader.limits(limits);
    let im = reader.decode()?.thumbnail(1400, 1000).to_rgba8();
    Ok((im.width() as usize, im.height() as usize, im.into_raw()))
}

fn env_secret(names: &[&str]) -> Option<Secret> {
    for name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(Secret::new(trimmed.to_string()));
            }
        }
    }
    None
}

fn spawn_approval_pump(
    approval_rx: Arc<Mutex<mpsc::Receiver<ApprovalDecision>>>,
    cancel: Cancel,
) -> (
    ApprovalGate,
    Arc<std::sync::atomic::AtomicBool>,
    thread::JoinHandle<()>,
) {
    let (gate, handle) = ApprovalGate::interactive();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_bg = done.clone();
    let pump = thread::spawn(move || {
        while !done_bg.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            let _ = handle.try_recv_request();
            if let Ok(rx) = approval_rx.lock()
                && let Ok(dec) = rx.try_recv()
            {
                handle.respond(dec.request_id, dec.action_digest, dec.approved);
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    (gate, done, pump)
}

fn forward_agent_stream(chat: Uuid, ev: AgentStreamEvent, emit: &dyn Fn(Event)) {
    match ev {
        AgentStreamEvent::Status(s) => emit(Event::Status(s)),
        AgentStreamEvent::TextDelta(s) => emit(Event::Delta(chat, s, false)),
        AgentStreamEvent::Event(e) => {
            if matches!(e, AgentEvent::ApprovalResolved { .. }) {
                emit(Event::ApprovalCleared(chat));
            }
            emit(Event::AgentEvent(chat, e));
        }
        AgentStreamEvent::Subtask(st) => emit(Event::Subtask(chat, st)),
        AgentStreamEvent::SeatUpdate(seat) => emit(Event::SeatUpdate(chat, seat)),
        AgentStreamEvent::ContextUsage(m) => emit(Event::ContextUsage(chat, m)),
        AgentStreamEvent::ApprovalNeeded(req) => emit(Event::ApprovalNeeded(chat, req)),
        AgentStreamEvent::Metrics {
            tokens,
            tps,
            seconds,
        } => emit(Event::Metrics {
            chat,
            tokens,
            tps,
            seconds,
        }),
    }
}

fn emit_agent_run_status(
    cli_label: Option<&str>,
    run: &crate::agent::AgentRunResult,
    emit: &dyn Fn(Event),
) {
    if let Some(lbl) = cli_label {
        if run.cancelled {
            emit(Event::Status(format!("{lbl} CLI остановлен пользователем")));
        } else if run.blocked {
            emit(Event::Status(format!(
                "{lbl} CLI отклонён или заблокирован"
            )));
        } else {
            emit(Event::Status(format!(
                "{lbl} CLI завершил задачу · файлов: {}",
                run.changed_files.len()
            )));
        }
    } else if run.cancelled {
        emit(Event::Status(
            "Агент остановлен; выполненные изменения сохранены".into(),
        ));
    } else if run.blocked {
        emit(Event::Status("Агент остановлен с блокером".into()));
    } else {
        emit(Event::Status(format!(
            "Агент завершил задачу · файлов изменено: {}",
            run.changed_files.len()
        )));
    }
}

pub async fn smoke_chat(alias: &str) -> Result<String> {
    let (models, _) = crate::catalog::discover(std::path::Path::new(crate::catalog::MODEL_ROOT));
    let model = models
        .iter()
        .find(|m| {
            m.loadable()
                && (m.alias() == alias
                    || m.repo == alias
                    || m.path.to_str() == Some(alias)
                    || m.filename == alias)
        })
        .ok_or_else(|| anyhow::anyhow!("Проверенная модель с таким именем или путём не найдена"))?;
    let cancel = Cancel::default();
    let mut server = ManagedServer::start(model, 0, &cancel).await?;
    let mut text = String::new();
    tokio::time::timeout(
        Duration::from_secs(180),
        server.chat(
            &[Message::user(
                "Ответь по-русски одной короткой фразой: сколько будет два плюс два?".into(),
            )],
            64,
            &cancel,
            |e| {
                if let StreamEvent::Text(s) = e {
                    text.push_str(&s)
                }
            },
        ),
    )
    .await??;
    if text.trim().is_empty() {
        bail!("Сервер завершил запрос без непустого текстового ответа");
    }
    Ok(text)
}

#[cfg(test)]
mod secret_tests {
    use super::*;
    #[test]
    fn api_inspection_redacts_nested_tokens() {
        let mut v = serde_json::json!({"token":"anything","nested":[{"message":"x test-secret y","Authorization":"Bearer value"}]});
        redact(&mut v, "test-secret");
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("test-secret"));
        assert!(!s.contains("Bearer value"));
        assert!(!s.contains("anything"));
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn shutdown_discards_admitted_but_not_started_job() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, gate) = mpsc::channel();
        let worker = Worker::spawn(
            || {},
            move || {
                gate.recv().unwrap();
            },
        );
        worker
            .submit(Job::Media {
                url,
                workflow: serde_json::json!({"1":{"class_type":"Test","inputs":{}}}),
                prompt: "test".into(),
                outputs: PathBuf::from("/unused-test-path"),
            })
            .unwrap();
        worker.shutdown();
        assert!(worker.submit(Job::Unload).is_err());
        release.send(()).unwrap();
        drop(worker);
        assert!(
            listener.accept().is_err(),
            "После shutdown очередное задание не отправляется в backend"
        );
    }
    #[test]
    fn direct_zero_context_resolves_profile_before_budget() {
        let worker = Worker::new(|| {});
        let model:Model=serde_json::from_value(serde_json::json!({"repo":"test-fixture-without-profile","filename":"missing.gguf","path":"/nonexistent-test-gguf","size":4,"sha256":"test","status":"verified"})).unwrap();
        worker
            .submit(Job::Direct {
                chat: Uuid::new_v4(),
                model,
                context: 0,
                messages: vec![Message::user("Короткий запрос".into())],
                max_tokens: 64,
            })
            .unwrap();
        let mut error = String::new();
        loop {
            match worker.rx.recv_timeout(Duration::from_secs(3)).unwrap() {
                Event::Error(e) => error = e,
                Event::Finished => break,
                _ => {}
            }
        }
        assert!(
            error.contains("Модель не проверена"),
            "Запрос должен пройти бюджет профиля и дойти до проверки файла: {error}"
        );
    }
}
