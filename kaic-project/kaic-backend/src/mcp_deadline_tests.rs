use super::*;

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("kaic-mcp-m1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn config(&self, mode: &str) -> McpServerConfig {
        let script = r#"
import json, os, sys
mode, journal = sys.argv[1:]
with open(journal + '.pid', 'w') as f:
    f.write(str(os.getpid()))
for raw in sys.stdin:
    req = json.loads(raw)
    if 'id' not in req:
        continue
    method = req['method']
    if method == 'initialize':
        if mode == 'handshake_hang':
            continue
        result = {'protocolVersion': req['params']['protocolVersion'],
                  'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'm1-fixture', 'version': '1'}}
    elif method == 'tools/list':
        if mode == 'list_hang':
            continue
        result = {'tools': [{'name': 'test', 'inputSchema': {'type': 'object'}}]}
        if mode == 'repeated_cursor':
            result['nextCursor'] = 'same'
        elif mode == 'endless_pages':
            cursor = int(req.get('params', {}).get('cursor', '0'))
            result['nextCursor'] = str(cursor + 1)
    elif method == 'tools/call':
        with open(journal, 'a') as f:
            f.write(json.dumps(req['params']) + '\n')
        if mode == 'call_hang':
            continue
        if mode == 'transport_close':
            sys.exit(0)
        result = {'content': [{'type': 'text', 'text': 'fixture-result'}]}
        if mode == 'tool_error':
            result['isError'] = True
    elif method == 'ping':
        result = {}
    else:
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)
"#;
        let mut config = McpServerConfig::new("python3");
        config.args = vec![
            "-u".into(),
            "-c".into(),
            script.into(),
            mode.into(),
            self.0.join("dispatch.jsonl").to_string_lossy().into_owned(),
        ];
        config.deadlines = McpDeadlines {
            handshake_ms: 2_000,
            list_ms: 100,
            call_ms: 100,
            shutdown_ms: 2_000,
            max_list_pages: 3,
        };
        config
    }

    fn dispatches(&self) -> usize {
        std::fs::read_to_string(self.0.join("dispatch.jsonl"))
            .map(|s| s.lines().count())
            .unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    async fn assert_child_reaped(&self) {
        let pid = std::fs::read_to_string(self.0.join("dispatch.jsonl.pid")).unwrap();
        let path = std::path::PathBuf::from(format!("/proc/{pid}"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned MCP child must be terminated and reaped");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn deadlines_keep_legacy_yaml_and_reject_unbounded_values() {
    let config: McpServerConfig = serde_yaml::from_str("command: fixture\n").unwrap();
    assert_eq!(config.deadlines.call_ms, 120_000);
    assert!(config.deadlines.validate().is_ok());
    for bad in [0, 3_600_001, u64::MAX] {
        let deadlines = McpDeadlines {
            call_ms: bad,
            ..McpDeadlines::default()
        };
        assert!(deadlines.validate().is_err());
    }
    let deadlines = McpDeadlines {
        max_list_pages: 0,
        ..McpDeadlines::default()
    };
    assert!(deadlines.validate().is_err());
    let fixture = Fixture::new();
    let path = fixture.0.join("invalid.yaml");
    std::fs::write(&path, "command: fixture\ndeadlines:\n  call_ms: 0\n").unwrap();
    assert!(McpServerConfig::load(&path).is_err());
}

#[tokio::test]
async fn successful_call_and_tool_error_keep_existing_results() {
    for (mode, expected) in [
        ("success", "fixture-result"),
        ("tool_error", "ОШИБКА ИНСТРУМЕНТА: fixture-result"),
    ] {
        let fixture = Fixture::new();
        let client = McpClient::connect(&fixture.config(mode)).await.unwrap();
        assert_eq!(client.list_tools().await.unwrap().len(), 1);
        assert_eq!(
            client
                .call_tool("test", serde_json::json!({}))
                .await
                .unwrap(),
            expected
        );
        assert_eq!(fixture.dispatches(), 1);
        client.shutdown().await.unwrap();
        #[cfg(target_os = "linux")]
        fixture.assert_child_reaped().await;
    }
}

#[tokio::test]
async fn hanging_call_has_unknown_outcome_and_cannot_reuse_transport() {
    let fixture = Fixture::new();
    let client = McpClient::connect(&fixture.config("call_hang"))
        .await
        .unwrap();
    let error = client
        .call_tool("test", serde_json::json!({}))
        .await
        .unwrap_err();
    let error = error.downcast_ref::<McpExecutionError>().unwrap();
    assert_eq!(error.kind, McpFailureKind::Timeout);
    assert!(error.outcome_may_be_unknown());
    assert_eq!(fixture.dispatches(), 1, "effect occurred before timeout");
    assert!(client.unusable.load(Ordering::Acquire));
    {
        let rejected = client.call_tool("test", serde_json::json!({}));
        tokio::pin!(rejected);
        // Admission must reject synchronously, without letting the SDK sender
        // race its asynchronous teardown and publish another tools/call.
        assert!(
            std::future::poll_fn(|cx| {
                match std::future::Future::poll(rejected.as_mut(), cx) {
                    std::task::Poll::Ready(result) => std::task::Poll::Ready(result.is_err()),
                    std::task::Poll::Pending => {
                        panic!("closed session must reject before any await")
                    }
                }
            })
            .await
        );
    }
    assert!(client.list_tools().await.is_err());
    assert_eq!(fixture.dispatches(), 1, "no retry after timeout");
    #[cfg(target_os = "linux")]
    fixture.assert_child_reaped().await;
    // Explicit shutdown is not needed for cleanup after a timed out call.
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_call_future_cancels_session_and_reaps_owned_child() {
    let fixture = Fixture::new();
    let mut config = fixture.config("call_hang");
    config.deadlines.call_ms = 5_000;
    let client = std::sync::Arc::new(McpClient::connect(&config).await.unwrap());
    let running = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("test", serde_json::json!({})).await }
    });
    // The durable fixture journal is a readiness signal: cancel only once
    // the server has accepted the request, never based on elapsed sleeps.
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.dispatches() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let client = std::sync::Arc::try_unwrap(client).ok().unwrap();
    assert!(client.unusable.load(Ordering::Acquire));
    assert!(client
        .call_tool("test", serde_json::json!({}))
        .await
        .is_err());
    assert_eq!(fixture.dispatches(), 1);
    #[cfg(target_os = "linux")]
    fixture.assert_child_reaped().await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn transport_loss_is_distinct_from_timeout_and_tool_error() {
    let fixture = Fixture::new();
    let client = McpClient::connect(&fixture.config("transport_close"))
        .await
        .unwrap();
    let error = client
        .call_tool("test", serde_json::json!({}))
        .await
        .unwrap_err();
    let error = error.downcast_ref::<McpExecutionError>().unwrap();
    assert_eq!(error.kind, McpFailureKind::Transport);
    assert!(error.outcome_may_be_unknown());
    assert_eq!(fixture.dispatches(), 1);
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn hanging_handshake_and_list_have_bounded_deadlines() {
    let fixture = Fixture::new();
    let mut config = fixture.config("handshake_hang");
    config.deadlines.handshake_ms = 100;
    let error = McpClient::connect(&config).await.err().unwrap();
    assert_eq!(
        error.downcast_ref::<McpExecutionError>().unwrap().kind,
        McpFailureKind::Timeout
    );

    let fixture = Fixture::new();
    let client = McpClient::connect(&fixture.config("list_hang"))
        .await
        .unwrap();
    let error = client.list_tools().await.unwrap_err();
    let error = error.downcast_ref::<McpExecutionError>().unwrap();
    assert_eq!(error.kind, McpFailureKind::Timeout);
    assert!(!error.outcome_may_be_unknown());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn pagination_rejects_repeated_cursor_and_bounds_unique_pages() {
    for (mode, message) in [
        ("repeated_cursor", "cursor"),
        ("endless_pages", "лимит страниц"),
    ] {
        let fixture = Fixture::new();
        let client = McpClient::connect(&fixture.config(mode)).await.unwrap();
        assert!(client
            .list_tools()
            .await
            .unwrap_err()
            .to_string()
            .contains(message));
        assert_eq!(fixture.dispatches(), 0);
        client.shutdown().await.unwrap();
    }
}
