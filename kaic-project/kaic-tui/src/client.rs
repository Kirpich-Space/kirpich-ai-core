//! Только локальный HTTP API. Нет повторов записей, прокси и перенаправлений.
use std::{fmt, net::IpAddr, time::Duration};

use reqwest::{header, Method, Url};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::protocol::{BackendStatus, ModelStatus, Task, TaskRecovery, TaskStatus, ToolApproval};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_INPUT: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Unauthorized,
    Conflict,
    NotFound,
    Unavailable,
    Invalid,
    Protocol,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ErrorKind,
}

impl ApiError {
    fn new(kind: ErrorKind) -> Self {
        Self { kind }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            ErrorKind::Unauthorized => "Доступ отклонён: проверьте локальный токен.",
            ErrorKind::Conflict => {
                "Состояние изменилось: обновите задачу и проверьте запрос заново."
            }
            ErrorKind::NotFound => "Задача или маршрут не найдены.",
            ErrorKind::Unavailable => "Локальный backend недоступен или не ответил вовремя.",
            ErrorKind::Invalid => "Недопустимый адрес, токен или ввод.",
            ErrorKind::Protocol => "Ответ backend не соответствует контракту или превышает лимит.",
            ErrorKind::Ambiguous => {
                "Исход операции неизвестен. Обновите состояние; автоматически повторять её нельзя."
            }
        })
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone)]
pub struct ApiClient {
    client: reqwest::Client,
    base: Url,
}

// reqwest::Client может содержать заголовки; не выводим внутреннее состояние.
impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiClient { credentials: [СКРЫТО] }")
    }
}

