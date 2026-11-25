use super::*;

fn call() -> crate::model_backend::ToolCall {
    crate::model_backend::ToolCall {
        id: "call_original".into(),
        name: "external_write".into(),
        arguments: r#"{"path":"original"}"#.into(),
    }
}

fn server() -> ToolServerIdentity {
    ToolServerIdentity {
        command: "local-test-server".into(),
        args: vec!["--test".into()],
        peer_name: "test-peer".into(),
    }
}

async fn claimed(store: &TaskStore, task_id: Uuid) -> ToolApproval {
    let approval = store
        .park_tool_approval(task_id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .unwrap();
    store
        .claim_tool_approval(task_id, approval.request_id, true)
        .await
        .unwrap()
        .unwrap()
}

struct DatabasePath(std::path::PathBuf);

impl DatabasePath {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("kaic-recovery-{}.db", Uuid::new_v4())))
    }
}

impl Drop for DatabasePath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn restart_pauses_running_tasks_and_preserves_history_idempotently() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let task = store.create("Simple").await.unwrap();
    store
        .append_context(task.id, recovery_note("original history"))
        .await
        .unwrap();
    drop(store);
    let reopened = TaskStore::new(&path.0).unwrap();
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 1);
    let task = reopened.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Paused);
    assert_eq!(task.context[0].content, "original history");
    assert_eq!(task.recovery.as_ref().unwrap().kind, "interrupted");
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 0);
    assert_eq!(
        reopened.get(task.id).await.unwrap().unwrap().context.len(),
        2
    );
    reopened
        .set_status(task.id, TaskStatus::Running)
        .await
        .unwrap();
    assert!(reopened
        .get(task.id)
        .await
        .unwrap()
        .unwrap()
        .recovery
        .is_none());
}

#[tokio::test]
async fn restart_preserves_pending_approval_without_pretending_it_executed() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    let approval = store
        .park_tool_approval(task.id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.reconcile_startup().await.unwrap(), 0);
    let task = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::WaitingForHuman);
    assert_eq!(task.pending_tool_approval.unwrap(), approval);
    assert!(task.recovery.is_none());
}

#[tokio::test]
async fn restart_claimed_or_executing_requires_acknowledgement_and_never_replays() {
    for executing in [false, true] {
        let path = DatabasePath::new();
        let store = TaskStore::new(&path.0).unwrap();
        let task = store.create("Programming").await.unwrap();
        let approval = claimed(&store, task.id).await;
        if executing {
            assert!(store.begin_approved_execution(&approval).await.unwrap());
        }
        drop(store);
        let store = TaskStore::new(&path.0).unwrap();
        assert_eq!(store.reconcile_startup().await.unwrap(), 1);
        let recovered = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, TaskStatus::WaitingForHuman);
        let recovery = recovered.recovery.unwrap();
        assert_eq!(recovery.kind, "unknown_outcome");
        assert_eq!(recovery.unresolved_tools, vec![approval.clone()]);
        assert!(store.has_unresolved_tool_approval(task.id).await.unwrap());
        assert!(store
            .set_status(task.id, TaskStatus::Running)
            .await
            .is_err());
        assert!(!store
            .finish_if_still_running(task.id, TaskStatus::Done)
            .await
            .unwrap());
        assert!(!store.begin_approved_execution(&approval).await.unwrap());
        assert!(store
            .claim_tool_approval(task.id, approval.request_id, true)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .park_tool_approval(task.id, Uuid::new_v4(), &call(), server())
            .await
            .unwrap()
            .is_none());
        assert_eq!(store.reconcile_startup().await.unwrap(), 0);
        assert!(!store
            .resolve_unknown_tool(Uuid::new_v4(), approval.request_id)
            .await
            .unwrap());
        assert!(store
            .resolve_unknown_tool(task.id, approval.request_id)
            .await
            .unwrap());
        assert!(!store
            .resolve_unknown_tool(task.id, approval.request_id)
            .await
            .unwrap());
        let acknowledged = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(acknowledged.status, TaskStatus::Paused);
        assert!(acknowledged.recovery.is_none());
        assert!(acknowledged
            .context
            .iter()
            .any(|e| e.role == "tool" && e.tool_call_id.as_deref() == Some("call_original")));
        store
            .set_status(task.id, TaskStatus::Running)
            .await
            .unwrap();
        assert!(!store.begin_approved_execution(&approval).await.unwrap());
        let retry = store
            .park_tool_approval(task.id, Uuid::new_v4(), &call(), server())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(retry.request_id, approval.request_id);
    }
}

