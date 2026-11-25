//! Independent executor ownership, panic, and graceful shutdown regressions.
use super::*;
use crate::model_backend::{ModelBackend, ToolCall};
use crate::task_store::ToolServerIdentity;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

struct FixtureDir(std::path::PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Signals {
    ready: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    dropped: AtomicBool,
    drop_notify: Notify,
}

impl Signals {
    fn new() -> (Arc<Self>, oneshot::Receiver<()>) {
        let (ready, receiver) = oneshot::channel();
        (
            Arc::new(Self {
                ready: std::sync::Mutex::new(Some(ready)),
                dropped: AtomicBool::new(false),
                drop_notify: Notify::new(),
            }),
            receiver,
        )
    }

    fn entered(&self) {
        if let Some(ready) = self.ready.lock().unwrap().take() {
            let _ = ready.send(());
        }
    }
}

struct DropSignal(Arc<Signals>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
        self.0.drop_notify.notify_waiters();
    }
}

struct ControlledBackend {
    signals: Arc<Signals>,
    panic: bool,
    loaded: AtomicBool,
}

#[async_trait::async_trait]
impl ModelBackend for ControlledBackend {
    async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
        Ok(self.loaded.load(Ordering::SeqCst))
    }

    async fn load(&self, _model: &str) -> anyhow::Result<()> {
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn unload(&self, _model: &str) -> anyhow::Result<()> {
        self.loaded.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn generate(
        &self,
        _model: &str,
        _request: GenerateRequest,
    ) -> anyhow::Result<GenerateResponse> {
        let _drop = DropSignal(self.signals.clone());
        self.signals.entered();
        assert!(!self.panic, "intentional fixture executor panic");
        std::future::pending().await
    }
}

fn fixture(panic: bool) -> (AppState, FixtureDir, Arc<Signals>, oneshot::Receiver<()>) {
    // Never launch a developer's configured external MCP service in these tests.
    assert!(
        !std::path::Path::new(MCP_CONFIG_PATH).exists(),
        "executor fixture requires no config/mcp.yaml; only synthetic backends are permitted"
    );
    let (mut state, dir) = super::tests::pipeline_fixture(0);
    let dir = FixtureDir(dir);
    let (signals, ready) = Signals::new();
    state.scheduler = Arc::new(Scheduler::new(
        Arc::new(
            crate::capability_registry::CapabilityRegistry::from_file(
                dir.0.join("capability_registry.yaml"),
            )
            .unwrap(),
        ),
        state.resource_registry.clone(),
        Arc::new(ControlledBackend {
            signals: signals.clone(),
            panic,
            loaded: AtomicBool::new(false),
        }),
        24_495,
    ));
    (state, dir, signals, ready)
}

fn spawn_pipeline(state: &AppState, id: Uuid) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_task_pipeline(
        state.task_store.clone(),
        state.scheduler.clone(),
        state.control.clone(),
        id,
        "Simple".into(),
        vec![Message::new(Role::User, "synthetic request")],
        false,
    ))
}

async fn wait_unregistered(state: &AppState, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let changed = state.control.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !state.control.is_running(id).await {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("executor must release ownership after completion or parent abort");
}

#[tokio::test]
async fn aborting_pipeline_monitor_drops_real_generation_and_releases_scheduler() {
    let (state, _dir, signals, ready) = fixture(false);
    let task = state.task_store.create("Simple").await.unwrap();
    let outer = spawn_pipeline(&state, task.id);
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.scheduler.running_generations(), 1);
    assert!(state.control.is_running(task.id).await);
    outer.abort();
    assert!(outer.await.unwrap_err().is_cancelled());
    wait_unregistered(&state, task.id).await;
    assert!(signals.dropped.load(Ordering::SeqCst));
    assert_eq!(state.scheduler.running_generations(), 0);
    assert!(state.scheduler.busy_models().is_empty());
    assert_eq!(
        state.task_store.get(task.id).await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
}

#[tokio::test]
async fn executor_panic_is_observed_and_closes_orphan_running_state() {
    let (state, _dir, signals, ready) = fixture(true);
    let task = state.task_store.create("Simple").await.unwrap();
    let outer = spawn_pipeline(&state, task.id);
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), outer)
        .await
        .unwrap()
        .unwrap();
    assert!(signals.dropped.load(Ordering::SeqCst));
    assert!(!state.control.is_running(task.id).await);
    assert_eq!(state.scheduler.running_generations(), 0);
    let task = state.task_store.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.context.iter().any(|entry| {
        entry.role == "system" && entry.content.contains("аварийно завершился")
    }));
}