impl ApiClient {
    pub fn new(base: &str, token: String) -> Result<Self, ApiError> {
        let invalid = || ApiError::new(ErrorKind::Invalid);
        let mut base_url = Url::parse(base).map_err(|_| invalid())?;
        if base.trim() != base
            || base.bytes().any(|b| b.is_ascii_control())
            || base_url.scheme() != "http"
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base.split('/').nth(2).is_some_and(|a| a.contains('@'))
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || base_url.path() != "/"
        {
            return Err(invalid());
        }
        let host = base_url.host_str().ok_or_else(invalid)?;
        if host == "localhost" {
            base_url
                .set_host(Some("127.0.0.1"))
                .map_err(|_| invalid())?;
        } else if !host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        {
            return Err(invalid());
        }
        let token = token.trim();
        if token.len() < 16 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid());
        }
        let mut auth =
            header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| invalid())?;
        auth.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| ApiError::new(ErrorKind::Unavailable))?;
        Ok(Self {
            client,
            base: base_url,
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Vec<u8>, ApiError> {
        let writing = method != Method::GET;
        let url = self
            .base
            .join(path)
            .map_err(|_| ApiError::new(ErrorKind::Invalid))?;
        let mut request = self.client.request(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let transport_error = || {
            ApiError::new(if writing {
                ErrorKind::Ambiguous
            } else {
                ErrorKind::Unavailable
            })
        };
        let mut response = request.send().await.map_err(|_| transport_error())?;
        let code = response.status();
        if !code.is_success() {
            return Err(ApiError::new(match code.as_u16() {
                401 | 403 => ErrorKind::Unauthorized,
                404 => ErrorKind::NotFound,
                409 => ErrorKind::Conflict,
                400 | 413 | 422 => ErrorKind::Invalid,
                500..=599 => {
                    if writing {
                        ErrorKind::Ambiguous
                    } else {
                        ErrorKind::Unavailable
                    }
                }
                _ => ErrorKind::Protocol,
            }));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(ApiError::new(if writing {
                ErrorKind::Ambiguous
            } else {
                ErrorKind::Protocol
            }));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| transport_error())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(ApiError::new(if writing {
                    ErrorKind::Ambiguous
                } else {
                    ErrorKind::Protocol
                }));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let bytes = self.request(Method::GET, path, None).await?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::new(ErrorKind::Protocol))
    }

    pub async fn list_tasks(&self) -> Result<Vec<Task>, ApiError> {
        self.get("tasks").await
    }
    pub async fn get_task(&self, id: Uuid) -> Result<Task, ApiError> {
        self.get(&format!("tasks/{id}")).await
    }
    pub async fn list_models(&self) -> Result<Vec<ModelStatus>, ApiError> {
        self.get("models").await
    }

    pub async fn set_model_temperature(
        &self,
        model: &str,
        temperature: Option<f32>,
    ) -> Result<Vec<ModelStatus>, ApiError> {
        let model = model.trim();
        if model.is_empty()
            || model.contains('/')
            || model.contains('\\')
            || model.contains("..")
            || model.contains('\0')
            || model.bytes().any(|b| b.is_ascii_control())
        {
            return Err(ApiError::new(ErrorKind::Invalid));
        }
        if let Some(t) = temperature {
            if !t.is_finite() || !(0.0..=2.0).contains(&t) {
                return Err(ApiError::new(ErrorKind::Invalid));
            }
        }
        let mut url = self.base.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| ApiError::new(ErrorKind::Invalid))?;
            segs.pop_if_empty();
            segs.push("models");
            segs.push(model);
            segs.push("temperature");
        }
        let path = url.path().trim_start_matches('/').to_string();
        let bytes = self
            .request(
                Method::POST,
                &path,
                Some(json!({ "temperature": temperature })),
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::new(ErrorKind::Ambiguous))
    }

    pub async fn local_models(&self) -> Result<crate::protocol::LocalModels, ApiError> {
        self.get("local-models").await
    }

    pub async fn inference_backend(&self) -> Result<crate::protocol::InferenceBackend, ApiError> {
        self.get("backend").await
    }
    pub async fn status(&self) -> Result<BackendStatus, ApiError> {
        self.get("status").await
    }

    pub async fn create_task(&self, text: &str) -> Result<Task, ApiError> {
        validate_input(text)?;
        let bytes = self
            .request(
                Method::POST,
                "tasks",
                Some(json!({"text":text,"category":null,"allow_manual":false})),
            )
            .await?;
        // Запись могла пройти даже при несовместимом ответе: не предлагать повтор.
        serde_json::from_slice(&bytes).map_err(|_| ApiError::new(ErrorKind::Ambiguous))
    }

    pub async fn continue_task(&self, id: Uuid, text: &str) -> Result<(), ApiError> {
        validate_input(text)?;
        self.request(
            Method::POST,
            &format!("tasks/{id}/continue"),
            Some(json!({"message":text,"allow_manual":false})),
        )
        .await?;
        Ok(())
    }

    pub async fn pause_task(&self, id: Uuid) -> Result<(), ApiError> {
        self.request(Method::POST, &format!("tasks/{id}/pause"), None)
            .await?;
        Ok(())
    }

    pub async fn cancel_task(&self, id: Uuid) -> Result<(), ApiError> {
        self.request(Method::POST, &format!("tasks/{id}/cancel"), None)
            .await?;
        Ok(())
    }

    pub async fn decide(&self, displayed: &ToolApproval, approve: bool) -> Result<(), ApiError> {
        // Сравнивается весь снимок, который действительно был показан человеку.
        // Серверный claim(request_id) остаётся окончательной защитой от гонки GET/POST.
        let task = self.get_task(displayed.task_id).await?;
        if task.id != displayed.task_id
            || task.status != TaskStatus::WaitingForHuman
            || task.pending_tool_approval.as_ref() != Some(displayed)
        {
            return Err(ApiError::new(ErrorKind::Conflict));
        }
        self.request(
            Method::POST,
            &format!("tasks/{}/tool-decision", displayed.task_id),
            Some(json!({"request_id": displayed.request_id, "approve": approve})),
        )
        .await?;
        Ok(())
    }

    /// Подтверждение ознакомления никогда не выдаёт разрешение на исполнение.
    pub async fn acknowledge_recovery(&self, displayed: &ToolApproval) -> Result<(), ApiError> {
        let task = self.get_task(displayed.task_id).await?;
        if task.id != displayed.task_id
            || !matches!(
                task.status,
                TaskStatus::WaitingForHuman | TaskStatus::Paused | TaskStatus::Cancelled
            )
            || !task.recovery.as_ref().is_some_and(|recovery| {
                recovery.kind == "unknown_outcome"
                    && recovery
                        .unresolved_tools
                        .iter()
                        .any(|tool| tool == displayed)
            })
        {
            return Err(ApiError::new(ErrorKind::Conflict));
        }
        // CAS в backend окончательно разрешает гонку между GET и POST.
        self.request(
            Method::POST,
            &format!("tasks/{}/recovery-decision", displayed.task_id),
            Some(json!({"request_id": displayed.request_id, "acknowledge": true})),
        )
        .await?;
        Ok(())
    }

    /// Старый внешний вызов без ledger имеет отдельную квитанцию задачи,
    /// а не вымышленный снимок разрешения инструмента.
    pub async fn acknowledge_legacy_recovery(
        &self,
        task_id: Uuid,
        displayed: &TaskRecovery,
    ) -> Result<(), ApiError> {
        let request_id = displayed
            .acknowledgement_id
            .filter(|_| displayed.kind == "unknown_outcome")
            .ok_or_else(|| ApiError::new(ErrorKind::Conflict))?;
        let task = self.get_task(task_id).await?;
        if task.id != task_id
            || !matches!(
                task.status,
                TaskStatus::WaitingForHuman | TaskStatus::Paused | TaskStatus::Cancelled
            )
            || task.recovery.as_ref() != Some(displayed)
        {
            return Err(ApiError::new(ErrorKind::Conflict));
        }
        self.request(
            Method::POST,
            &format!("tasks/{task_id}/recovery-decision"),
            Some(json!({"request_id": request_id, "acknowledge": true})),
        )
        .await?;
        Ok(())
    }
}