#[tokio::test]
async fn concurrent_acknowledgements_on_distinct_connections_have_one_winner() {
    let path = DatabasePath::new();
    let first = TaskStore::new(&path.0).unwrap();
    let task = first.create("Programming").await.unwrap();
    let approval = claimed(&first, task.id).await;
    first.reconcile_startup().await.unwrap();
    let second = TaskStore::new(&path.0).unwrap();
    let (a, b) = tokio::join!(
        first.resolve_unknown_tool(task.id, approval.request_id),
        second.resolve_unknown_tool(task.id, approval.request_id)
    );
    assert_eq!(usize::from(a.unwrap()) + usize::from(b.unwrap()), 1);
    let task = first.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.context.iter().filter(|e| e.role == "tool").count(), 1);
}

#[tokio::test]
async fn stopping_claimed_or_executing_preserves_unknown_and_terminal_status() {
    for status in [TaskStatus::Paused, TaskStatus::Cancelled] {
        for executing in [false, true] {
            let store = TaskStore::new(":memory:").unwrap();
            let task = store.create("Programming").await.unwrap();
            let approval = claimed(&store, task.id).await;
            if executing {
                assert!(store.begin_approved_execution(&approval).await.unwrap());
            }
            store.set_status(task.id, status).await.unwrap();
            store
                .mark_inflight_tools_unknown(task.id, "late cancellation cleanup")
                .await
                .unwrap();
            store
                .complete_tool_approval(approval.request_id)
                .await
                .unwrap();
            let task = store.get(task.id).await.unwrap().unwrap();
            assert_eq!(task.status, status);
            assert_eq!(
                task.recovery.unwrap().unresolved_tools,
                vec![approval.clone()]
            );
            assert!(store
                .resolve_unknown_tool(task.id, approval.request_id)
                .await
                .unwrap());
            assert_eq!(store.get(task.id).await.unwrap().unwrap().status, status);
        }
    }
}

#[tokio::test]
async fn timeout_only_marks_inflight_and_preserves_pending_requests() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    let approval = store
        .park_tool_approval(task.id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .unwrap();
    store
        .mark_inflight_tools_unknown(task.id, "timeout")
        .await
        .unwrap();
    let task = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.pending_tool_approval, Some(approval));
    assert!(task.recovery.is_none());
}

#[tokio::test]
async fn allowlisted_calls_have_durable_identity_and_crash_recovery() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    let approval = store
        .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .is_none());
    store
        .mark_inflight_tools_unknown(task.id, "MCP deadline exceeded")
        .await
        .unwrap();
    assert_eq!(
        store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .unwrap()
            .unresolved_tools,
        vec![approval.clone()]
    );
    assert!(!store.begin_approved_execution(&approval).await.unwrap());
    assert!(store
        .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call(), server())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn legacy_database_migrates_additively_and_reads_old_context() {
    let path = DatabasePath::new();
    let id = Uuid::new_v4();
    let conn = Connection::open(&path.0).unwrap();
    conn.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,category TEXT NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,context TEXT NOT NULL,pending_telegram_message_id INTEGER);").unwrap();
    conn.execute("INSERT INTO tasks VALUES (?1,'Simple','running','2026-10-01T00:00:00Z','2026-10-01T00:00:00Z',?2,42)",
        rusqlite::params![id.to_string(), "- role: user\n  content: legacy\n  at: 2026-10-01T00:00:00Z\n"]).unwrap();
    drop(conn);
    let store = TaskStore::new(&path.0).unwrap();
    assert_eq!(store.reconcile_startup().await.unwrap(), 1);
    let task = store.get(id).await.unwrap().unwrap();
    assert_eq!(task.context[0].content, "legacy");
    assert_eq!(task.context[0].tool_call_id, None);
    assert_eq!(task.pending_telegram_message_id, Some(42));
    assert_eq!(task.status, TaskStatus::Paused);
}

#[tokio::test]
async fn unknown_outcome_cannot_be_acknowledged_while_an_executor_is_running() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    let approval = claimed(&store, task.id).await;
    store.reconcile_startup().await.unwrap();
    // A legacy writer or another process can produce this inconsistent state.
    // Recovery decisions must reject it atomically at the storage boundary.
    store
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE tasks SET status='running' WHERE id=?1",
            [task.id.to_string()],
        )
        .unwrap();
    assert!(!store
        .resolve_unknown_tool(task.id, approval.request_id)
        .await
        .unwrap());
    let unchanged = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(unchanged.status, TaskStatus::Running);
    assert_eq!(unchanged.recovery.unwrap().unresolved_tools, vec![approval]);
    assert!(!unchanged.context.iter().any(|entry| entry.role == "tool"));
}

