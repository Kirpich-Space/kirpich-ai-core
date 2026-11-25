//! Независимые проверки HTTP-границы без моделей, реального backend и новых зависимостей.
use std::{collections::VecDeque, time::Duration};

use kaic_tui::{
    client::{ApiClient, ErrorKind},
    protocol::TaskStatus,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const TOKEN: &str = "contract-test-token-123456";
const TASK_ID: &str = "11111111-2222-4333-8444-555555555555";
const AT: &str = "2026-10-18T12:00:00Z";

enum Response {
    Http(u16, String),
    Disconnect,
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    authorization: Option<String>,
}

struct Server {
    base: String,
    stop: oneshot::Sender<()>,
    handle: JoinHandle<Vec<Request>>,
}

impl Server {
    async fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (stop, mut stopped) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            let mut requests = Vec::new();
            loop {
                let accepted = tokio::select! {
                    _ = &mut stopped => break,
                    accepted = timeout(Duration::from_secs(6), listener.accept()) => accepted,
                };
                let (mut socket, _) = accepted.expect("ограниченное ожидание запроса").unwrap();
                requests.push(
                    timeout(Duration::from_secs(2), read_request(&mut socket))
                        .await
                        .expect("ограниченное чтение запроса"),
                );
                // Лишний запрос получает ответ и остаётся виден в счётчике повторов.
                match responses
                    .pop_front()
                    .unwrap_or(Response::Http(500, "{}".into()))
                {
                    Response::Disconnect => drop(socket),
                    Response::Http(status, body) => {
                        let response = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        timeout(
                            Duration::from_secs(2),
                            socket.write_all(response.as_bytes()),
                        )
                        .await
                        .expect("ограниченная запись ответа")
                        .unwrap();
                    }
                }
            }
            requests
        });
        Self { base, stop, handle }
    }

    fn client(&self) -> ApiClient {
        ApiClient::new(&self.base, TOKEN.into()).unwrap()
    }

    async fn finish(self) -> Vec<Request> {
        self.stop.send(()).unwrap();
        timeout(Duration::from_secs(7), self.handle)
            .await
            .expect("локальный сервер завершился")
            .unwrap()
    }
}

async fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 2048];
        let length = socket.read(&mut chunk).await.unwrap();
        assert!(length > 0, "соединение не оборвано до полного запроса");
        bytes.extend_from_slice(&chunk[..length]);
        assert!(
            bytes.len() <= 65536,
            "ограничение размера тестового запроса"
        );
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let body_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + body_length {
                let mut first_line = headers.lines().next().unwrap().split_whitespace();
                return Request {
                    method: first_line.next().unwrap().into(),
                    path: first_line.next().unwrap().into(),
                    body: bytes[end + 4..end + 4 + body_length].to_vec(),
                    authorization: headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                        .map(|(_, value)| value.trim().to_owned()),
                };
            }
        }
    }
}

fn task(status: &str) -> Value {
    // Форма Task из backend task_store.rs, включая неиспользуемое клиентом поле.
    json!({
        "id": TASK_ID, "category": "Programming", "status": status,
        "created_at": AT, "updated_at": AT,
        "context": [{"role": "assistant", "content": "История 🧱", "at": AT}],
        "pending_telegram_message_id": null, "pending_tool_approval": null
    })
}