fn validate_input(text: &str) -> Result<(), ApiError> {
    if text.trim().is_empty() || text.len() > MAX_INPUT || text.contains('\0') {
        return Err(ApiError::new(ErrorKind::Invalid));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TaskRecovery, ToolServerIdentity};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const TOKEN: &str = "private-test-token-123456";

    async fn server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut part = [0; 4096];
                    let n = socket.read(&mut part).await.unwrap();
                    assert!(n > 0 && bytes.len() + n <= 65536);
                    bytes.extend_from_slice(&part[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (base, handle)
    }

    fn approval() -> ToolApproval {
        ToolApproval {
            request_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            tool_call_id: "call-1".into(),
            tool_name: "read_file".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
            server: ToolServerIdentity {
                command: "local-mcp".into(),
                args: vec!["--stdio".into()],
                peer_name: "files".into(),
            },
        }
    }

    fn task(approval: ToolApproval) -> Task {
        Task {
            id: approval.task_id,
            category: "Programming".into(),
            status: TaskStatus::WaitingForHuman,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            context: vec![],
            pending_tool_approval: Some(approval),
            recovery: None,
        }
    }

    fn uncertain(shown: ToolApproval) -> Task {
        let mut task = task(shown.clone());
        task.pending_tool_approval = None;
        task.recovery = Some(TaskRecovery {
            kind: "unknown_outcome".into(),
            message: "Результат нужно проверить вручную".into(),
            unresolved_tools: vec![shown],
            acknowledgement_id: None,
        });
        task
    }

    #[tokio::test]
    async fn recovery_ack_refreshes_exact_snapshot_and_sends_only_acknowledgment() {
        for status in [
            TaskStatus::WaitingForHuman,
            TaskStatus::Paused,
            TaskStatus::Cancelled,
        ] {
            let shown = approval();
            let mut fresh = uncertain(shown.clone());
            fresh.status = status;
            let (base, handle) = server(vec![
                (200, serde_json::to_string(&fresh).unwrap()),
                (200, String::new()),
            ])
            .await;
            ApiClient::new(&base, TOKEN.into())
                .unwrap()
                .acknowledge_recovery(&shown)
                .await
                .unwrap();
            let requests = handle.await.unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].starts_with(&format!("GET /tasks/{} ", shown.task_id)));
            assert!(requests[1]
                .starts_with(&format!("POST /tasks/{}/recovery-decision ", shown.task_id)));
            let body: Value =
                serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(
                body,
                json!({"request_id":shown.request_id,"acknowledge":true})
            );
        }
    }

    #[tokio::test]
    async fn recovery_ack_rejects_changed_identity_resolved_or_running_task_without_post() {
        for variant in 0..14 {
            let shown = approval();
            let mut fresh = uncertain(shown.clone());
            let pending = &mut fresh.recovery.as_mut().unwrap().unresolved_tools[0];
            match variant {
                0 => pending.request_id = Uuid::new_v4(),
                1 => pending.execution_id = Uuid::new_v4(),
                2 => pending.task_id = Uuid::new_v4(),
                3 => pending.tool_call_id.push('x'),
                4 => pending.tool_name.push('x'),
                5 => pending.arguments.push(' '),
                6 => pending.server.command.push('x'),
                7 => pending.server.args.push("x".into()),
                8 => pending.server.peer_name.push('x'),
                9 => fresh.status = TaskStatus::Running,
                10 => fresh.recovery = None,
                11 => fresh.recovery.as_mut().unwrap().unresolved_tools.clear(),
                12 => fresh.recovery.as_mut().unwrap().kind = "interrupted".into(),
                _ => fresh.id = Uuid::new_v4(),
            }
            let (base, handle) = server(vec![(200, serde_json::to_string(&fresh).unwrap())]).await;
            let result = ApiClient::new(&base, TOKEN.into())
                .unwrap()
                .acknowledge_recovery(&shown)
                .await;
            assert_eq!(result.unwrap_err().kind, ErrorKind::Conflict);
            assert_eq!(handle.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn recovery_ack_race_returns_conflict_and_never_retries() {
        let shown = approval();
        let (base, handle) = server(vec![
            (
                200,
                serde_json::to_string(&uncertain(shown.clone())).unwrap(),
            ),
            (409, String::new()),
        ])
        .await;
        let result = ApiClient::new(&base, TOKEN.into())
            .unwrap()
            .acknowledge_recovery(&shown)
            .await;
        assert_eq!(result.unwrap_err().kind, ErrorKind::Conflict);
        assert_eq!(handle.await.unwrap().len(), 2);
    }

    #[test]
    fn urls_and_credentials_fail_closed() {
        for base in [
            "https://localhost/",
            "http://example.org/",
            "http://192.168.1.2/",
            "http://localhost/path",
            "http://user@localhost/",
            "http://@localhost/",
            "http://localhost/?x=1",
            "http://localhost/#fragment",
        ] {
            assert_eq!(
                ApiClient::new(base, TOKEN.into()).unwrap_err().kind,
                ErrorKind::Invalid
            );
        }
        for base in [
            "http://127.0.0.1:4545",
            "http://[::1]:4545/",
            "http://localhost:4545/",
        ] {
            let client = ApiClient::new(base, TOKEN.into()).unwrap();
            assert!(!format!("{client:?}").contains(TOKEN));
        }
        assert_eq!(
            ApiClient::new("http://localhost", "short".into())
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
        assert_eq!(
            ApiClient::new("http://localhost", format!("{TOKEN}\r\nInjected: yes"))
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
    }

    #[test]
    fn input_limits_preserve_valid_unicode() {
        assert!(validate_input("Привет\nмир 🧱").is_ok());
        for text in [
            String::new(),
            " \n".into(),
            "a\0b".into(),
            "а".repeat(MAX_INPUT),
        ] {
            assert_eq!(validate_input(&text).unwrap_err().kind, ErrorKind::Invalid);
        }
    }

    #[tokio::test]
    async fn decisions_refresh_then_send_exact_identity_and_explicit_choice() {
        for approve in [true, false] {
            let shown = approval();
            let (base, handle) = server(vec![
                (200, serde_json::to_string(&task(shown.clone())).unwrap()),
                (200, String::new()),
            ])
            .await;
            let client = ApiClient::new(&base, TOKEN.into()).unwrap();
            client.decide(&shown, approve).await.unwrap();
            let requests = handle.await.unwrap();
            assert!(requests[0].starts_with(&format!("GET /tasks/{} ", shown.task_id)));
            assert!(
                requests[1].starts_with(&format!("POST /tasks/{}/tool-decision ", shown.task_id))
            );
            let body: Value =
                serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(
                body,
                json!({"request_id":shown.request_id,"approve":approve})
            );
            assert!(requests[0]
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {TOKEN}")));
        }
    }

    #[tokio::test]
    async fn every_identity_field_and_cancelled_or_missing_request_prevent_post() {
        for change in 0..10 {
            let shown = approval();
            let mut fresh = task(shown.clone());
            let pending = fresh.pending_tool_approval.as_mut().unwrap();
            match change {
                0 => pending.request_id = Uuid::new_v4(),
                1 => pending.execution_id = Uuid::new_v4(),
                2 => pending.task_id = Uuid::new_v4(),
                3 => pending.tool_call_id.push('x'),
                4 => pending.tool_name.push('x'),
                5 => pending.arguments.push(' '),
                6 => pending.server.command.push('x'),
                7 => pending.server.args.push("x".into()),
                8 => pending.server.peer_name.push('x'),
                _ => fresh.status = TaskStatus::Cancelled,
            }
            let (base, handle) = server(vec![(200, serde_json::to_string(&fresh).unwrap())]).await;
            assert_eq!(
                ApiClient::new(&base, TOKEN.into())
                    .unwrap()
                    .decide(&shown, true)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
            assert_eq!(handle.await.unwrap().len(), 1);
        }
        let shown = approval();
        let mut fresh = task(shown.clone());
        fresh.pending_tool_approval = None;
        let (base, handle) = server(vec![(200, serde_json::to_string(&fresh).unwrap())]).await;
        assert_eq!(
            ApiClient::new(&base, TOKEN.into())
                .unwrap()
                .decide(&shown, false)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(handle.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_and_duplicate_responses_are_distinct_without_body_leak() {
        let shown = approval();
        for (status, expected) in [(401, ErrorKind::Unauthorized), (409, ErrorKind::Conflict)] {
            let (base, handle) = server(vec![
                (200, serde_json::to_string(&task(shown.clone())).unwrap()),
                (status, TOKEN.into()),
            ])
            .await;
            let error = ApiClient::new(&base, TOKEN.into())
                .unwrap()
                .decide(&shown, true)
                .await
                .unwrap_err();
            assert_eq!(error.kind, expected);
            assert!(!format!("{error:?} {error}").contains(TOKEN));
            assert_eq!(handle.await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn connection_failure_and_malformed_json_are_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let client = ApiClient::new(&base, TOKEN.into()).unwrap();
        assert_eq!(
            client.list_tasks().await.unwrap_err().kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            client.create_task("hello").await.unwrap_err().kind,
            ErrorKind::Ambiguous
        );
        let (base, handle) = server(vec![(200, "invalid-json".into())]).await;
        assert_eq!(
            ApiClient::new(&base, TOKEN.into())
                .unwrap()
                .list_tasks()
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Protocol
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_responses_are_bounded() {
        for (code, body) in [(302, String::new()), (200, "x".repeat(MAX_RESPONSE + 1))] {
            let (base, handle) = server(vec![(code, body)]).await;
            assert_eq!(
                ApiClient::new(&base, TOKEN.into())
                    .unwrap()
                    .list_tasks()
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Protocol
            );
            // Большое тело клиент вправе закрыть до полной передачи.
            let _ = handle.await;
        }
    }
}