#[test]
fn detached_queued_dispatch_cannot_execute_after_stop_commits() {
    use std::future::Future;
    use std::task::Poll;

    for auto_approved in [false, true] {
        let path = DatabasePath::new();
        let store = TaskStore::new(&path.0).unwrap();
        let stopping_store = TaskStore::new(&path.0).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let task_id = runtime.block_on(async {
            let task = store.create("Programming").await.unwrap();
            let approval = if auto_approved {
                None
            } else {
                Some(claimed(&store, task.id).await)
            };
            // Occupy the only blocking worker. Dispatch can now be polled into
            // its spawn_blocking queue and dropped before its SQL executes.
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            entered_rx.await.unwrap();
            let test_call = call();
            let mut dispatch: std::pin::Pin<Box<dyn Future<Output = ()> + '_>> =
                if let Some(approval) = approval.as_ref() {
                    Box::pin(async {
                        let _ = store.begin_approved_execution(approval).await;
                    })
                } else {
                    Box::pin(async {
                        let _ = store
                            .begin_auto_approved_execution(
                                task.id,
                                Uuid::new_v4(),
                                &test_call,
                                server(),
                            )
                            .await;
                    })
                };
            std::future::poll_fn(|cx| {
                assert!(dispatch.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(dispatch);
            // A separate runtime/connection commits stop while the original
            // detached SQL is still queued. No sleeps or timing assumptions.
            std::thread::spawn(move || {
                let stopper = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                stopper
                    .block_on(stopping_store.set_status(task.id, TaskStatus::Cancelled))
                    .unwrap();
            })
            .join()
            .unwrap();
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            task.id
        });
        // Runtime drop waits for every detached blocking dispatch to finish.
        drop(runtime);
        let conn = store.conn.lock().unwrap();
        let task = select_task_by_id(&conn, &task_id.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        let executing: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM tool_approvals WHERE task_id=?1 AND state='executing'",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(executing, 0);
        if auto_approved {
            let approvals: usize = conn
                .query_row(
                    "SELECT COUNT(*) FROM tool_approvals WHERE task_id=?1",
                    [task_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(approvals, 0);
        } else {
            assert_eq!(task.recovery.unwrap().unresolved_tools.len(), 1);
        }
    }
}

fn tool_history_entry(role: &str, content: &str) -> ContextEntry {
    ContextEntry {
        role: role.into(),
        content: content.into(),
        at: Utc::now(),
        tool_call_id: Some(call().id),
    }
}

fn tool_request_entry() -> ContextEntry {
    tool_history_entry(
        "tool_request",
        &serde_json::json!({ "name": call().name, "arguments": call().arguments }).to_string(),
    )
}

#[tokio::test]
async fn crash_after_saved_tool_result_does_not_append_a_duplicate_reply_on_acknowledgement() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let task = store.create("Programming").await.unwrap();
    store
        .append_context(task.id, tool_request_entry())
        .await
        .unwrap();
    let approval = claimed(&store, task.id).await;
    assert!(store.begin_approved_execution(&approval).await.unwrap());
    store
        .append_context(
            task.id,
            tool_history_entry("tool", "saved successful result"),
        )
        .await
        .unwrap();
    // Crash before complete_tool_approval: the outcome is conservatively unknown,
    // but acknowledging it must preserve the already valid chat transcript.
    drop(store);
    let reopened = TaskStore::new(&path.0).unwrap();
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 1);
    assert!(reopened
        .resolve_unknown_tool(task.id, approval.request_id)
        .await
        .unwrap());
    let task = reopened.get(task.id).await.unwrap().unwrap();
    let replies: Vec<_> = task
        .context
        .iter()
        .filter(|entry| entry.role == "tool")
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].content, "saved successful result");
    assert!(task.context.iter().any(|entry| entry.role == "system"
        && entry.content.contains("разрешение на повтор не выдавалось")));
    assert!(!reopened.begin_approved_execution(&approval).await.unwrap());
}

#[tokio::test]
async fn reused_provider_id_only_matches_results_after_its_latest_request() {
    for latest_result_saved in [false, true] {
        let store = TaskStore::new(":memory:").unwrap();
        let task = store.create("Programming").await.unwrap();
        for entry in [
            tool_request_entry(),
            tool_history_entry("tool", "older result"),
            tool_request_entry(),
        ] {
            store.append_context(task.id, entry).await.unwrap();
        }
        let approval = claimed(&store, task.id).await;
        assert!(store.begin_approved_execution(&approval).await.unwrap());
        if latest_result_saved {
            store
                .append_context(task.id, tool_history_entry("tool", "latest result"))
                .await
                .unwrap();
        }
        store.reconcile_startup().await.unwrap();
        assert!(store
            .resolve_unknown_tool(task.id, approval.request_id)
            .await
            .unwrap());
        let task = store.get(task.id).await.unwrap().unwrap();
        let replies: Vec<_> = task
            .context
            .iter()
            .filter(|entry| entry.role == "tool")
            .collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].content, "older result");
        if latest_result_saved {
            assert_eq!(replies[1].content, "latest result");
        } else {
            assert!(replies[1].content.contains("НЕИЗВЕСТНЫЙ ИСХОД"));
        }
    }
}