#[tokio::test]
async fn local_models_reads_authenticated_catalog_with_nullable_metadata_and_rejections() {
    let catalog = json!({
        "root":"/tmp/Модели",
        "models":[
            {"id":"gpt-oss.gguf", "path":"/tmp/Модели/gpt-oss.gguf",
             "size_bytes":12345678901u64, "gguf_version":3, "tensor_count":459,
             "architecture":"gptoss", "file_type":38},
            {"id":"other.gguf", "path":"/tmp/Модели/other.gguf",
             "size_bytes":64, "gguf_version":2, "tensor_count":0,
             "architecture":null, "file_type":null}
        ],
        "rejected":[{"path":"/tmp/Модели/broken.gguf", "error":"invalid GGUF magic"}]
    });
    let server = Server::start(vec![Response::Http(200, catalog.to_string())]).await;
    let local = server.client().local_models().await.unwrap();
    assert_eq!(local.root, "/tmp/Модели");
    assert_eq!(local.models.len(), 2);
    let model = &local.models[0];
    assert_eq!(model.id, "gpt-oss.gguf");
    assert_eq!(model.path, "/tmp/Модели/gpt-oss.gguf");
    assert_eq!(model.size_bytes, 12345678901);
    assert_eq!(model.gguf_version, 3);
    assert_eq!(model.tensor_count, 459);
    assert_eq!(model.architecture.as_deref(), Some("gptoss"));
    assert_eq!(model.file_type, Some(38));
    assert!(local.models[1].architecture.is_none());
    assert!(local.models[1].file_type.is_none());
    assert_eq!(local.rejected.len(), 1);
    assert_eq!(local.rejected[0].path, "/tmp/Модели/broken.gguf");
    assert_eq!(local.rejected[0].error, "invalid GGUF magic");
    let requests = server.finish().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/local-models");
    assert!(requests[0].body.is_empty());
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[tokio::test]
async fn local_models_distinguishes_empty_catalog_from_missing_route_and_invalid_response() {
    let server = Server::start(vec![
        Response::Http(
            200,
            json!({"root":"/empty", "models":[], "rejected":[]}).to_string(),
        ),
        Response::Http(404, "{}".into()),
        Response::Http(401, "{}".into()),
        Response::Http(503, "{}".into()),
        Response::Http(200, json!({"models":[], "rejected":[]}).to_string()),
    ])
    .await;
    let client = server.client();
    let empty = client.local_models().await.unwrap();
    assert_eq!(empty.root, "/empty");
    assert!(empty.models.is_empty() && empty.rejected.is_empty());
    for expected in [
        ErrorKind::NotFound,
        ErrorKind::Unauthorized,
        ErrorKind::Unavailable,
        ErrorKind::Protocol,
    ] {
        assert_eq!(client.local_models().await.unwrap_err().kind, expected);
    }
    let requests = server.finish().await;
    assert_eq!(requests.len(), 5);
    assert!(requests
        .iter()
        .all(|r| r.method == "GET" && r.path == "/local-models"));
}

#[tokio::test]
async fn backend_shaped_tasks_models_and_scheduler_status_remain_readable() {
    // Независимые JSON-образцы соответствуют DTO control_center_api.rs, без
    // сериализации через клиентские типы: иначе одинаковая ошибка скрыла бы drift.
    let statuses = [
        "running",
        "waiting_for_human",
        "paused",
        "done",
        "failed",
        "cancelled",
    ];
    let tasks = Value::Array(statuses.into_iter().map(task).collect());
    let models = json!([
        {"model":"local-code", "vram_mb":4096, "ram_offload_mb":1024,
         "always_loaded":false, "preferred_device":"hybrid", "loaded":true,
         "last_used":AT, "context_length":8192, "max_context_length":32768,
         "temperature":0.25},
        {"model":"local-idle", "vram_mb":0, "ram_offload_mb":2048,
         "always_loaded":true, "preferred_device":"cpu", "loaded":false,
         "last_used":null, "context_length":null, "max_context_length":null,
         "temperature":null}
    ]);
    let status = json!({
        "total_model_memory_mb":16384, "used_model_memory_mb":4096,
        "loaded_models":[{"model":"local-code", "last_used":AT}],
        "running_tasks":2, "max_concurrent_tasks":3,
        "busy_models":[{"model":"local-code", "generations":2}],
        "active_operation":{"kind":"loading", "model":"local-idle",
            "reason":"Programming", "started_at":AT, "elapsed_ms":512}
    });
    let server = Server::start(vec![
        Response::Http(200, tasks.to_string()),
        Response::Http(200, models.to_string()),
        Response::Http(200, status.to_string()),
    ])
    .await;
    let client = server.client();
    let tasks = client.list_tasks().await.unwrap();
    assert_eq!(
        tasks.iter().map(|t| t.status).collect::<Vec<_>>(),
        vec![
            TaskStatus::Running,
            TaskStatus::WaitingForHuman,
            TaskStatus::Paused,
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ]
    );
    assert_eq!(tasks[0].context[0].content, "История 🧱");
    assert_eq!(tasks[0].context[0].tool_call_id, None);
    assert!(tasks.iter().all(|task| task.recovery.is_none()));
    let models = client.list_models().await.unwrap();
    assert_eq!(models[0].model, "local-code");
    assert_eq!(models[0].preferred_device, "hybrid");
    assert_eq!(models[0].context_length, Some(8192));
    assert_eq!(models[0].temperature, Some(0.25));
    assert!(models[1].last_used.is_none() && models[1].temperature.is_none());
    let status = client.status().await.unwrap();
    assert_eq!(status.running_tasks, 2);
    assert!(status.recovery_warnings.is_empty());
    assert_eq!(status.max_concurrent_tasks, 3);
    assert_eq!(status.loaded_models[0].last_used, AT);
    assert_eq!(status.busy_models[0].generations, 2);
    assert_eq!(status.active_operation.unwrap().elapsed_ms, 512);
    let requests = server.finish().await;
    assert_eq!(
        requests
            .iter()
            .map(|r| (r.method.as_str(), r.path.as_str()))
            .collect::<Vec<_>>(),
        vec![("GET", "/tasks"), ("GET", "/models"), ("GET", "/status")]
    );
}

#[tokio::test]
async fn task_writes_preserve_text_and_never_enable_manual_models_implicitly() {
    let server = Server::start(vec![
        Response::Http(200, task("running").to_string()),
        Response::Http(202, String::new()),
        Response::Http(200, String::new()),
        Response::Http(200, String::new()),
    ])
    .await;
    let client = server.client();
    let text = "  Новая задача 🧱\nс точным пробелом  ";
    let created = client.create_task(text).await.unwrap();
    assert_eq!(created.id, Uuid::parse_str(TASK_ID).unwrap());
    client
        .continue_task(created.id, "Продолжить\nбез смены модели")
        .await
        .unwrap();
    client.pause_task(created.id).await.unwrap();
    client.cancel_task(created.id).await.unwrap();
    let requests = server.finish().await;
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|request| request.method == "POST"));
    assert_eq!(requests[0].path, "/tasks");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"text":text, "category":null, "allow_manual":false})
    );
    assert_eq!(requests[1].path, format!("/tasks/{TASK_ID}/continue"));
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[1].body).unwrap(),
        json!({"message":"Продолжить\nбез смены модели", "allow_manual":false})
    );
    assert_eq!(requests[2].path, format!("/tasks/{TASK_ID}/pause"));
    assert_eq!(requests[3].path, format!("/tasks/{TASK_ID}/cancel"));
    assert!(requests[2].body.is_empty() && requests[3].body.is_empty());
}

