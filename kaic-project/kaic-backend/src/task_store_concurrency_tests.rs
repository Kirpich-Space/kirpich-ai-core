use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

struct DatabasePath(std::path::PathBuf);

impl DatabasePath {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("kaic-context-{}.db", Uuid::new_v4())))
    }
}

impl Drop for DatabasePath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn entry(content: &str) -> ContextEntry {
    ContextEntry {
        role: "user".into(),
        content: content.into(),
        at: Utc::now(),
        tool_call_id: None,
    }
}

struct BusyGate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

thread_local! {
    static BUSY_GATE: RefCell<Option<BusyGate>> = const { RefCell::new(None) };
}

// SQLite calls this on the appender's blocking worker, at the actual lock
// conflict. No sleeps or assumptions about how quickly either writer runs.
fn wait_for_competing_commit(_: i32) -> bool {
    BUSY_GATE.with(|slot| {
        let Some(gate) = slot.borrow_mut().take() else {
            return false;
        };
        gate.entered.send(()).is_ok() && gate.release.recv_timeout(Duration::from_secs(10)).is_ok()
    })
}

#[test]
fn append_preserves_recovery_history_committed_by_another_connection() {
    for journal in ["DELETE", "WAL"] {
        let path = DatabasePath::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = TaskStore::new(&path.0).unwrap();
        let connection = Connection::open(&path.0).unwrap();
        let actual: String = connection
            .query_row(&format!("PRAGMA journal_mode={journal}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(actual.eq_ignore_ascii_case(journal));
        let (task, approval) = runtime.block_on(async {
            let task = store.create("Programming").await.unwrap();
            store
                .append_context(task.id, entry("original"))
                .await
                .unwrap();
            let approval = store
                .begin_auto_approved_execution(
                    task.id,
                    Uuid::new_v4(),
                    &crate::model_backend::ToolCall {
                        id: "call_write".into(),
                        name: "external_write".into(),
                        arguments: "{}".into(),
                    },
                    ToolServerIdentity {
                        command: "synthetic-server".into(),
                        args: Vec::new(),
                        peer_name: "synthetic-peer".into(),
                    },
                )
                .await
                .unwrap()
                .unwrap();
            (task, approval)
        });
        let tx = immediate_transaction(&connection).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let appender = std::thread::spawn(move || {
            let gate = Arc::new(Mutex::new(Some(BusyGate {
                entered: entered_tx,
                release: release_rx,
            })));
            // Only one blocking worker: its thread-local gate belongs solely
            // to this connection and cannot interact with parallel tests.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .on_thread_start(move || {
                    BUSY_GATE.with(|slot| *slot.borrow_mut() = gate.lock().unwrap().take());
                })
                .build()
                .unwrap();
            store
                .conn
                .lock()
                .unwrap()
                .busy_handler(Some(wait_for_competing_commit))
                .unwrap();
            runtime.block_on(store.append_context(task.id, entry("concurrent message")))
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(10));
        // Commit the real recovery path while append is blocked by SQLite.
        // An autocommit read/modify/write has already read the old YAML here;
        // BEGIN IMMEDIATE must instead wait before reading any task fields.
        let reason = "external effect may have happened; do not replay";
        assert!(mark_unknown_in_transaction(&tx, task.id, reason).unwrap());
        tx.commit().unwrap();
        let _ = release_tx.send(());
        let appended = appender.join().unwrap();
        entered.expect("append must reach the SQLite lock conflict");
        appended.unwrap();
        drop(connection);

        // Verify persisted data through a fresh connection, including the
        // lifecycle/ledger changes that must coexist with the appended text.
        let reopened = TaskStore::new(&path.0).unwrap();
        let saved = runtime.block_on(reopened.get(task.id)).unwrap().unwrap();
        let history: Vec<_> = saved
            .context
            .iter()
            .map(|entry| entry.content.as_str())
            .collect();
        assert_eq!(
            history,
            ["original", reason, "concurrent message"],
            "{journal}"
        );
        assert_eq!(saved.status, TaskStatus::WaitingForHuman);
        let recovery = saved.recovery.unwrap();
        assert_eq!(recovery.message, reason);
        assert_eq!(recovery.unresolved_tools, vec![approval.clone()]);
        assert!(!runtime
            .block_on(reopened.begin_approved_execution(&approval))
            .unwrap());
    }
}

#[tokio::test]
async fn failed_append_rolls_back_context_and_timestamp_and_releases_writer() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let task = store.create("Simple").await.unwrap();
    store
        .append_context(task.id, entry("original"))
        .await
        .unwrap();
    let before = store.get(task.id).await.unwrap().unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_context AFTER UPDATE OF context ON tasks
         BEGIN SELECT RAISE(FAIL, 'synthetic failure after row update'); END;",
        )
        .unwrap();
    assert!(store
        .append_context(task.id, entry("must roll back"))
        .await
        .is_err());
    let after = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(after.context.len(), 1);
    assert_eq!(after.context[0].content, "original");
    assert_eq!(after.updated_at, before.updated_at);
    // Another connection must be able to write after the error; no stranded
    // transaction or lock may prevent a later append from succeeding.
    let other = TaskStore::new(&path.0).unwrap();
    other
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_context")
        .unwrap();
    other.append_context(task.id, entry("retry")).await.unwrap();
    let saved = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(saved.context.len(), 2);
    assert_eq!(saved.context[1].content, "retry");
}

#[tokio::test]
async fn corrupt_context_is_reported_and_preserved_without_blocking_other_tasks() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let broken = store.create("Simple").await.unwrap();
    let healthy = store.create("Simple").await.unwrap();
    let corrupt = "[unclosed legacy YAML";
    store
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE tasks SET context=?1 WHERE id=?2",
            rusqlite::params![corrupt, broken.id.to_string()],
        )
        .unwrap();
    let error = store
        .append_context(broken.id, entry("must not replace corruption"))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("не удалось разобрать контекст задачи"));
    let other = TaskStore::new(&path.0).unwrap();
    other
        .append_context(healthy.id, entry("still writable"))
        .await
        .unwrap();
    let (context, updated_at): (String, String) = other
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT context,updated_at FROM tasks WHERE id=?1",
            [broken.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(context, corrupt);
    assert_eq!(updated_at, broken.updated_at.to_rfc3339());
    assert!(other.get(broken.id).await.is_err());
}

#[tokio::test]
async fn busy_append_returns_an_error_without_writing_and_can_be_retried() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let task = store.create("Simple").await.unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let connection = Connection::open(&path.0).unwrap();
    let tx = immediate_transaction(&connection).unwrap();
    let error = store
        .append_context(task.id, entry("retry once"))
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy),
    );
    tx.rollback().unwrap();
    let unchanged = store.get(task.id).await.unwrap().unwrap();
    assert!(unchanged.context.is_empty());
    assert_eq!(unchanged.updated_at, task.updated_at);
    store
        .append_context(task.id, entry("retry once"))
        .await
        .unwrap();
    assert_eq!(store.get(task.id).await.unwrap().unwrap().context.len(), 1);
}
