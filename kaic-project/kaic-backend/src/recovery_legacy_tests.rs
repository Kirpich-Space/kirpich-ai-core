use super::*;

fn request(arguments: &str) -> ContextEntry {
    ContextEntry {
        role: "tool_request".into(),
        content: serde_json::json!({"name":"write", "arguments":arguments}).to_string(),
        at: Utc::now(),
        tool_call_id: Some("reused-id".into()),
    }
}

fn call(arguments: &str) -> crate::model_backend::ToolCall {
    crate::model_backend::ToolCall {
        id: "reused-id".into(),
        name: "write".into(),
        arguments: arguments.into(),
    }
}

fn server() -> ToolServerIdentity {
    ToolServerIdentity {
        command: "test-server".into(),
        args: vec![],
        peer_name: "test".into(),
    }
}

#[test]
fn old_and_partially_migrated_recovery_tables_preserve_data_on_repeated_open() {
    for extra_column in [
        "",
        ", acknowledgement_id TEXT",
        ", orphan_requests TEXT NOT NULL DEFAULT '[]'",
    ] {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE task_recovery(task_id TEXT PRIMARY KEY,kind TEXT NOT NULL,message TEXT NOT NULL{extra_column});
             INSERT INTO task_recovery(task_id,kind,message) VALUES ('old','interrupted','original');"
        )).unwrap();
        for _ in 0..2 {
            migrate_recovery_schema(&conn).unwrap();
        }
        let row: (String, Option<String>, String) = conn.query_row(
            "SELECT message,acknowledgement_id,orphan_requests FROM task_recovery WHERE task_id='old'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(row, ("original".into(), None, "[]".into()));
    }
}

#[tokio::test]
async fn legacy_receipt_survives_reopen_and_acknowledgement_is_durable_without_replay() {
    let path = std::env::temp_dir().join(format!("kaic-legacy-{}.db", Uuid::new_v4()));
    let store = TaskStore::new(&path).unwrap();
    let task = store.create("Programming").await.unwrap();
    store
        .append_context(task.id, request("original"))
        .await
        .unwrap();
    assert_eq!(store.reconcile_startup().await.unwrap(), 1);
    let recovered = store.get(task.id).await.unwrap().unwrap();
    let receipt = recovered.recovery.unwrap().acknowledgement_id.unwrap();
    assert_eq!(recovered.status, TaskStatus::WaitingForHuman);
    assert!(store
        .set_status(task.id, TaskStatus::Running)
        .await
        .is_err());
    assert!(store.has_unresolved_tool_approval(task.id).await.unwrap());
    drop(store);

    let reopened = TaskStore::new(&path).unwrap();
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 0);
    assert_eq!(
        reopened
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .unwrap()
            .acknowledgement_id,
        Some(receipt)
    );
    assert!(!reopened
        .resolve_unknown_tool(Uuid::new_v4(), receipt)
        .await
        .unwrap());
    assert!(!reopened
        .resolve_unknown_tool(task.id, Uuid::new_v4())
        .await
        .unwrap());
    assert!(reopened
        .resolve_unknown_tool(task.id, receipt)
        .await
        .unwrap());
    assert!(!reopened
        .resolve_unknown_tool(task.id, receipt)
        .await
        .unwrap());
    let acknowledged = reopened.get(task.id).await.unwrap().unwrap();
    assert!(acknowledged.recovery.is_none());
    assert_eq!(acknowledged.context[0].content, request("original").content);
    assert_eq!(acknowledged.context[1].role, "tool");
    let count: u32 = reopened
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tool_approvals", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "ознакомление не создаёт разрешения");
    reopened
        .set_status(task.id, TaskStatus::Running)
        .await
        .unwrap();
    reopened
        .finish_if_still_running(task.id, TaskStatus::Done)
        .await
        .unwrap();
    drop(reopened);
    let reopened = TaskStore::new(&path).unwrap();
    assert_eq!(reopened.reconcile_startup().await.unwrap(), 0);
    assert!(reopened
        .get(task.id)
        .await
        .unwrap()
        .unwrap()
        .recovery
        .is_none());
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn stale_receipt_cannot_acknowledge_changed_or_additional_requests() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    store
        .append_context(task.id, request("first"))
        .await
        .unwrap();
    store.reconcile_startup().await.unwrap();
    let old = store
        .get(task.id)
        .await
        .unwrap()
        .unwrap()
        .recovery
        .unwrap()
        .acknowledgement_id
        .unwrap();
    store
        .append_context(task.id, request("second"))
        .await
        .unwrap();
    assert!(!store.resolve_unknown_tool(task.id, old).await.unwrap());
    assert_eq!(store.reconcile_startup().await.unwrap(), 1);
    let new = store
        .get(task.id)
        .await
        .unwrap()
        .unwrap()
        .recovery
        .unwrap()
        .acknowledgement_id
        .unwrap();
    assert_ne!(old, new);
    assert!(!store.resolve_unknown_tool(task.id, old).await.unwrap());
    assert!(store.resolve_unknown_tool(task.id, new).await.unwrap());
    assert_eq!(store.reconcile_startup().await.unwrap(), 0);
    let task = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(
        task.context
            .iter()
            .filter(|entry| entry.role == "tool")
            .count(),
        2
    );
}