#[tokio::test]
async fn uncertain_create_outcomes_never_retry_the_post() {
    for response in [
        Response::Http(500, "internal diagnostic".into()),
        Response::Disconnect,
        Response::Http(200, "invalid task JSON".into()),
    ] {
        let server = Server::start(vec![response]).await;
        let result = timeout(
            Duration::from_secs(6),
            server.client().create_task("Один запрос"),
        )
        .await
        .expect("HTTP-операция ограничена по времени");
        assert_eq!(result.unwrap_err().kind, ErrorKind::Ambiguous);
        let requests = server.finish().await;
        assert_eq!(
            requests.len(),
            1,
            "неопределённая запись не должна повторяться"
        );
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/tasks");
    }
}

#[tokio::test]
async fn model_temperature_update_validates_bounds_and_posts_exact_payload() {
    let updated_models = json!([
        {"model":"local-code", "vram_mb":4096, "ram_offload_mb":1024,
         "always_loaded":false, "preferred_device":"hybrid", "loaded":true,
         "last_used":AT, "context_length":8192, "max_context_length":32768,
         "temperature":0.5}
    ]);
    let reset_models = json!([
        {"model":"local-code", "vram_mb":4096, "ram_offload_mb":1024,
         "always_loaded":false, "preferred_device":"hybrid", "loaded":true,
         "last_used":AT, "context_length":8192, "max_context_length":32768,
         "temperature":null}
    ]);
    let server = Server::start(vec![
        Response::Http(200, updated_models.to_string()),
        Response::Http(200, reset_models.to_string()),
    ])
    .await;
    let client = server.client();

    for bad in [-0.1_f32, 2.5_f32, f32::NAN] {
        assert_eq!(
            client
                .set_model_temperature("local-code", Some(bad))
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
    }
    assert_eq!(
        client
            .set_model_temperature("../etc", Some(0.5))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );

    let first = client
        .set_model_temperature("local-code", Some(0.5))
        .await
        .unwrap();
    assert_eq!(first[0].temperature, Some(0.5));

    let second = client
        .set_model_temperature("local-code", None)
        .await
        .unwrap();
    assert_eq!(second[0].temperature, None);

    let requests = server.finish().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/models/local-code/temperature");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"temperature": 0.5})
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/models/local-code/temperature");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[1].body).unwrap(),
        json!({"temperature": null})
    );
}
