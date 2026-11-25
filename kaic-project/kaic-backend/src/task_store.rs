//! Task Store — персистентное хранилище задач.
//!
//! Это то, что делает Human-in-the-Loop через Telegram возможным: агент
//! может работать часами, затем остановиться и ждать решения человека,
//! а после ответа — продолжить с того же места, а не начать заново.
//!
//! Задача хранит не только статус, но и накопленный контекст выполнения
//! (историю сообщений/шагов). Именно поэтому это SQLite-таблица на диске,
//! а не структура в памяти — она обязана пережить перезапуск программы
//! и перезагрузку компьютера.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Состояние задачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Агент сейчас работает над задачей.
    Running,
    /// Агент закончил этап и ждёт решения человека (написал в Telegram).
    WaitingForHuman,
    /// Задача приостановлена (не ждёт ответа прямо сейчас, но не завершена).
    Paused,
    /// Задача полностью завершена.
    Done,
    /// Задача завершилась ошибкой.
    Failed,
    /// Задача отменена явно пользователем (не ошибка и не естественное завершение).
    Cancelled,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::WaitingForHuman => "waiting_for_human",
            TaskStatus::Paused => "paused",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(TaskStatus::Running),
            "waiting_for_human" => Ok(TaskStatus::WaitingForHuman),
            "paused" => Ok(TaskStatus::Paused),
            "done" => Ok(TaskStatus::Done),
            "failed" => Ok(TaskStatus::Failed),
            "cancelled" => Ok(TaskStatus::Cancelled),
            other => anyhow::bail!("неизвестный статус задачи в базе: '{other}'"),
        }
    }
}

/// Один накопленный элемент контекста задачи — реплика, результат шага
/// или заметка агента. Свободная форма (`role` — просто строка, не enum),
/// потому что это внутренняя история задачи, а не прямой запрос к модели:
/// формирование `model_backend::Message` из этой истории — забота Agent'а.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    pub role: String,
    pub content: String,
    pub at: DateTime<Utc>,

    /// Идентификатор вызова инструмента, на который отвечает эта запись.
    /// Заполнен только у роли `tool`.
    ///
    /// Миграция здесь НЕ трогает схему SQLite, и это не упущение: контекст
    /// лежит в колонке `context` одним YAML-блобом, а не строками таблицы,
    /// поэтому новое поле — изменение формата сериализации, а не DDL.
    /// `serde(default)` делает старые блобы читаемыми: в них ключа нет, и
    /// разбор даёт `None`. `skip_serializing_if` не даёт ключу засорять
    /// записи, которым он не нужен, — то есть подавляющему большинству.
    ///
    /// Что было бы без `default`: `serde_yaml` отверг бы КАЖДУЮ задачу,
    /// созданную до этой правки, и Task Store перестал бы читаться целиком.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Неизменяемая идентичность MCP-сервера при запросе подтверждения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolServerIdentity {
    pub command: String,
    pub args: Vec<String>,
    pub peer_name: String,
}

/// Серверный идентификатор разрешения не связан с идентификатором модели.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolApproval {
    pub request_id: Uuid,
    pub task_id: Uuid,
    pub execution_id: Uuid,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: String,
    pub server: ToolServerIdentity,
}

impl ToolApproval {
    pub fn call(&self) -> crate::model_backend::ToolCall {
        crate::model_backend::ToolCall {
            id: self.tool_call_id.clone(),
            name: self.tool_name.clone(),
            arguments: self.arguments.clone(),
        }
    }
}

/// Задача целиком: статус, накопленный контекст, привязка к Telegram.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: Uuid,
    /// Категория из Router'а (см. router.rs), например "Programming".
    pub category: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub context: Vec<ContextEntry>,
    /// ID сообщения в Telegram, на которое ждём Reply, если статус —
    /// `WaitingForHuman`. Позволяет Telegram Bridge связать входящий
    /// Reply именно с этой задачей.
    pub pending_telegram_message_id: Option<i64>,
    pub pending_tool_approval: Option<ToolApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<TaskRecovery>,
}

