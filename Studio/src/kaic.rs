use crate::{
    network::{Cancel, LocalEndpoint, client, json_response},
    storage::Message,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt,
    time::{Duration, Instant},
};
#[derive(Clone, Default)]
pub struct Secret(String);
impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[скрыто]")
    }
}
#[derive(Debug, PartialEq)]
pub enum TaskPhase {
    Active,
    Waiting,
    Done,
    Failed,
    Cancelled,
}
pub fn task_phase(s: &str) -> TaskPhase {
    match s {
        "done" => TaskPhase::Done,
        "failed" => TaskPhase::Failed,
        "cancelled" => TaskPhase::Cancelled,
        "paused" | "waiting_for_human" => TaskPhase::Waiting,
        _ => TaskPhase::Active,
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub category: String,
    pub status: String,
    #[serde(default)]
    pub context: Vec<Turn>,
    #[serde(default)]
    pub pending_tool_approval: Option<Value>,
    #[serde(default)]
    pub recovery: Option<Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub at: Value,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}
impl Task {
    pub fn messages(&self) -> Vec<Message> {
        self.context
            .iter()
            .map(|t| Message {
                role: t.role.clone(),
                content: t.content.clone(),
                reasoning: String::new(),
                state: String::new(),
                error: None,
                tps: None,
                tokens: None,
                duration_secs: None,
                media: Vec::new(),
                agent_events: Vec::new(),
                subtasks: Vec::new(),
                changed_files: Vec::new(),
            })
            .collect()
    }
}
pub struct KaicClient {
    endpoint: LocalEndpoint,
    token: Secret,
    http: reqwest::Client,
}
impl KaicClient {
    pub fn new(endpoint: LocalEndpoint, token: Secret) -> Result<Self> {
        if token.is_empty() {
            bail!(
                "Нет токена KAIC. Укажите KAIC_CONTROL_TOKEN в окружении или введите токен в настройках (только в памяти)."
            );
        }
        Ok(Self {
            endpoint,
            token,
            http: client()?,
        })
    }
    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        r.bearer_auth(self.token.expose())
            .timeout(Duration::from_secs(10))
    }
    pub async fn inspect(&self) -> Result<Value> {
        let mut out = serde_json::Map::new();
        for p in ["status", "backend", "models", "local-models"] {
            let v = json_response(
                self.auth(self.http.get(self.endpoint.route(&format!("/{p}"))))
                    .send()
                    .await?,
            )
            .await?;
            out.insert(p.into(), v);
        }
        Ok(Value::Object(out))
    }
    pub async fn submit(&self, text: &str) -> Result<Task> {
        self.submit_cancellable(text, &Cancel::default()).await
    }
    pub async fn submit_cancellable(&self, text: &str, cancel: &Cancel) -> Result<Task> {
        if cancel.is_cancelled() {
            bail!("Создание задачи KAIC остановлено до отправки");
        }
        let request = async {
            let v = json_response(
                self.auth(self.http.post(self.endpoint.route("/tasks")))
                    .json(&json!({"text":text,"allow_manual":false}))
                    .send()
                    .await?,
            )
            .await?;
            serde_json::from_value(v).context("KAIC вернул несовместимый контракт задачи")
        };
        tokio::select! {biased;_=cancel.cancelled()=>bail!("Ожидание создания задачи KAIC остановлено. Backend мог принять задачу; ID не получен. Проверьте KAIC перед повторной отправкой."),r=request=>r}
    }
    pub async fn get(&self, id: &str) -> Result<Task> {
        self.get_with_timeout(id, Duration::from_secs(10)).await
    }
    async fn get_with_timeout(&self, id: &str, timeout: Duration) -> Result<Task> {
        Ok(serde_json::from_value(
            json_response(
                self.auth(self.http.get(self.endpoint.segment("/tasks/", id)))
                    .timeout(timeout)
                    .send()
                    .await?,
            )
            .await?,
        )?)
    }
    pub async fn cancel(&self, id: &str) -> Result<()> {
        let mut u = self.endpoint.segment("/tasks/", id);
        u.path_segments_mut().unwrap().push("cancel");
        let r = self
            .auth(self.http.post(u))
            .timeout(Duration::from_secs(2))
            .send()
            .await?;
        if !r.status().is_success() {
            bail!(
                "KAIC не подтвердил запрос отмены: HTTP {}",
                r.status().as_u16()
            );
        }
        Ok(())
    }
    pub async fn follow(
        &self,
        mut task: Task,
        cancel: &Cancel,
        mut emit: impl FnMut(&Task),
    ) -> Result<Task> {
        let start = Instant::now();
        loop {
            emit(&task);
            if task.pending_tool_approval.is_some() || task_phase(&task.status) != TaskPhase::Active
            {
                return Ok(task);
            }
            if start.elapsed() > Duration::from_secs(1800) {
                bail!(
                    "Мониторинг KAIC завершён по таймауту. Задача {} может продолжаться; проверьте её статус.",
                    task.id
                );
            }
            tokio::select! {
              _=cancel.cancelled()=>{self.cancel(&task.id).await?;return self.get_with_timeout(&task.id,Duration::from_secs(2)).await;},
              _=tokio::time::sleep(Duration::from_millis(800))=>{}
            }
            task = tokio::select! {_=cancel.cancelled()=>{self.cancel(&task.id).await?;return self.get_with_timeout(&task.id,Duration::from_secs(2)).await;},r=self.get(&task.id)=>r?};
        }
    }
}
