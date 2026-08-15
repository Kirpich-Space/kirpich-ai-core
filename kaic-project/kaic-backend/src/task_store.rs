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
        conn.execute_batch(SCHEMA_SQL)
            .context("не удалось создать схему Task Store")?;
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
        };

        let conn = self.conn.clone();
        let task_clone = task.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
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
                tasks.push(raw_to_task(raw)?);
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
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            select_task_by_id(&conn, &id.to_string())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }

    /// Ищет задачу, которая ждёт Reply на указанное сообщение Telegram.
    pub async fn find_by_pending_telegram_message(
        &self,
        message_id: i64,
    ) -> Result<Option<Task>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Task>> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
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
            conn.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![status.as_str(), Utc::now().to_rfc3339(), id.to_string()],
            )
            .context("не удалось обновить статус задачи")?;
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
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let changed = conn
                .execute(
                    "UPDATE tasks SET status = ?1, updated_at = ?2 \
                     WHERE id = ?3 AND status = ?4",
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

    /// Привязывает задачу к сообщению Telegram, на которое ожидается Reply.
    /// `None` снимает привязку (например, после получения ответа).
    pub async fn set_pending_telegram_message(
        &self,
        id: Uuid,
        message_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
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
    /// Читает текущий контекст, дописывает запись, сохраняет обратно —
    /// этого достаточно при одном пользователе и не самой высокой частоте
    /// обновлений; более тонкая конкурентная запись сейчас не нужна.
    pub async fn append_context(&self, id: Uuid, entry: ContextEntry) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|e| anyhow::anyhow!("TaskStore mutex poisoned: {e}"))?;
            let mut task = select_task_by_id(&conn, &id.to_string())?
                .with_context(|| format!("задача {id} не найдена"))?;
            task.context.push(entry);

            let context_yaml =
                serde_yaml::to_string(&task.context).context("не удалось сериализовать контекст")?;
            conn.execute(
                "UPDATE tasks SET context = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![context_yaml, Utc::now().to_rfc3339(), id.to_string()],
            )
            .context("не удалось сохранить контекст задачи")?;
            Ok(())
        })
        .await
        .context("паника в фоновом потоке Task Store")?
    }
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

    row.map(raw_to_task).transpose()
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

    row.map(raw_to_task).transpose()
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
    })
}

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
