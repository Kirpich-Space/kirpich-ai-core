//! Independent M1 checks at the real authenticated HTTP and MCP/storage boundaries.
use super::*;
use crate::model_backend::ToolCall;
use crate::task_store::ToolServerIdentity;
use serde_json::{json, Value};

const TOKEN: &str = "m1-lifecycle-test-token-123456";

struct FixtureDir(std::path::PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct HttpServer {
    base: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl HttpServer {
    async fn start(state: AppState) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = build_router(state, crate::control_auth::ControlToken::for_test(TOKEN));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            task,
        }
    }

    async fn post(&self, path: &str, body: Value) -> u16 {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn task(&self, id: Uuid) -> Value {
        self.client
            .get(format!("{}/tasks/{id}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "provider-original-call".into(),
        name: "fixture_write".into(),
        arguments: "{\"text\":\"exact original 🧱\"}".into(),
    }
}

async fn recovered(executing: bool) -> (AppState, FixtureDir, ToolApproval) {
    let (mut state, dir) = super::tests::pipeline_fixture(0);
    let dir = FixtureDir(dir);
    let path = dir.0.join("restart.db");
    let store = TaskStore::new(&path).unwrap();
    let task = store.create("Simple").await.unwrap();
    let snapshot = store
        .park_tool_approval(
            task.id,
            Uuid::new_v4(),
            &call(),
            ToolServerIdentity {
                command: "synthetic-mcp".into(),
                args: vec!["exact argument".into()],
                peer_name: "m1-fixture".into(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    store
        .claim_tool_approval(task.id, snapshot.request_id, true)
        .await
        .unwrap()
        .unwrap();
    if executing {
        assert!(store.begin_approved_execution(&snapshot).await.unwrap());
    }
    drop(store);
    let store = Arc::new(TaskStore::new(&path).unwrap());
    assert_eq!(store.reconcile_startup().await.unwrap(), 1);
    assert_eq!(store.reconcile_startup().await.unwrap(), 0);
    state.task_store = store;
    (state, dir, snapshot)
}

#[tokio::test]
async fn restart_unknown_is_visible_and_recovery_http_auth_and_identity_are_enforced() {
    for executing in [false, true] {
        let (state, _dir, snapshot) = recovered(executing).await;
        let other = state.task_store.create("Simple").await.unwrap();
        state
            .task_store
            .set_status(other.id, TaskStatus::Paused)
            .await
            .unwrap();
        let server = HttpServer::start(state.clone()).await;
        let task = server.task(snapshot.task_id).await;
        assert_eq!(task["status"], "waiting_for_human");
        assert!(task["pending_tool_approval"].is_null());
        assert_eq!(task["recovery"]["kind"], "unknown_outcome");
        assert_eq!(
            task["recovery"]["unresolved_tools"],
            json!([snapshot.clone()])
        );
        let path = format!("/tasks/{}/recovery-decision", snapshot.task_id);
        let body = json!({"request_id":snapshot.request_id,"acknowledge":true});
        let unauthorized = server
            .client
            .post(format!("{}{path}", server.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status().as_u16(), 401);
        assert_eq!(
            server
                .post(
                    &path,
                    json!({"request_id":snapshot.request_id,"acknowledge":false})
                )
                .await,
            400
        );
        assert_eq!(
            server
                .post(&path, json!({"request_id":snapshot.request_id}))
                .await,
            422
        );
        assert_eq!(
            server
                .post(
                    &format!("/tasks/{}/recovery-decision", other.id),
                    body.clone()
                )
                .await,
            409
        );
        assert_eq!(
            server
                .post(
                    &path,
                    json!({"request_id":Uuid::new_v4(),"acknowledge":true})
                )
                .await,
            409
        );
        assert_eq!(server.task(snapshot.task_id).await, task);
        assert!(!state.control.is_running(snapshot.task_id).await);
    }
}

#[tokio::test]
async fn concurrent_recovery_http_has_one_winner_and_cannot_rearm_original_request() {
    let (state, _dir, snapshot) = recovered(true).await;
    let server = HttpServer::start(state.clone()).await;
    let path = format!("/tasks/{}/recovery-decision", snapshot.task_id);
    let body = json!({"request_id":snapshot.request_id,"acknowledge":true});
    let (a, b) = tokio::join!(
        server.post(&path, body.clone()),
        server.post(&path, body.clone())
    );
    let mut statuses = [a, b];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409]);
    assert_eq!(server.post(&path, body).await, 409);
    for approve in [true, false] {
        assert_eq!(
            server
                .post(
                    &format!("/tasks/{}/tool-decision", snapshot.task_id),
                    json!({"request_id":snapshot.request_id,"approve":approve}),
                )
                .await,
            409
        );
    }
    let task = server.task(snapshot.task_id).await;
    assert_eq!(task["status"], "paused");
    assert!(task["recovery"].is_null());
    assert_eq!(
        task["context"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["role"] == "tool")
            .count(),
        1
    );
    assert!(!state.control.is_running(snapshot.task_id).await);
    assert!(!state
        .task_store
        .begin_approved_execution(&snapshot)
        .await
        .unwrap());
    let status: Value = server
        .client
        .get(format!("{}/status", server.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["running_tasks"], 0);
}

#[tokio::test]
async fn recovery_ack_waits_for_old_executor_to_unregister_and_keeps_cancelled_state() {
    let (state, _dir, snapshot) = recovered(true).await;
    let receiver = state.control.register(snapshot.task_id).await.unwrap();
    let server = HttpServer::start(state.clone()).await;
    let path = format!("/tasks/{}/recovery-decision", snapshot.task_id);
    let body = json!({"request_id":snapshot.request_id,"acknowledge":true});
    assert_eq!(server.post(&path, body.clone()).await, 409);
    assert_eq!(
        server
            .post(
                &format!("/tasks/{}/continue", snapshot.task_id),
                json!({"message":"retry"})
            )
            .await,
        409
    );
    assert_eq!(
        server
            .post(&format!("/tasks/{}/cancel", snapshot.task_id), json!({}))
            .await,
        200
    );
    assert!(*receiver.borrow());
    assert_eq!(server.post(&path, body.clone()).await, 409);
    state.control.unregister(snapshot.task_id).await;
    assert_eq!(server.post(&path, body).await, 200);
    let task = server.task(snapshot.task_id).await;
    assert_eq!(task["status"], "cancelled");
    assert!(task["recovery"].is_null());
    assert!(!state.control.is_running(snapshot.task_id).await);
}

#[tokio::test]
async fn real_mcp_timeout_persists_unknown_for_allowlisted_execution_without_retry() {
    let (mut state, dir) = super::tests::pipeline_fixture(0);
    let _dir = FixtureDir(dir);
    let database = _dir.0.join("timeout.db");
    state.task_store = Arc::new(TaskStore::new(&database).unwrap());
    let journal = _dir.0.join("effect.jsonl");
    let script = r#"
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    if req['method'] == 'initialize':
        result = {'protocolVersion': req['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name':'lifecycle-fixture','version':'1'}}
    elif req['method'] == 'tools/call':
        with open(sys.argv[1], 'a') as journal:
            journal.write(json.dumps(req['params']) + '\n')
        continue
    elif req['method'] == 'tools/list':
        result = {'tools': []}
    else:
        continue
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;
    let mut config = McpServerConfig::new("python3");
    config.args = vec![
        "-u".into(),
        "-c".into(),
        script.into(),
        journal.to_string_lossy().into_owned(),
    ];
    config.deadlines.call_ms = 200;
    config.deadlines.handshake_ms = 2_000;
    config.deadlines.shutdown_ms = 2_000;
    let client = McpClient::connect(&config).await.unwrap();
    let server_identity = client.identity().clone();
    let task = state.task_store.create("Simple").await.unwrap();
    let call = call();
    let snapshot = state
        .task_store
        .begin_auto_approved_execution(task.id, Uuid::new_v4(), &call, client.identity().clone())
        .await
        .unwrap()
        .unwrap();
    let approved = match crate::tool_gate::decide(&[call.name.clone()], call.clone()) {
        crate::tool_gate::Decision::Approved(approved) => approved,
        _ => panic!("fixture allowlist must approve exact name"),
    };
    let error = execute_tool(&state.task_store, task.id, &client, &approved, 1)
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::mcp::McpExecutionError>()
            .unwrap()
            .kind,
        crate::mcp::McpFailureKind::Timeout
    );
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap().lines().count(),
        1
    );
    client.shutdown().await.unwrap();
    // Separate connection verifies the API is reading durable recovery, not only in-memory metadata.
    state.task_store = Arc::new(TaskStore::new(&database).unwrap());
    assert_eq!(state.task_store.reconcile_startup().await.unwrap(), 0);
    let server = HttpServer::start(state.clone()).await;
    let task = server.task(task.id).await;
    assert_eq!(task["status"], "waiting_for_human");
    assert_eq!(task["recovery"]["kind"], "unknown_outcome");
    assert_eq!(
        task["recovery"]["unresolved_tools"],
        json!([snapshot.clone()])
    );
    assert!(!state
        .task_store
        .begin_approved_execution(&snapshot)
        .await
        .unwrap());
    assert!(state
        .task_store
        .begin_auto_approved_execution(
            snapshot.task_id,
            Uuid::new_v4(),
            &call,
            server_identity.clone()
        )
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap().lines().count(),
        1
    );
    assert!(!state.control.is_running(snapshot.task_id).await);
}