#[tokio::test]
async fn mixed_legacy_and_ledger_unknown_require_both_acknowledgements_in_either_order() {
    for legacy_first in [false, true] {
        for legacy_history_first in [false, true] {
            let store = TaskStore::new(":memory:").unwrap();
            let task = store.create("Programming").await.unwrap();
            let history = if legacy_history_first {
                ["legacy", "modern"]
            } else {
                ["modern", "legacy"]
            };
            for arguments in history {
                store
                    .append_context(task.id, request(arguments))
                    .await
                    .unwrap();
            }
            let approval = store
                .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call("modern"), server())
                .await
                .unwrap()
                .unwrap();
            store.reconcile_startup().await.unwrap();
            let receipt = store
                .get(task.id)
                .await
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .acknowledgement_id
                .unwrap();
            let (first, second) = if legacy_first {
                (receipt, approval.request_id)
            } else {
                (approval.request_id, receipt)
            };
            assert!(store.resolve_unknown_tool(task.id, first).await.unwrap());
            assert!(store.has_unresolved_tool_approval(task.id).await.unwrap());
            assert!(store
                .set_status(task.id, TaskStatus::Running)
                .await
                .is_err());
            assert!(store.resolve_unknown_tool(task.id, second).await.unwrap());
            assert!(!store.has_unresolved_tool_approval(task.id).await.unwrap());
            assert!(store
                .get(task.id)
                .await
                .unwrap()
                .unwrap()
                .recovery
                .is_none());
            assert_eq!(store.reconcile_startup().await.unwrap(), 0);
            assert!(!store.begin_approved_execution(&approval).await.unwrap());
            let task = store.get(task.id).await.unwrap().unwrap();
            for (index, entry) in task.context.iter().enumerate() {
                if entry.role == "tool_request" {
                    assert_eq!(task.context[index + 1].role, "tool");
                    assert_eq!(task.context[index + 1].tool_call_id, entry.tool_call_id);
                }
            }
        }
    }
}

#[tokio::test]
async fn malformed_historical_requests_do_not_prevent_other_tasks_from_recovering() {
    for (content, id) in [("invalid json", Some("id")), ("{}", None)] {
        let store = TaskStore::new(":memory:").unwrap();
        let broken = store.create("Programming").await.unwrap();
        let healthy = store.create("Simple").await.unwrap();
        store
            .append_context(
                broken.id,
                ContextEntry {
                    role: "tool_request".into(),
                    content: content.into(),
                    at: Utc::now(),
                    tool_call_id: id.map(str::to_string),
                },
            )
            .await
            .unwrap();
        assert_eq!(store.reconcile_startup().await.unwrap(), 2);
        assert_eq!(
            store.get(healthy.id).await.unwrap().unwrap().status,
            TaskStatus::Paused
        );
        let receipt = store
            .get(broken.id)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .unwrap()
            .acknowledgement_id
            .unwrap();
        assert!(store
            .set_status(broken.id, TaskStatus::Running)
            .await
            .is_err());
        assert!(store
            .resolve_unknown_tool(broken.id, receipt)
            .await
            .unwrap());
        assert_eq!(store.reconcile_startup().await.unwrap(), 0);
    }
}

#[tokio::test]
async fn receipt_blocks_dispatch_and_completion_even_with_inconsistent_running_status() {
    let store = TaskStore::new(":memory:").unwrap();
    let task = store.create("Programming").await.unwrap();
    store
        .append_context(task.id, request("legacy"))
        .await
        .unwrap();
    store.reconcile_startup().await.unwrap();
    let receipt = store
        .get(task.id)
        .await
        .unwrap()
        .unwrap()
        .recovery
        .unwrap()
        .acknowledgement_id
        .unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE tasks SET status='running' WHERE id=?1",
            [task.id.to_string()],
        )
        .unwrap();
    assert!(!store.resolve_unknown_tool(task.id, receipt).await.unwrap());
    assert!(store
        .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call("new"), server())
        .await
        .unwrap()
        .is_none());
    assert!(store
        .park_tool_approval(task.id, Uuid::new_v4(), &call("new"), server())
        .await
        .unwrap()
        .is_none());
    assert!(!store
        .finish_if_still_running(task.id, TaskStatus::Done)
        .await
        .unwrap());
    assert!(!store
        .finish_with_context_if_still_running(task.id, TaskStatus::Done, vec![])
        .await
        .unwrap());
}

#[tokio::test]
async fn duplicate_provider_ids_do_not_cover_different_or_repeated_requests() {
    for latest in ["same", "different"] {
        let store = TaskStore::new(":memory:").unwrap();
        let task = store.create("Programming").await.unwrap();
        store
            .append_context(task.id, request("same"))
            .await
            .unwrap();
        let approval = store
            .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call("same"), server())
            .await
            .unwrap()
            .unwrap();
        store
            .complete_tool_approval(approval.request_id)
            .await
            .unwrap();
        store
            .append_context(task.id, request(latest))
            .await
            .unwrap();
        store.reconcile_startup().await.unwrap();
        assert!(store
            .get(task.id)
            .await
            .unwrap()
            .unwrap()
            .recovery
            .unwrap()
            .acknowledgement_id
            .is_some());
    }
}