#[tokio::test]
async fn aborted_monitor_preserves_unknown_external_outcome_and_waits_child_drop() {
    let (state, _dir) = super::tests::pipeline_fixture(0);
    let _dir = FixtureDir(_dir);
    let task = state.task_store.create("Simple").await.unwrap();
    let call = ToolCall {
        id: "inflight-before-monitor-abort".into(),
        name: "external_write".into(),
        arguments: "{}".into(),
    };
    let snapshot = state
        .task_store
        .park_tool_approval(
            task.id,
            Uuid::new_v4(),
            &call,
            ToolServerIdentity {
                command: "synthetic-mcp".into(),
                args: vec![],
                peer_name: "fixture".into(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    state
        .task_store
        .claim_tool_approval(task.id, snapshot.request_id, true)
        .await
        .unwrap()
        .unwrap();
    assert!(state
        .task_store
        .begin_approved_execution(&snapshot)
        .await
        .unwrap());
    let _receiver = state.control.register(task.id).await.unwrap();
    let (signals, ready) = Signals::new();
    let (installed, monitor_ready) = oneshot::channel();
    let outer = tokio::spawn({
        let state = state.clone();
        let signals = signals.clone();
        async move {
            let executor = tokio::spawn(async move {
                let _drop = DropSignal(signals.clone());
                signals.entered();
                std::future::pending::<()>().await;
            });
            let _monitor = ExecutionMonitor {
                executor: Some(executor),
                task_store: state.task_store.clone(),
                control: state.control.clone(),
                task_id: task.id,
            };
            installed.send(()).unwrap();
            std::future::pending::<()>().await;
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        ready.await.unwrap();
        monitor_ready.await.unwrap();
    })
    .await
    .unwrap();
    outer.abort();
    assert!(outer.await.unwrap_err().is_cancelled());
    wait_unregistered(&state, task.id).await;
    assert!(signals.dropped.load(Ordering::SeqCst));
    let task = state.task_store.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::WaitingForHuman);
    assert_eq!(
        task.recovery.unwrap().unresolved_tools,
        vec![snapshot.clone()]
    );
    assert!(!state
        .task_store
        .begin_approved_execution(&snapshot)
        .await
        .unwrap());
}

#[tokio::test]
async fn graceful_shutdown_waits_generation_drop_and_preserves_unstarted_human_request() {
    let (state, _dir, signals, ready) = fixture(false);
    let task = state.task_store.create("Simple").await.unwrap();
    let outer = spawn_pipeline(&state, task.id);
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    let pending = state.task_store.create("Simple").await.unwrap();
    let snapshot = state
        .task_store
        .park_tool_approval(
            pending.id,
            Uuid::new_v4(),
            &ToolCall {
                id: "not-dispatched".into(),
                name: "external_write".into(),
                arguments: "{}".into(),
            },
            ToolServerIdentity {
                command: "synthetic-mcp".into(),
                args: vec![],
                peer_name: "fixture".into(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        state.control.shutdown(&state.task_store),
    )
    .await
    .unwrap()
    .unwrap();
    // Shutdown's return proves child drop and registry cleanup, not merely signal delivery.
    assert!(signals.dropped.load(Ordering::SeqCst));
    assert!(!state.control.is_running(task.id).await);
    assert_eq!(state.scheduler.running_generations(), 0);
    assert_eq!(
        state.task_store.get(task.id).await.unwrap().unwrap().status,
        TaskStatus::Paused
    );
    outer.await.unwrap();
    let pending = state.task_store.get(pending.id).await.unwrap().unwrap();
    assert_eq!(pending.status, TaskStatus::WaitingForHuman);
    assert_eq!(pending.pending_tool_approval, Some(snapshot));
    assert!(pending.recovery.is_none());
    state.control.shutdown(&state.task_store).await.unwrap();
}
