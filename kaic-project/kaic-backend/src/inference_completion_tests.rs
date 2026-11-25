use super::*;

struct DatabaseDirectory(std::path::PathBuf);

impl DatabaseDirectory {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("kaic-inference-completion-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn database(&self) -> std::path::PathBuf {
        self.0.join("tasks.db")
    }
}

impl Drop for DatabaseDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn done_and_assistant_response_are_published_in_one_database_update() {
    let directory = DatabaseDirectory::new();
    let store = Arc::new(TaskStore::new(directory.database()).unwrap());
    let task = store.create("Simple").await.unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.context.is_empty());

    // SQLite observes NEW.context at the exact UPDATE that publishes Done.
    // A status-first implementation deterministically aborts here; delaying
    // the assertion or polling could instead hide the incomplete response.
    rusqlite::Connection::open(directory.database())
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER require_response_before_done
             BEFORE UPDATE OF status ON tasks
             WHEN NEW.status = 'done'
                  AND instr(NEW.context, 'role: assistant') = 0
             BEGIN
                 SELECT RAISE(ABORT, 'status visible without response');
             END;",
        )
        .unwrap();

    record_outcome(&store, task.id, TaskStatus::Done, "answer".into()).await;

    let completed = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(completed.status, TaskStatus::Done);
    assert_eq!(completed.context.len(), 1);
    assert_eq!(completed.context[0].role, "assistant");
    assert_eq!(completed.context[0].content, "answer");
    assert!(completed.context[0].tool_call_id.is_none());
}

#[tokio::test]
async fn cancelled_task_keeps_its_status_and_records_late_response_as_a_system_note() {
    let directory = DatabaseDirectory::new();
    let store = Arc::new(TaskStore::new(directory.database()).unwrap());
    let task = store.create("Simple").await.unwrap();
    store
        .set_status(task.id, TaskStatus::Cancelled)
        .await
        .unwrap();

    record_outcome(&store, task.id, TaskStatus::Done, "late answer".into()).await;

    let cancelled = store.get(task.id).await.unwrap().unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(cancelled.context.len(), 1);
    assert_eq!(cancelled.context[0].role, "system");
    assert!(cancelled.context[0].content.contains("late answer"));
    assert!(cancelled
        .context
        .iter()
        .all(|entry| entry.role != "assistant"));
}
