//! Deterministic checks for cancellation boundaries and attempt ownership.
use super::*;
use std::future::{pending, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tokio::sync::{oneshot, Notify};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kaic-m1-video-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn already_cancelled_video_stage_is_never_polled() {
    let (_tx, mut rx) = tokio::sync::watch::channel(true);
    let polls = AtomicUsize::new(0);
    let result = video_stage(&mut rx, async {
        polls.fetch_add(1, Ordering::SeqCst);
        "must not run"
    })
    .await;
    assert_eq!(result, None);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_between_stages_prevents_the_next_stage() {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let stages = AtomicUsize::new(0);
    assert_eq!(
        video_stage(&mut rx, async {
            stages.fetch_add(1, Ordering::SeqCst);
            1
        })
        .await,
        Some(1)
    );
    tx.send(true).unwrap();
    assert_eq!(
        video_stage(&mut rx, async {
            stages.fetch_add(1, Ordering::SeqCst);
            2
        })
        .await,
        None
    );
    assert_eq!(stages.load(Ordering::SeqCst), 1);
}

struct PendingStage {
    entered: Option<oneshot::Sender<()>>,
    dropped: Arc<Notify>,
}

impl Future for PendingStage {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(tx) = self.entered.take() {
            let _ = tx.send(());
        }
        Poll::Pending
    }
}

impl Drop for PendingStage {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

#[tokio::test]
async fn cancellation_drops_the_current_pending_stage_before_returning() {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let (entered_tx, entered_rx) = oneshot::channel();
    let dropped = Arc::new(Notify::new());
    let worker = tokio::spawn({
        let dropped = dropped.clone();
        async move {
            video_stage(
                &mut rx,
                PendingStage {
                    entered: Some(entered_tx),
                    dropped,
                },
            )
            .await
        }
    });
    entered_rx.await.unwrap();
    tx.send(true).unwrap();
    assert_eq!(worker.await.unwrap(), None);
    // notify_one retains a permit, so this is a drop signal rather than a sleep.
    tokio::time::timeout(std::time::Duration::from_secs(2), dropped.notified())
        .await
        .expect("stage resources must be released before cancellation returns");
}

struct ForbiddenBackend(Arc<AtomicUsize>);

impl ForbiddenBackend {
    fn forbidden(&self) -> ! {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("a pre-cancelled Video pipeline contacted a model backend");
    }
}

#[async_trait::async_trait]
impl crate::model_backend::ModelBackend for ForbiddenBackend {
    async fn is_loaded(&self, _model: &str) -> anyhow::Result<bool> {
        self.forbidden()
    }

    async fn load(&self, _model: &str) -> anyhow::Result<()> {
        self.forbidden()
    }

    async fn unload(&self, _model: &str) -> anyhow::Result<()> {
        self.forbidden()
    }

    async fn generate(
        &self,
        _model: &str,
        _request: GenerateRequest,
    ) -> anyhow::Result<GenerateResponse> {
        self.forbidden()
    }
}

async fn stopped_video_dispatch(status: TaskStatus) {
    let (mut state, fixture_dir) = super::tests::pipeline_fixture(0);
    let _fixture_dir = TempDir(fixture_dir.clone());
    // Include the real extraction label, so a missing label cannot hide a
    // regression by returning before the forbidden backend is contacted.
    let resources = fixture_dir.join("video-resources.yaml");
    std::fs::write(
        &resources,
        format!(
            "{}:\n  vram_mb: 1000\n  load_seconds: 1\n",
            media_query_extract::EXTRACTION_MODEL_LABEL
        ),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    state.scheduler = Arc::new(Scheduler::new(
        Arc::new(
            crate::capability_registry::CapabilityRegistry::from_file(
                fixture_dir.join("capability_registry.yaml"),
            )
            .unwrap(),
        ),
        Arc::new(ResourceRegistry::from_file(resources).unwrap()),
        Arc::new(ForbiddenBackend(calls.clone())),
        24495,
    ));
    let task = state
        .task_store
        .create(Category::Video.as_str())
        .await
        .unwrap();
    state.task_store.set_status(task.id, status).await.unwrap();
    let (_tx, mut rx) = tokio::sync::watch::channel(true);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run_task_pipeline_inner(
            &state.task_store,
            &state.scheduler,
            &state.control,
            &mut rx,
            task.id,
            Category::Video.as_str().to_string(),
            vec![Message::new(Role::User, "mountain lake")],
            false,
            None,
        ),
    )
    .await
    .expect("a pre-cancelled pipeline must return without external work");
    let task = state.task_store.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, status);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(state.scheduler.snapshot().await.is_empty());
    assert!(!task.context.iter().any(|entry| entry.role == ARTIFACT_ROLE));
    assert!(!media_cache_dir(task.id).exists());
    assert!(!media_render_dir(task.id).exists());
}

#[tokio::test]
async fn actual_video_dispatch_respects_pre_cancel_without_provider_calls() {
    stopped_video_dispatch(TaskStatus::Cancelled).await;
}

#[tokio::test]
async fn actual_video_dispatch_respects_pre_pause_without_provider_calls() {
    stopped_video_dispatch(TaskStatus::Paused).await;
}

#[test]
fn interrupted_attempt_removes_its_directories_and_preserves_other_attempts() {
    let dir = TempDir::new();
    let cache = dir.0.join("cache/this-attempt");
    let render = dir.0.join("render/this-attempt");
    let previous = dir.0.join("render/previous-success");
    for path in [&cache, &render, &previous] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("asset"), "owned content").unwrap();
    }
    drop(VideoResources {
        cache: cache.clone(),
        render: render.clone(),
        keep: false,
    });
    assert!(!cache.exists());
    assert!(!render.exists());
    assert_eq!(
        std::fs::read_to_string(previous.join("asset")).unwrap(),
        "owned content"
    );
}

#[test]
fn successful_attempt_keeps_its_artifacts_when_guard_is_dropped() {
    let dir = TempDir::new();
    let cache = dir.0.join("cache");
    let render = dir.0.join("render");
    for path in [&cache, &render] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("asset"), "completed content").unwrap();
    }
    drop(VideoResources {
        cache: cache.clone(),
        render: render.clone(),
        keep: true,
    });
    assert_eq!(
        std::fs::read_to_string(cache.join("asset")).unwrap(),
        "completed content"
    );
    assert_eq!(
        std::fs::read_to_string(render.join("asset")).unwrap(),
        "completed content"
    );
}

#[tokio::test]
async fn dropping_the_video_stage_caller_releases_its_current_stage() {
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let (entered_tx, entered_rx) = oneshot::channel();
    let dropped = Arc::new(Notify::new());
    let worker = tokio::spawn({
        let dropped = dropped.clone();
        async move {
            video_stage(
                &mut rx,
                PendingStage {
                    entered: Some(entered_tx),
                    dropped,
                },
            )
            .await;
            pending::<()>().await;
        }
    });
    entered_rx.await.unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), dropped.notified())
        .await
        .expect("dropping the caller must release the pending stage");
}
