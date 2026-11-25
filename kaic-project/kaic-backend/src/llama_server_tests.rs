use super::*;
use axum::{
    body::Bytes,
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Mock {
    async fn start(override_path: &str, status: u16, body: &str, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let override_path = override_path.to_string();
        let body = body.to_string();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, bytes: Bytes| {
            let log = log.clone();
            let override_path = override_path.clone();
            let body = body.clone();
            async move {
                log.lock().unwrap().push((format!("{method} {uri}"), serde_json::from_slice(&bytes).unwrap_or(Value::Null)));
                if uri.path() == override_path {
                    tokio::time::sleep(delay).await;
                    return (StatusCode::from_u16(status).unwrap(), body).into_response();
                }
                let response = match uri.path() {
                    "/health" => json!({"status":"ok"}),
                    "/v1/models" => json!({"data":[{"id":"provider-model", "architecture":{"input_modalities":["text"],"output_modalities":["text"]},"meta":{"n_ctx_train":8192}}]}),
                    "/v1/chat/completions" => json!({"choices":[{"message":{"content":"Ответ локальной модели"}}]}),
                    _ => return StatusCode::NOT_FOUND.into_response(),
                };
                axum::Json(response).into_response()
            }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }

    fn backend(&self) -> LlamaServerBackend {
        LlamaServerBackend::with_timeouts(
            &self.base,
            None,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request() -> GenerateRequest {
    GenerateRequest {
        messages: vec![crate::model_backend::Message::new(
            crate::model_backend::Role::User,
            "Привет",
        )],
        tools: vec![],
        temperature: Some(0.2),
    }
}

#[test]
fn endpoints_are_loopback_only_without_credential_or_proxy_paths() {
    for base in [
        "https://127.0.0.1:8080",
        "http://example.org",
        "http://user:secret@localhost",
        "http://localhost/v1",
        "http://localhost/?token=secret",
        "http://localhost/#secret",
    ] {
        assert!(LlamaServerBackend::new(base, None).is_err(), "{base}");
    }
    for base in [
        "http://127.0.0.1:8080",
        "http://localhost:8080",
        "http://[::1]:8080",
    ] {
        assert!(LlamaServerBackend::new(base, None).is_ok(), "{base}");
    }
}

#[tokio::test]
async fn discovery_preserves_identity_metadata_and_lifecycle_is_explicitly_unsupported() {
    let server = Mock::start("", 200, "", Duration::ZERO).await;
    let backend = server.backend();
    let models = backend.discover_models().await.unwrap();
    assert_eq!(models[0].id, "provider-model");
    assert_eq!(models[0].meta.as_ref().unwrap()["n_ctx_train"], 8192);
    assert!(models[0].architecture.is_some());
    assert!(backend.externally_managed());
    assert!(backend.load("provider-model").await.is_err());
    assert!(backend.unload("provider-model").await.is_err());
    assert!(backend.owned_instances().await.unwrap().is_empty());
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(path, _)| path.starts_with("GET ")));
}

#[tokio::test]
async fn inference_uses_exact_model_and_existing_openai_wire_protocol() {
    let server = Mock::start("", 200, "", Duration::ZERO).await;
    let response = server
        .backend()
        .generate("provider-model", request())
        .await
        .unwrap();
    assert_eq!(response.text(), Some("Ответ локальной модели"));
    let requests = server.requests.lock().unwrap();
    let (path, body) = requests
        .iter()
        .find(|(path, _)| path.starts_with("POST "))
        .unwrap();
    assert_eq!(path, "POST /v1/chat/completions?autoload=false");
    assert_eq!(body["model"], "provider-model");
    assert_eq!(body["messages"][0]["content"], "Привет");
    assert_eq!(body["temperature"], 0.2);
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn absent_model_never_sends_an_inference_request() {
    let server = Mock::start("", 200, "", Duration::ZERO).await;
    let error = server
        .backend()
        .generate("unknown", request())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("не обслуживается"));
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(path, _)| !path.starts_with("POST ")));
}

#[tokio::test]
async fn invalid_discovery_and_router_mode_fail_without_lifecycle_calls() {
    for (path, status, body) in [
        ("/health", 503, "{}"),
        ("/health", 200, "invalid json"),
        ("/v1/models", 200, "{}"),
        ("/v1/models", 200, r#"{"data":[]}"#),
        ("/v1/models", 200, r#"{"data":[{"id":""}]}"#),
        ("/v1/models", 200, r#"{"data":[{"id":"one"},{"id":"two"}]}"#),
        (
            "/v1/models",
            200,
            r#"{"data":[{"id":"one","status":{"value":"unloaded"}}]}"#,
        ),
    ] {
        let server = Mock::start(path, status, body, Duration::ZERO).await;
        assert!(
            server.backend().discover_models().await.is_err(),
            "{path} {body}"
        );
        assert!(server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path.starts_with("GET ")));
    }
}

#[tokio::test]
async fn malformed_generation_and_http_errors_are_not_successful_answers() {
    for (status, body) in [
        (500, "{}"),
        (200, "invalid json"),
        (200, "{}"),
        (200, r#"{"choices":[]}"#),
    ] {
        let server = Mock::start("/v1/chat/completions", status, body, Duration::ZERO).await;
        assert!(server
            .backend()
            .generate("provider-model", request())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn generation_without_text_or_tool_calls_is_an_error() {
    for body in [
        r#"{"choices":[{"message":{"content":null}}]}"#,
        r#"{"choices":[{"message":{}}]}"#,
        r#"{"choices":[{"message":{"content":null,"tool_calls":[]}}]}"#,
    ] {
        let server = Mock::start("/v1/chat/completions", 200, body, Duration::ZERO).await;
        let result = server.backend().generate("provider-model", request()).await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("отсутствие текста и инструментов не означает успешный ответ"),
        };
        assert!(error
            .to_string()
            .contains("не содержит текста или вызовов инструментов"));
    }
}

#[tokio::test]
async fn explicit_empty_string_preserves_the_existing_response_contract() {
    let server = Mock::start(
        "/v1/chat/completions",
        200,
        r#"{"choices":[{"message":{"content":""}}]}"#,
        Duration::ZERO,
    )
    .await;
    let response = server
        .backend()
        .generate("provider-model", request())
        .await
        .unwrap();
    assert_eq!(response.text(), Some(""));
}

#[tokio::test]
async fn connection_failure_and_timeouts_release_callers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let backend = LlamaServerBackend::with_timeouts(
        &base,
        None,
        Duration::from_millis(30),
        Duration::from_millis(30),
    )
    .unwrap();
    assert!(backend.discover_models().await.is_err());
    for path in ["/health", "/v1/chat/completions"] {
        let server = Mock::start(path, 200, "{}", Duration::from_secs(1)).await;
        assert!(tokio::time::timeout(
            Duration::from_millis(500),
            server.backend().generate("provider-model", request())
        )
        .await
        .unwrap()
        .is_err());
    }
}

struct Registries(std::path::PathBuf);

impl Drop for Registries {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test]
async fn external_footprint_and_model_identity_must_match_registry_before_execution() {
    let server = Mock::start("", 200, "", Duration::ZERO).await;
    for resources in [
        "Local: {model_id: provider-model, vram_mb: 0, load_seconds: 0}",
        "Local: {model_id: provider-model, vram_mb: 101, load_seconds: 0}",
        "Local: {model_id: missing, vram_mb: 60, load_seconds: 0}",
        "Local: {model_id: provider-model, vram_mb: 60, load_seconds: 0}\nDuplicate: {model_id: provider-model, vram_mb: 20, load_seconds: 0}",
    ] {
        let dir = Registries(std::env::temp_dir().join(format!("kaic-llama-invalid-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&dir.0).unwrap();
        std::fs::write(dir.0.join("resources.yaml"), resources).unwrap();
        std::fs::write(dir.0.join("capabilities.yaml"), "Simple: {primary: Local}").unwrap();
        let scheduler = crate::scheduler::Scheduler::new(
            Arc::new(crate::capability_registry::CapabilityRegistry::from_file(dir.0.join("capabilities.yaml")).unwrap()),
            Arc::new(crate::resource_registry::ResourceRegistry::from_file(dir.0.join("resources.yaml")).unwrap()),
            Arc::new(server.backend()), 100,
        );
        assert!(scheduler.preload_always_loaded().await.is_err());
        assert!(scheduler.run("Simple", false, request()).await.is_err());
        assert_eq!(scheduler.running_generations(), 0);
        assert!(scheduler.snapshot().await.is_empty());
    }
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(path, _)| path.starts_with("GET ")));
}

#[tokio::test]
async fn router_and_scheduler_use_discovered_model_without_loading_or_eviction() {
    let server = Mock::start("", 200, "", Duration::ZERO).await;
    let dir = Registries(std::env::temp_dir().join(format!("kaic-llama-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&dir.0).unwrap();
    let resources = dir.0.join("resources.yaml");
    let capabilities = dir.0.join("capabilities.yaml");
    std::fs::write(&resources, "Local: {model_id: provider-model, vram_mb: 60, load_seconds: 0, always_loaded: true}\nUnavailable: {model_id: absent, vram_mb: 200, load_seconds: 0}\n").unwrap();
    std::fs::write(
        &capabilities,
        "Simple: {primary: Local}\nProgramming: {primary: Unavailable, fallback: [Local]}\n",
    )
    .unwrap();
    let registry =
        Arc::new(crate::resource_registry::ResourceRegistry::from_file(&resources).unwrap());
    let scheduler = crate::scheduler::Scheduler::new(
        Arc::new(crate::capability_registry::CapabilityRegistry::from_file(&capabilities).unwrap()),
        registry.clone(),
        Arc::new(server.backend()),
        100,
    );
    scheduler.preload_always_loaded().await.unwrap();
    let category = crate::router::Router::classify(
        "напиши код на Rust",
        &crate::router::TaskMetadata::default(),
    );
    assert_eq!(category, crate::router::Category::Programming);
    assert_eq!(
        scheduler
            .run(category.as_str(), false, request())
            .await
            .unwrap()
            .text(),
        Some("Ответ локальной модели")
    );
    assert!(scheduler
        .run_with_model("Unavailable", request())
        .await
        .is_err());
    assert_eq!(scheduler.snapshot().await.len(), 1);
    assert_eq!(scheduler.running_generations(), 0);
    assert!(scheduler.active_operation().is_none());
    assert!(scheduler.recovery_warnings().is_empty());
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(path, _)| path.starts_with("GET ")
            || path.starts_with("POST /v1/chat/completions?autoload=false")));

    let info = scheduler.backend_info().await;
    assert_eq!(info.connected, Some(true));
    let app = crate::control_center_api::AppState {
        scheduler: Arc::new(scheduler),
        resource_registry: registry,
        task_store: Arc::new(crate::task_store::TaskStore::new(":memory:").unwrap()),
        control: crate::control_center_api::TaskControl::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/backend", listener.local_addr().unwrap());
    let router = crate::control_center_api::build_router(
        app,
        crate::control_auth::ControlToken::for_test("llama-test-token-123456"),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let info: Value = client
        .get(&url)
        .bearer_auth("llama-test-token-123456")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["models"][0]["id"], "provider-model");
    assert_eq!(info["externally_managed"], true);
    task.abort();
}