/// Сохранённое объяснение остановки и снимки действий с неизвестным исходом.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRecovery {
    pub kind: String,
    pub message: String,
    pub unresolved_tools: Vec<ToolApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledgement_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OrphanRequest {
    context_index: usize,
    tool_call_id: String,
    name: String,
    arguments: String,
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id                          TEXT PRIMARY KEY,
    category                    TEXT NOT NULL,
    status                      TEXT NOT NULL,
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    context                     TEXT NOT NULL,
    pending_telegram_message_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tasks_pending_telegram
    ON tasks (pending_telegram_message_id);
CREATE TABLE IF NOT EXISTS tool_approvals (
    request_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    state TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_one_pending_tool_approval
    ON tool_approvals(task_id) WHERE state IN ('pending', 'claimed');
CREATE TABLE IF NOT EXISTS task_recovery (
    task_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    acknowledgement_id TEXT,
    orphan_requests TEXT NOT NULL DEFAULT '[]'
);
";

/// Персистентное хранилище задач поверх SQLite.
///
/// Все операции асинхронные: rusqlite сам по себе блокирующий, поэтому
/// каждый вызов уходит в `spawn_blocking`, чтобы не занимать поток Tokio.
pub struct TaskStore {
    conn: Arc<Mutex<Connection>>,
}

impl TaskStore {
    /// Открывает (или создаёт) базу задач по указанному пути.
    /// `":memory:"` — временная база в памяти, удобно для тестов.
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(db_path).context("не удалось открыть базу Task Store")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA_SQL)
            .context("не удалось создать схему Task Store")?;
        migrate_recovery_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Создаёт новую задачу в статусе `Running` с пустым контекстом.
    pub async fn create(&self, category: &str) -> Result<Task> {
        let task = Task {
            id: Uuid::new_v4(),
            category: category.to_string(),
            status: TaskStatus::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            context: Vec::new(),
            pending_telegram_message_id: None,
            pending_tool_approval: None,
            recovery: None,
        };

        let conn = self.conn.clone();
        let task_clone = task.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            insert_task(&conn, &task_clone)
        })
        .await
        .context("паника в фоновом потоке Task Store")??;

        Ok(task)
    }

    /// Возвращает все задачи, от недавно обновлённых к старым.
    /// Используется Control Center API для отображения списка задач.
    pub async fn list(&self) -> Result<Vec<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<Task>> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, category, status, created_at, updated_at, context, pending_telegram_message_id
                     FROM tasks ORDER BY updated_at DESC",
                )
                .context("не удалось подготовить запрос списка задач")?;
            let rows = stmt
                .query_map([], row_to_raw)
                .context("не удалось выполнить запрос списка задач")?;

            let mut tasks = Vec::new();
            for row in rows {
                let raw = row.context("ошибка чтения строки задачи")?;
                let mut task = raw_to_task(raw)?;
                task.pending_tool_approval = select_pending_approval(&conn, task.id)?;
                task.recovery = select_recovery(&conn, task.id)?;
                tasks.push(task);
            }
            Ok(tasks)
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Возвращает задачу по ID, если она существует.
    pub async fn get(&self, id: Uuid) -> Result<Option<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Task>> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            select_task_by_id(&conn, &id.to_string())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Вызывается до запуска HTTP/executor: прежних исполнителей после рестарта нет.
    /// Ничего не возобновляет и не меняет неизменяемые снимки запросов.
    pub async fn reconcile_startup(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let ids = {
                let mut stmt = tx.prepare("SELECT id FROM tasks")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut recovered = 0;
            for id in &ids {
                let id = Uuid::parse_str(id)?;
                let mut changed = mark_unknown_in_transaction(&tx, id,
                    "Backend перезапущен во время вызова инструмента. Исход неизвестен; проверьте внешний сервис и подтвердите получение информации. Повтор старого запроса запрещён.")?;
                let mut task = select_task_by_id(&tx, &id.to_string())?.context("задача отсутствует")?;
                let orphans = uncovered_requests(&tx, &task)?;
                let existing = recovery_receipt(&tx, id)?;
                if !orphans.is_empty()
                    && existing.as_ref().map(|(_, saved)| saved) != Some(&orphans)
                {
                    let receipt = Uuid::new_v4();
                    let message = format!(
                        "Найдены запросы инструмента без долговечной записи исполнения (legacy/прерванная запись): {}. Внешний эффект мог произойти; отсутствие ledger не доказывает отсутствие исполнения. Проверьте внешний сервис и подтвердите receipt {receipt}. Он не разрешает повтор.",
                        serde_json::to_string(&orphans)?);
                    task.context.push(recovery_note(&message));
                    let status = if task.status == TaskStatus::Running { TaskStatus::WaitingForHuman } else { task.status };
                    tx.execute("UPDATE tasks SET status=?1,context=?2,updated_at=?3 WHERE id=?4",
                        rusqlite::params![status.as_str(), serde_yaml::to_string(&task.context)?, Utc::now().to_rfc3339(), id.to_string()])?;
                    tx.execute("INSERT INTO task_recovery(task_id,kind,message,acknowledgement_id,orphan_requests) VALUES (?1,'unknown_outcome',?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET kind=excluded.kind,message=excluded.message,acknowledgement_id=excluded.acknowledgement_id,orphan_requests=excluded.orphan_requests",
                        rusqlite::params![id.to_string(), message, receipt.to_string(), serde_json::to_string(&orphans)?])?;
                    changed = true;
                } else if task.status == TaskStatus::Running {
                    let has_unknown = task.recovery.as_ref().is_some_and(|r| r.kind == "unknown_outcome");
                    let message = "Backend перезапущен: прежнего исполнителя больше нет. Задача остановлена; автоматическое возобновление не выполнялось.";
                    task.context.push(recovery_note(message));
                    let status = if task.pending_tool_approval.is_some() || has_unknown { "waiting_for_human" } else { "paused" };
                    tx.execute("UPDATE tasks SET status=?1,context=?2,updated_at=?3 WHERE id=?4",
                        rusqlite::params![status, serde_yaml::to_string(&task.context)?, Utc::now().to_rfc3339(), id.to_string()])?;
                    if !has_unknown {
                        tx.execute("INSERT INTO task_recovery(task_id,kind,message) VALUES (?1,'interrupted',?2) ON CONFLICT(task_id) DO UPDATE SET kind=excluded.kind,message=excluded.message",
                            rusqlite::params![id.to_string(), message])?;
                    }
                    changed = true;
                }
                recovered += usize::from(changed);
            }
            tx.commit()?;
            Ok(recovered)
        }).await.context("паника Task Store")?
    }

    /// Неопределённый внешний эффект нельзя трактовать как завершение или отказ.
    pub async fn mark_inflight_tools_unknown(&self, task_id: Uuid, reason: &str) -> Result<()> {
        let conn = self.conn.clone();
        let reason = reason.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            mark_unknown_in_transaction(&tx, task_id, &reason)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .context("паника Task Store")?
    }

    /// Подтверждение человеком закрывает неопределённость, но НЕ даёт права
    /// повторно исполнить request_id. Намеренный повтор требует нового запроса.
    pub async fn resolve_unknown_tool(&self, task_id: Uuid, request_id: Uuid) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let snapshot: Option<String> = tx.query_row(
                "SELECT snapshot FROM tool_approvals WHERE task_id=?1 AND request_id=?2 AND state='unknown_outcome' AND EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status!='running')",
                rusqlite::params![task_id.to_string(), request_id.to_string()], |r| r.get(0)).optional()?;
            let Some(snapshot) = snapshot else {
                let acknowledged = acknowledge_legacy_recovery(&tx, task_id, request_id)?;
                tx.commit()?;
                return Ok(acknowledged);
            };
            let approval: ToolApproval = serde_json::from_str(&snapshot)?;
            let changed = tx.execute("UPDATE tool_approvals SET state='acknowledged' WHERE task_id=?1 AND request_id=?2 AND state='unknown_outcome' AND EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status!='running')",
                rusqlite::params![task_id.to_string(), request_id.to_string()])?;
            if changed != 1 { return Ok(false); }
            let mut task = select_task_by_id(&tx, &task_id.to_string())?.context("задача отсутствует")?;
            // Результат мог сохраниться до crash, а отметка completed — нет.
            // Provider ID может повторяться: сверяем также имя и точные аргументы,
            // а ответ ищем только до следующего запроса истории.
            let last_request = task.context.iter().rposition(|entry|
                entry.role == "tool_request"
                    && entry.tool_call_id.as_deref() == Some(approval.tool_call_id.as_str())
                    && serde_json::from_str::<serde_json::Value>(&entry.content).is_ok_and(|value|
                        value["name"].as_str() == Some(approval.tool_name.as_str())
                            && value["arguments"].as_str() == Some(approval.arguments.as_str())));
            let has_result = last_request.is_some_and(|index| task.context[index + 1..].iter()
                .take_while(|entry| entry.role != "tool_request")
                .any(|entry| entry.role == "tool" && entry.tool_call_id.as_deref() == Some(approval.tool_call_id.as_str())));
            if !has_result {
                let insertion = last_request.map_or(task.context.len(), |index| index + 1);
                task.context.insert(insertion, ContextEntry { role: "tool".into(),
                    content: format!("НЕИЗВЕСТНЫЙ ИСХОД ПОДТВЕРЖДЁН ЧЕЛОВЕКОМ: '{}' (request_id={request_id}). Результат не установлен. Старый запрос больше не исполним; повтор допускается только новым запросом с новым подтверждением.", approval.tool_name),
                    at: Utc::now(), tool_call_id: Some(approval.tool_call_id) });
                if let Some((_, mut requests)) = recovery_receipt(&tx, task_id)? {
                    for request in &mut requests {
                        if request.context_index >= insertion { request.context_index += 1; }
                    }
                    tx.execute("UPDATE task_recovery SET orphan_requests=?1 WHERE task_id=?2",
                        rusqlite::params![serde_json::to_string(&requests)?, task_id.to_string()])?;
                }
            }
            task.context.push(recovery_note(&format!("Человек подтвердил неизвестный исход request_id={request_id}; разрешение на повтор не выдавалось.")));
            let unresolved = select_unknown_approvals(&tx, task_id)?;
            let pending_receipt = recovery_receipt(&tx, task_id)?.is_some_and(|(id, _)| id.is_some());
            let status = if task.status == TaskStatus::Cancelled { "cancelled" } else if task.status == TaskStatus::Paused || (unresolved.is_empty() && !pending_receipt) { "paused" } else { "waiting_for_human" };
            tx.execute("UPDATE tasks SET status=?1,context=?2,updated_at=?3 WHERE id=?4",
                rusqlite::params![status, serde_yaml::to_string(&task.context)?, Utc::now().to_rfc3339(), task_id.to_string()])?;
            if unresolved.is_empty() {
                clear_resolved_recovery(&tx, task_id)?;
            }
            tx.commit()?;
            Ok(true)
        }).await.context("паника Task Store")?
    }

    /// Даже инструменты из allowlist получают долговечную идентичность до dispatch.
    pub async fn begin_auto_approved_execution(
        &self,
        task_id: Uuid,
        execution_id: Uuid,
        call: &crate::model_backend::ToolCall,
        server: ToolServerIdentity,
    ) -> Result<Option<ToolApproval>> {
        let approval = ToolApproval {
            request_id: Uuid::new_v4(),
            task_id,
            execution_id,
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            server,
        };
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<ToolApproval>> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let allowed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status='running' AND NOT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id=?1 AND state IN ('pending','claimed','executing','unknown_outcome')) AND NOT EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NOT NULL))",
                [task_id.to_string()], |r| r.get(0))?;
            if !allowed { return Ok(None); }
            tx.execute("INSERT INTO tool_approvals(request_id,task_id,snapshot,state) VALUES (?1,?2,?3,'executing')",
                rusqlite::params![approval.request_id.to_string(), task_id.to_string(), serde_json::to_string(&approval)?])?;
            tx.commit()?;
            Ok(Some(approval))
        }).await.context("паника Task Store")?
    }

    /// Ищет задачу, которая ждёт Reply на указанное сообщение Telegram.
    pub async fn find_by_pending_telegram_message(&self, message_id: i64) -> Result<Option<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Task>> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            select_task_by_pending_message(&conn, message_id)
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Меняет статус задачи.
    pub async fn set_status(&self, id: Uuid, status: TaskStatus) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            if status == TaskStatus::Running {
                let unresolved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id = ?1 AND state IN ('pending','claimed','executing','unknown_outcome')) OR EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NOT NULL)",
                    [id.to_string()], |r| r.get(0))?;
                anyhow::ensure!(!unresolved, "сначала разрешите запрос инструмента или подтвердите неизвестный исход; continue не разрешает повторное исполнение");
                clear_resolved_recovery(&tx, id)?;
            }
            if matches!(status, TaskStatus::Cancelled | TaskStatus::Paused) {
                tx.execute("UPDATE tool_approvals SET state = 'invalidated' WHERE task_id = ?1 AND state = 'pending'", [id.to_string()])?;
            }
            tx.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![status.as_str(), Utc::now().to_rfc3339(), id.to_string()],
            )
            .context("не удалось обновить статус задачи")?;
            if matches!(status, TaskStatus::Cancelled | TaskStatus::Paused) {
                mark_unknown_in_transaction(&tx, id, "Исполнение инструмента прервано командой остановки; внешний эффект мог произойти.")?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Записывает итоговый статус — но ТОЛЬКО если задача всё ещё выполняется.
    ///
    /// Возвращает `false`, если статус за время работы сменил кто-то другой;
    /// тогда запись не делается вовсе.
    ///
    /// Зачем отдельный метод, а не проверка внутри `set_status`: у записи
    /// статуса два разных вида вызывающих, и запрет им нужен разный.
    ///
    /// * **Команда человека** (`cancel`, `pause`, `continue`) обязана
    ///   применяться безусловно — на то она и команда. Она продолжает ходить
    ///   через `set_status`.
    /// * **Итог фоновой работы** (генерация досчитала, рендер собрался,
    ///   пайплайн упал) имеет право записаться, только если задача осталась
    ///   в том состоянии, в котором фон её оставил.
    ///
    /// Без этого различия отменённая задача через несколько минут молча
    /// возвращалась в `Done`: `POST /cancel` ставил `Cancelled`, а
    /// досчитавшая генерация затирала его сверху. Задержка равнялась просто
    /// длительности той генерации — таймера, который бы «возвращал» задачу,
    /// в системе нет и не было.
    ///
    /// Условие живёт в самом `UPDATE ... WHERE status = 'running'`, а не в
    /// паре «прочитать, потом записать»: между чтением и записью отмена
    /// успела бы проскочить, и дефект воспроизвёлся бы в более узком окне
    /// вместо того, чтобы исчезнуть.
    pub async fn finish_if_still_running(&self, id: Uuid, status: TaskStatus) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let changed = conn
                .execute(
                    "UPDATE tasks SET status = ?1, updated_at = ?2 \
                     WHERE id = ?3 AND status = ?4 AND NOT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id = ?3 AND state IN ('claimed','executing','unknown_outcome')) AND NOT EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?3 AND acknowledgement_id IS NOT NULL)",
                    rusqlite::params![
                        status.as_str(),
                        Utc::now().to_rfc3339(),
                        id.to_string(),
                        TaskStatus::Running.as_str(),
                    ],
                )
                .context("не удалось обновить статус задачи")?;
            Ok(changed == 1)
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Итог и его артефакты публикуются одним commit. После crash нельзя
    /// увидеть Done без результата, а выигравшая отмена запрещает публикацию.
    pub async fn finish_with_context_if_still_running(
        &self,
        id: Uuid,
        status: TaskStatus,
        entries: Vec<ContextEntry>,
    ) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let Some(mut task) = select_task_by_id(&tx, &id.to_string())? else {
                return Ok(false);
            };
            task.context.extend(entries);
            let changed = tx.execute(
                "UPDATE tasks SET status=?1,context=?2,updated_at=?3
                 WHERE id=?4 AND status='running' AND NOT EXISTS(
                    SELECT 1 FROM tool_approvals WHERE task_id=?4
                    AND state IN ('claimed','executing','unknown_outcome')) AND NOT EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?4 AND acknowledgement_id IS NOT NULL)",
                rusqlite::params![
                    status.as_str(),
                    serde_yaml::to_string(&task.context)?,
                    Utc::now().to_rfc3339(),
                    id.to_string(),
                ],
            )?;
            tx.commit()?;
            Ok(changed == 1)
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Публикует снимок и WaitingForHuman в одной транзакции.
    pub async fn park_tool_approval(
        &self,
        task_id: Uuid,
        execution_id: Uuid,
        call: &crate::model_backend::ToolCall,
        server: ToolServerIdentity,
    ) -> Result<Option<ToolApproval>> {
        let approval = ToolApproval {
            request_id: Uuid::new_v4(),
            task_id,
            execution_id,
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            server,
        };
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<ToolApproval>> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id = ?1 AND state IN ('pending','claimed','executing','unknown_outcome')) OR EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NOT NULL)",
                [task_id.to_string()], |r| r.get(0))?;
            if active { return Ok(None); }
            let changed = tx.execute("UPDATE tasks SET status = 'waiting_for_human', updated_at = ?1 WHERE id = ?2 AND status = 'running'",
                rusqlite::params![Utc::now().to_rfc3339(), task_id.to_string()])?;
            if changed != 1 { return Ok(None); }
            tx.execute("INSERT INTO tool_approvals(request_id,task_id,snapshot,state) VALUES (?1,?2,?3,'pending')",
                rusqlite::params![approval.request_id.to_string(), task_id.to_string(), serde_json::to_string(&approval)?])?;
            tx.commit()?;
            Ok(Some(approval))
        }).await.context("паника Task Store")?
    }

    /// Решение привязано к task_id и request_id. Claim одноразовый и не
    /// восстанавливается из истории или после перезапуска процесса.
    pub async fn claim_tool_approval(
        &self,
        task_id: Uuid,
        request_id: Uuid,
        approve: bool,
    ) -> Result<Option<ToolApproval>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<ToolApproval>> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let snapshot: Option<String> = tx.query_row(
                "SELECT a.snapshot FROM tool_approvals a JOIN tasks t ON t.id = a.task_id WHERE a.task_id = ?1 AND a.request_id = ?2 AND a.state = 'pending' AND t.status = 'waiting_for_human' AND NOT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id=?1 AND state='unknown_outcome') AND NOT EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NOT NULL)",
                rusqlite::params![task_id.to_string(), request_id.to_string()], |r| r.get(0)).optional()?;
            let Some(snapshot) = snapshot else { return Ok(None); };
            let approval: ToolApproval = serde_json::from_str(&snapshot)?;
            tx.execute("UPDATE tool_approvals SET state = ?1 WHERE request_id = ?2",
                rusqlite::params![if approve { "claimed" } else { "denied" }, request_id.to_string()])?;
            if !approve {
                let mut task = select_task_by_id(&tx, &task_id.to_string())?.context("задача отсутствует")?;
                task.context.push(ContextEntry { role: "tool".into(),
                    content: format!("ОТКАЗАНО ЧЕЛОВЕКОМ: вызов '{}' не был исполнен. Не повторяй его.", approval.tool_name),
                    at: Utc::now(), tool_call_id: Some(approval.tool_call_id.clone()) });
                tx.execute("UPDATE tasks SET context = ?1 WHERE id = ?2",
                    rusqlite::params![serde_yaml::to_string(&task.context)?, task_id.to_string()])?;
            }
            tx.execute("UPDATE tasks SET status = 'running', updated_at = ?1 WHERE id = ?2",
                rusqlite::params![Utc::now().to_rfc3339(), task_id.to_string()])?;
            tx.commit()?;
            Ok(Some(approval))
        }).await.context("паника Task Store")?
    }

    /// Последняя атомарная граница перед внешним эффектом. Сравнение всего
    /// snapshot исключает подмену аргументов/сервера даже при верном UUID.
    pub async fn begin_approved_execution(&self, approval: &ToolApproval) -> Result<bool> {
        let conn = self.conn.clone();
        let approval = approval.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let changed = conn.execute(
                "UPDATE tool_approvals SET state = 'executing' WHERE request_id = ?1 AND task_id = ?2 AND snapshot = ?3 AND state = 'claimed' AND EXISTS(SELECT 1 FROM tasks WHERE id = ?2 AND status = 'running') AND NOT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id=?2 AND state='unknown_outcome') AND NOT EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?2 AND acknowledgement_id IS NOT NULL)",
                rusqlite::params![approval.request_id.to_string(), approval.task_id.to_string(), serde_json::to_string(&approval)?])?;
            Ok(changed == 1)
        }).await.context("паника Task Store")?
    }

    /// Неразрешённые запросы и неизвестный исход claimed/executing блокируют
    /// обычный continue: он не является повторным разрешением инструмента.
    pub async fn has_unresolved_tool_approval(&self, task_id: Uuid) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<bool> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM tool_approvals WHERE task_id = ?1 AND state IN ('pending','claimed','executing','unknown_outcome')) OR EXISTS(SELECT 1 FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NOT NULL)", [task_id.to_string()], |r| r.get(0))?)
        }).await.context("паника Task Store")?
    }

    pub async fn complete_tool_approval(&self, request_id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            conn.execute("UPDATE tool_approvals SET state = 'completed' WHERE request_id = ?1 AND state = 'executing'", [request_id.to_string()])?;
            Ok(())
        }).await.context("паника Task Store")?
    }

    /// Привязывает задачу к сообщению Telegram, на которое ожидается Reply.
    /// `None` снимает привязку (например, после получения ответа).
    pub async fn set_pending_telegram_message(
        &self,
        id: Uuid,
        message_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            conn.execute(
                "UPDATE tasks SET pending_telegram_message_id = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![message_id, Utc::now().to_rfc3339(), id.to_string()],
            )
            .context("не удалось привязать задачу к сообщению Telegram")?;
            Ok(())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Добавляет запись в накопленный контекст задачи.
    /// Резервирует запись SQLite до чтения YAML: mutex защищает только это
    /// соединение, а другая запись контекста или recovery может прийти через
    /// другое соединение. Ошибка откатывает и контекст, и updated_at.
    pub async fn append_context(&self, id: Uuid, entry: ContextEntry) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let tx = immediate_transaction(&conn)?;
            let mut task = select_task_by_id(&tx, &id.to_string())?
                .with_context(|| format!("задача {id} не найдена"))?;
            task.context.push(entry);

            let context_yaml = serde_yaml::to_string(&task.context)
                .context("не удалось сериализовать контекст")?;
            tx.execute(
                "UPDATE tasks SET context = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![context_yaml, Utc::now().to_rfc3339(), id.to_string()],
            )
            .context("не удалось сохранить контекст задачи")?;
            tx.commit()
                .context("не удалось зафиксировать контекст задачи")?;
            Ok(())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }
}