fn video_result_entries() -> Vec<ContextEntry> {
    vec![
        ContextEntry {
            role: "artifact".into(),
            content: "/temporary-test-output/render.mp4".into(),
            at: Utc::now(),
            tool_call_id: None,
        },
        ContextEntry {
            role: "assistant".into(),
            content: "Видео готово".into(),
            at: Utc::now(),
            tool_call_id: None,
        },
    ]
}

#[tokio::test]
async fn video_terminal_status_and_artifact_survive_reopen_together() {
    let path = DatabasePath::new();
    let store = TaskStore::new(&path.0).unwrap();
    let task = store.create("Video").await.unwrap();
    store
        .append_context(task.id, recovery_note("original history"))
        .await
        .unwrap();
    assert!(store
        .finish_with_context_if_still_running(task.id, TaskStatus::Done, video_result_entries())
        .await
        .unwrap());
    assert!(!store
        .finish_with_context_if_still_running(task.id, TaskStatus::Done, video_result_entries())
        .await
        .unwrap());
    drop(store);
    let reopened = TaskStore::new(&path.0).unwrap();
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 0);
    let task = reopened.get(task.id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.context.len(), 3);
    assert_eq!(task.context[0].content, "original history");
    assert_eq!(task.context[1].role, "artifact");
    assert_eq!(task.context[2].role, "assistant");
}

#[tokio::test]
async fn stopped_video_cannot_publish_late_artifact_or_answer() {
    for stopped_status in [TaskStatus::Paused, TaskStatus::Cancelled] {
        let store = TaskStore::new(":memory:").unwrap();
        let task = store.create("Video").await.unwrap();
        store
            .append_context(task.id, recovery_note("original history"))
            .await
            .unwrap();
        store.set_status(task.id, stopped_status).await.unwrap();
        assert!(!store
            .finish_with_context_if_still_running(task.id, TaskStatus::Done, video_result_entries())
            .await
            .unwrap());
        let task = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(task.status, stopped_status);
        assert_eq!(task.context.len(), 1);
        assert_eq!(task.context[0].content, "original history");
    }
}

#[tokio::test]
async fn failed_artifact_publication_does_not_persist_terminal_status() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Video").await.unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_artifact BEFORE UPDATE OF context ON tasks
         BEGIN SELECT RAISE(ABORT, 'synthetic artifact persistence failure'); END;",
        )
        .unwrap();
    assert!(store
        .finish_with_context_if_still_running(task.id, TaskStatus::Done, video_result_entries())
        .await
        .is_err());
    let unchanged = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(unchanged.status, TaskStatus::Running);
    assert!(unchanged.context.is_empty());
}

#[tokio::test]
async fn atomic_finish_cannot_hide_an_inflight_tool_or_unknown_outcome() {
    for state in ["claimed", "executing", "unknown_outcome"] {
        let store = TaskStore::new(":memory:").unwrap();
        let task = store.create("Programming").await.unwrap();
        let approval = claimed(&store, task.id).await;
        if state != "claimed" {
            assert!(store.begin_approved_execution(&approval).await.unwrap());
        }
        if state == "unknown_outcome" {
            store.reconcile_startup().await.unwrap();
            store
                .conn
                .lock()
                .unwrap()
                .execute(
                    "UPDATE tasks SET status='running' WHERE id=?1",
                    [task.id.to_string()],
                )
                .unwrap();
        }
        let before = store.get(task.id).await.unwrap().unwrap();
        assert!(!store
            .finish_with_context_if_still_running(task.id, TaskStatus::Done, video_result_entries())
            .await
            .unwrap());
        let after = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Running);
        assert_eq!(after.context.len(), before.context.len());
        assert!(store.has_unresolved_tool_approval(task.id).await.unwrap());
    }
}