fn immediate_transaction(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    )?)
}

/// Старые таблицы сохраняются; новые поля добавляются атомарно и только один раз.
fn migrate_recovery_schema(conn: &Connection) -> Result<()> {
    let tx = immediate_transaction(conn)?;
    let columns = {
        let mut stmt = tx.prepare("PRAGMA table_info(task_recovery)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if !columns.iter().any(|name| name == "acknowledgement_id") {
        tx.execute_batch("ALTER TABLE task_recovery ADD COLUMN acknowledgement_id TEXT;")?;
    }
    if !columns.iter().any(|name| name == "orphan_requests") {
        tx.execute_batch(
            "ALTER TABLE task_recovery ADD COLUMN orphan_requests TEXT NOT NULL DEFAULT '[]';",
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn recovery_receipt(
    conn: &Connection,
    task_id: Uuid,
) -> Result<Option<(Option<Uuid>, Vec<OrphanRequest>)>> {
    let row: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT acknowledgement_id,orphan_requests FROM task_recovery WHERE task_id=?1",
            [task_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(id, requests)| {
        Ok((
            id.map(|id| Uuid::parse_str(&id)).transpose()?,
            serde_json::from_str(&requests)?,
        ))
    })
    .transpose()
}

/// История не выдаёт полномочий. Здесь она используется только для поиска
/// возможных внешних действий, которые не покрыты долговечным ledger.
fn uncovered_requests(conn: &Connection, task: &Task) -> Result<Vec<OrphanRequest>> {
    let mut stmt = conn.prepare("SELECT snapshot FROM tool_approvals WHERE task_id=?1")?;
    let rows = stmt.query_map([task.id.to_string()], |row| row.get::<_, String>(0))?;
    let approvals = rows
        .map(|row| Ok(serde_json::from_str::<ToolApproval>(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut requests = Vec::new();
    for (context_index, entry) in task.context.iter().enumerate() {
        if entry.role != "tool_request" {
            continue;
        }
        // Повреждённая историческая запись изолируется квитанцией этой задачи,
        // а не блокирует запуск всего backend. Точный исходный текст сохраняется.
        let value: serde_json::Value = serde_json::from_str(&entry.content).unwrap_or_default();
        requests.push(OrphanRequest {
            context_index,
            tool_call_id: entry.tool_call_id.clone().unwrap_or_default(),
            name: value["name"]
                .as_str()
                .unwrap_or("повреждённый tool_request")
                .into(),
            arguments: value["arguments"].as_str().unwrap_or(&entry.content).into(),
        });
    }
    Ok(requests
        .iter()
        .filter(|request| {
            let occurrences = requests
                .iter()
                .filter(|other| {
                    other.tool_call_id == request.tool_call_id
                        && other.name == request.name
                        && other.arguments == request.arguments
                })
                .count();
            let covered = approvals
                .iter()
                .filter(|approval| {
                    approval.task_id == task.id
                        && approval.tool_call_id == request.tool_call_id
                        && approval.tool_name == request.name
                        && approval.arguments == request.arguments
                })
                .count();
            // При повторных ID нельзя произвольно приписывать единственный
            // snapshot одному из нескольких одинаковых запросов истории.
            let entry = &task.context[request.context_index];
            let valid = !request.tool_call_id.is_empty()
                && serde_json::from_str::<serde_json::Value>(&entry.content)
                    .is_ok_and(|value| value["name"].is_string() && value["arguments"].is_string());
            !valid || covered < occurrences
        })
        .cloned()
        .collect())
}

/// Квитанция с orphan_requests остаётся как tombstone после ознакомления:
/// следующий запуск не должен снова объявлять те же записи неизвестными.
fn clear_resolved_recovery(conn: &Connection, task_id: Uuid) -> Result<()> {
    conn.execute(
        "UPDATE task_recovery SET kind='acknowledged',message='' WHERE task_id=?1
         AND acknowledgement_id IS NULL AND orphan_requests!='[]'",
        [task_id.to_string()],
    )?;
    conn.execute(
        "DELETE FROM task_recovery WHERE task_id=?1 AND acknowledgement_id IS NULL
         AND orphan_requests='[]'",
        [task_id.to_string()],
    )?;
    Ok(())
}

fn acknowledge_legacy_recovery(conn: &Connection, task_id: Uuid, receipt_id: Uuid) -> Result<bool> {
    let Some((Some(current_id), requests)) = recovery_receipt(conn, task_id)? else {
        return Ok(false);
    };
    let Some(mut task) = select_task_by_id(conn, &task_id.to_string())? else {
        return Ok(false);
    };
    if current_id != receipt_id
        || task.status == TaskStatus::Running
        || uncovered_requests(conn, &task)? != requests
    {
        return Ok(false);
    }
    // Вставляем только недостающие ответы, сохраняя исходные записи истории.
    // Обратный порядок сохраняет context_index ещё не обработанных запросов.
    for request in requests.iter().rev() {
        let entry = &task.context[request.context_index];
        if entry.tool_call_id.as_ref().is_none_or(|id| id.is_empty())
            || !serde_json::from_str::<serde_json::Value>(&entry.content)
                .is_ok_and(|value| value["name"].is_string() && value["arguments"].is_string())
        {
            // context_to_messages уже отображает такую запись как system note.
            continue;
        }
        let following = &task.context[request.context_index + 1..];
        let has_result = following
            .iter()
            .take_while(|entry| entry.role != "tool_request")
            .any(|entry| {
                entry.role == "tool" && entry.tool_call_id.as_ref() == Some(&request.tool_call_id)
            });
        if !has_result {
            task.context.insert(request.context_index + 1, ContextEntry {
                role: "tool".into(),
                content: format!("НЕИЗВЕСТНЫЙ LEGACY-ИСХОД ПОДТВЕРЖДЁН ЧЕЛОВЕКОМ: '{}'. Результат не установлен; старый запрос не исполнялся повторно.", request.name),
                at: Utc::now(), tool_call_id: Some(request.tool_call_id.clone()),
            });
        }
    }
    let acknowledged = uncovered_requests(conn, &task)?;
    task.context.push(recovery_note(&format!(
        "Человек подтвердил legacy receipt {receipt_id}; разрешение на повтор не выдавалось."
    )));
    let unknown = !select_unknown_approvals(conn, task_id)?.is_empty();
    let status = if task.status == TaskStatus::Cancelled {
        "cancelled"
    } else if unknown {
        "waiting_for_human"
    } else {
        "paused"
    };
    conn.execute(
        "UPDATE tasks SET status=?1,context=?2,updated_at=?3 WHERE id=?4",
        rusqlite::params![
            status,
            serde_yaml::to_string(&task.context)?,
            Utc::now().to_rfc3339(),
            task_id.to_string()
        ],
    )?;
    conn.execute(
        "UPDATE task_recovery SET acknowledgement_id=NULL,orphan_requests=?1,kind=?2
         WHERE task_id=?3 AND acknowledgement_id=?4",
        rusqlite::params![
            serde_json::to_string(&acknowledged)?,
            if unknown {
                "unknown_outcome"
            } else {
                "acknowledged"
            },
            task_id.to_string(),
            receipt_id.to_string()
        ],
    )?;
    Ok(true)
}

fn recovery_note(message: &str) -> ContextEntry {
    ContextEntry {
        role: "system".into(),
        content: message.into(),
        at: Utc::now(),
        tool_call_id: None,
    }
}

/// Вызывается только внутри короткой SQLite write-транзакции.
fn mark_unknown_in_transaction(conn: &Connection, task_id: Uuid, reason: &str) -> Result<bool> {
    let changed = conn.execute("UPDATE tool_approvals SET state='unknown_outcome' WHERE task_id=?1 AND state IN ('claimed','executing')", [task_id.to_string()])?;
    if changed == 0 {
        return Ok(false);
    }
    let mut task = select_task_by_id(conn, &task_id.to_string())?.context("задача отсутствует")?;
    let status = match task.status {
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Paused => "paused",
        _ => "waiting_for_human",
    };
    task.context.push(recovery_note(reason));
    conn.execute(
        "UPDATE tasks SET status=?1,context=?2,updated_at=?3 WHERE id=?4",
        rusqlite::params![
            status,
            serde_yaml::to_string(&task.context)?,
            Utc::now().to_rfc3339(),
            task_id.to_string()
        ],
    )?;
    conn.execute("INSERT INTO task_recovery(task_id,kind,message) VALUES (?1,'unknown_outcome',?2) ON CONFLICT(task_id) DO UPDATE SET kind=excluded.kind,message=excluded.message",
        rusqlite::params![task_id.to_string(), reason])?;
    Ok(true)
}

fn select_unknown_approvals(conn: &Connection, task_id: Uuid) -> Result<Vec<ToolApproval>> {
    let mut stmt = conn.prepare("SELECT snapshot FROM tool_approvals WHERE task_id=?1 AND state='unknown_outcome' ORDER BY request_id")?;
    let rows = stmt.query_map([task_id.to_string()], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn select_recovery(conn: &Connection, task_id: Uuid) -> Result<Option<TaskRecovery>> {
    let row: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT kind,message,acknowledgement_id FROM task_recovery WHERE task_id=?1 AND kind!='acknowledged'",
            [task_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(kind, message, acknowledgement_id)| {
        Ok(TaskRecovery {
            kind,
            message,
            unresolved_tools: select_unknown_approvals(conn, task_id)?,
            acknowledgement_id: acknowledgement_id
                .map(|id| Uuid::parse_str(&id))
                .transpose()?,
        })
    })
    .transpose()
}

fn insert_task(conn: &Connection, task: &Task) -> Result<()> {
    let context_yaml =
        serde_yaml::to_string(&task.context).context("не удалось сериализовать контекст")?;
    conn.execute(
        "INSERT INTO tasks (id, category, status, created_at, updated_at, context, pending_telegram_message_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            task.id.to_string(),
            task.category,
            task.status.as_str(),
            task.created_at.to_rfc3339(),
            task.updated_at.to_rfc3339(),
            context_yaml,
            task.pending_telegram_message_id,
        ],
    )
    .context("не удалось вставить задачу в Task Store")?;
    Ok(())
}

fn select_task_by_id(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let row = conn
        .query_row(
            "SELECT id, category, status, created_at, updated_at, context, pending_telegram_message_id
             FROM tasks WHERE id = ?1",
            rusqlite::params![id],
            row_to_raw,
        )
        .optional()
        .context("ошибка при чтении задачи из Task Store")?;

    let mut task = row.map(raw_to_task).transpose()?;
    if let Some(task) = task.as_mut() {
        task.pending_tool_approval = select_pending_approval(conn, task.id)?;
        task.recovery = select_recovery(conn, task.id)?;
    }
    Ok(task)
}

fn select_task_by_pending_message(conn: &Connection, message_id: i64) -> Result<Option<Task>> {
    let row = conn
        .query_row(
            "SELECT id, category, status, created_at, updated_at, context, pending_telegram_message_id
             FROM tasks WHERE pending_telegram_message_id = ?1",
            rusqlite::params![message_id],
            row_to_raw,
        )
        .optional()
        .context("ошибка при поиске задачи по сообщению Telegram")?;

    let mut task = row.map(raw_to_task).transpose()?;
    if let Some(task) = task.as_mut() {
        task.pending_tool_approval = select_pending_approval(conn, task.id)?;
        task.recovery = select_recovery(conn, task.id)?;
    }
    Ok(task)
}

fn select_pending_approval(conn: &Connection, task_id: Uuid) -> Result<Option<ToolApproval>> {
    let snapshot: Option<String> = conn
        .query_row(
            "SELECT snapshot FROM tool_approvals WHERE task_id = ?1 AND state = 'pending'",
            [task_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    snapshot
        .map(|s| serde_json::from_str(&s).context("неверный снимок подтверждения"))
        .transpose()
}

/// Сырые строковые/примитивные поля строки — распарсить их в `Task`
/// (с обработкой ошибок через `anyhow`) удобнее отдельно от rusqlite-замыкания,
/// которое ограничено типом `rusqlite::Result`.
type RawRow = (String, String, String, String, String, String, Option<i64>);

fn row_to_raw(row: &rusqlite::Row) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn raw_to_task(raw: RawRow) -> Result<Task> {
    let (id, category, status, created_at, updated_at, context, pending_telegram_message_id) = raw;

    Ok(Task {
        id: Uuid::parse_str(&id).context("некорректный UUID в базе")?,
        category,
        status: TaskStatus::from_str(&status)?,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .context("некорректная дата created_at в базе")?
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .context("некорректная дата updated_at в базе")?
            .with_timezone(&Utc),
        context: serde_yaml::from_str(&context).context("не удалось разобрать контекст задачи")?,
        pending_telegram_message_id,
        pending_tool_approval: None,
        recovery: None,
    })
}

#[cfg(test)]
#[path = "recovery_storage_tests.rs"]
mod recovery_storage_tests;

#[cfg(test)]
#[path = "task_store_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_store() -> TaskStore {
        TaskStore::new(":memory:").unwrap()
    }

    #[tokio::test]
    async fn create_and_get_roundtrip() {
        let store = memory_store().await;
        let task = store.create("Programming").await.unwrap();

        let loaded = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, task.id);
        assert_eq!(loaded.category, "Programming");
        assert_eq!(loaded.status, TaskStatus::Running);
        assert!(loaded.context.is_empty());
    }

    #[tokio::test]
    async fn get_unknown_id_returns_none() {
        let store = memory_store().await;
        assert!(store.get(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_status_persists() {
        let store = memory_store().await;
        let task = store.create("Simple").await.unwrap();

        store
            .set_status(task.id, TaskStatus::WaitingForHuman)
            .await
            .unwrap();

        let loaded = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::WaitingForHuman);
    }

    #[tokio::test]
    async fn append_context_accumulates_entries() {
        let store = memory_store().await;
        let task = store.create("Video").await.unwrap();

        store
            .append_context(
                task.id,
                ContextEntry {
                    role: "assistant".to_string(),
                    content: "Начал монтаж".to_string(),
                    at: Utc::now(),
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();
        store
            .append_context(
                task.id,
                ContextEntry {
                    role: "user".to_string(),
                    content: "Продолжай".to_string(),
                    at: Utc::now(),
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();

        let loaded = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.context.len(), 2);
        assert_eq!(loaded.context[0].content, "Начал монтаж");
        assert_eq!(loaded.context[1].content, "Продолжай");
    }

    /// Задача, записанная ДО появления `tool_call_id`, обязана читаться.
    ///
    /// Блоб ниже — не выдумка про формат, а ровно то, что писал прежний код:
    /// три ключа и ни одного лишнего. Если `serde(default)` с поля снимут,
    /// этот тест упадёт, а без него отказ был бы куда дороже — Task Store
    /// перестал бы читаться целиком, вместе со всей историей задач.
    #[tokio::test]
    async fn tasks_written_before_the_tool_field_still_load() {
        let store = memory_store().await;
        let task = store.create("Programming").await.unwrap();

        // Подкладываем контекст в старом формате напрямую, минуя
        // append_context: он писал бы уже новый.
        let legacy = "- role: user\n  content: посчитай\n  at: 2026-08-10T10:00:00Z\n\
                      - role: assistant\n  content: готово\n  at: 2026-08-10T10:00:05Z\n";
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET context = ?1 WHERE id = ?2",
                rusqlite::params![legacy, task.id.to_string()],
            )
            .unwrap();
        }

        let loaded = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.context.len(), 2);
        assert_eq!(loaded.context[0].content, "посчитай");
        assert_eq!(loaded.context[0].tool_call_id, None);
        assert_eq!(loaded.context[1].tool_call_id, None);
    }

    /// Новая запись роли `tool` доходит до диска вместе с идентификатором.
    #[tokio::test]
    async fn a_tool_result_keeps_its_call_id_across_a_reload() {
        let store = memory_store().await;
        let task = store.create("Programming").await.unwrap();

        store
            .append_context(
                task.id,
                ContextEntry {
                    role: "tool".to_string(),
                    content: "{\"objects\": 3}".to_string(),
                    at: Utc::now(),
                    tool_call_id: Some("call_7".to_string()),
                },
            )
            .await
            .unwrap();

        let loaded = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(loaded.context[0].tool_call_id.as_deref(), Some("call_7"));
    }

    #[tokio::test]
    async fn pending_telegram_message_roundtrip() {
        let store = memory_store().await;
        let task = store.create("Research").await.unwrap();

        store
            .set_pending_telegram_message(task.id, Some(42))
            .await
            .unwrap();

        let found = store
            .find_by_pending_telegram_message(42)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, task.id);

        store
            .set_pending_telegram_message(task.id, None)
            .await
            .unwrap();
        assert!(store
            .find_by_pending_telegram_message(42)
            .await
            .unwrap()
            .is_none());
    }
}

#[cfg(test)]
#[path = "recovery_legacy_tests.rs"]
mod recovery_legacy_tests;
