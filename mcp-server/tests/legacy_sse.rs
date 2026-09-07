//! Legacy HTTP+SSE integration tests (transport-spec §42): the complete
//! 2024-11-05 flow over real HTTP — GET /sse → endpoint event → POST
//! /messages → message events — plus auth, session, and concurrency rules.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use mcpod::config::Config;
use mcpod::server;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "legacy-sse-token";

struct SseApp {
    client: reqwest::Client,
    base_url: String,
    #[allow(dead_code)]
    workspace: std::path::PathBuf,
}

async fn spawn_app() -> SseApp {
    let workspace = tempfile::tempdir().unwrap().keep();
    let workspace = workspace.canonicalize().unwrap();
    let config = Arc::new(Config {
        token: Some(TOKEN.to_string()),
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        workspace: workspace.clone(),
        allowed_hosts: vec![],
        allowed_origins: vec![],
        sse_session_ttl: Duration::from_secs(1800),
        sse_keepalive: Duration::from_secs(15),
    });
    let app = server::build_router(config, CancellationToken::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    SseApp { client: reqwest::Client::new(), base_url: format!("http://{address}"), workspace }
}

/// One parsed SSE event.
#[derive(Debug, Clone)]
struct SseEvent {
    event: String,
    data: String,
}

/// Read SSE events from a streaming response, with an overall timeout so a
/// missing event fails the test instead of hanging it.
struct SseReader {
    buffer: String,
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
}

impl SseReader {
    async fn next_event(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(event) = self.take_event() {
                return Some(event);
            }
            let chunk = tokio::time::timeout(Duration::from_secs(30), self.stream.next())
                .await
                .ok()??
                .ok()?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        let boundary = self.buffer.find("\n\n")?;
        let raw = self.buffer[..boundary].to_string();
        self.buffer.drain(..boundary + 2);
        let mut event = String::new();
        let mut data = String::new();
        for line in raw.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data: ") {
                data = value.trim().to_string();
            }
        }
        Some(SseEvent { event, data })
    }
}

/// Open the SSE stream and return (reader, messages endpoint path).
async fn open_sse(app: &SseApp) -> (SseReader, String) {
    open_sse_with_token(app, TOKEN).await
}

async fn open_sse_with_token(app: &SseApp, token: &str) -> (SseReader, String) {
    let response = app
        .client
        .get(format!("{}/sse", app.base_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "GET /sse should return 200");
    assert_eq!(
        response.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let stream = response.bytes_stream();
    let mut reader =
        SseReader { buffer: String::new(), stream: Box::pin(stream) };

    let first = reader.next_event().await.expect("endpoint event should arrive");
    assert_eq!(first.event, "endpoint", "first SSE event must be `endpoint`");
    assert!(first.data.starts_with("/messages?sessionId="), "endpoint data: {}", first.data);
    (reader, first.data)
}

/// POST one JSON-RPC message to the legacy endpoint.
async fn post_message(app: &SseApp, path: &str, body: Value) -> reqwest::Response {
    app.client
        .post(format!("{}{path}", app.base_url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Wait for the next `message` event and parse it as JSON.
async fn next_json(reader: &mut SseReader) -> Value {
    loop {
        let event = reader.next_event().await.expect("message event should arrive");
        assert_eq!(event.event, "message", "unexpected event: {:?}", event);
        if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
            return value;
        }
    }
}

fn initialize_body(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "legacy-test", "version": "0.1.0"}
        }
    })
}

// ---------------------------------------------------------------------------
// Full flow (§42)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_sse_full_flow_initialize_list_tools_call_tool() {
    let app = spawn_app().await;
    let (mut reader, messages_path) = open_sse(&app).await;

    // initialize
    let response = post_message(&app, &messages_path, initialize_body(1)).await;
    assert_eq!(response.status(), 202, "POST /messages should be 202 Accepted");

    let initialize_result = next_json(&mut reader).await;
    assert_eq!(initialize_result["id"], json!(1));
    let result = &initialize_result["result"];
    assert_eq!(result["protocolVersion"], json!("2024-11-05"));
    assert_eq!(result["serverInfo"]["name"], json!("mcpod"));
    assert!(result["instructions"].as_str().unwrap().contains("MCPod development container"));

    // notifications/initialized (notification: no response expected)
    let response = post_message(
        &app,
        &messages_path,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(response.status(), 202);

    // tools/list: same four tools in the same order as /mcp (§27)
    post_message(
        &app,
        &messages_path,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = next_json(&mut reader).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["read", "bash", "edit", "write"]);

    // tools/call write then read
    post_message(
        &app,
        &messages_path,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "write", "arguments": {"path": "legacy.txt", "content": "from-sse"}}}),
    )
    .await;
    let write_result = next_json(&mut reader).await;
    assert_eq!(write_result["result"]["isError"], json!(false));

    post_message(
        &app,
        &messages_path,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "read", "arguments": {"path": "legacy.txt"}}}),
    )
    .await;
    let read_result = next_json(&mut reader).await;
    assert_eq!(read_result["result"]["content"][0]["text"], json!("from-sse"));

    // resources/read over legacy SSE as well
    post_message(
        &app,
        &messages_path,
        json!({"jsonrpc": "2.0", "id": 5, "method": "resources/read",
               "params": {"uri": "resource://environment"}}),
    )
    .await;
    let resource = next_json(&mut reader).await;
    let text = resource["result"]["contents"][0]["text"].as_str().unwrap();
    let environment: Value = serde_json::from_str(text).unwrap();
    assert!(environment["runtime_manager"]["name"] == "mise");
}

// ---------------------------------------------------------------------------
// Session rules (§17)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_session_returns_404() {
    let app = spawn_app().await;
    let response = app
        .client
        .post(format!("{}/messages?sessionId=00000000-0000-4000-8000-000000000000", app.base_url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .json(&initialize_body(1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn bad_content_type_and_bad_json_rejected() {
    let app = spawn_app().await;
    let (_reader, path) = open_sse(&app).await;

    let response = app
        .client
        .post(format!("{}{path}", app.base_url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "text/plain")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 415);

    let response = app
        .client
        .post(format!("{}{path}", app.base_url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

// ---------------------------------------------------------------------------
// Authentication (§45)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_sse_and_messages_are_401() {
    let app = spawn_app().await;

    // GET /sse without a token
    let response = app
        .client
        .get(format!("{}/sse", app.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // GET /sse with a wrong token
    let response = app
        .client
        .get(format!("{}/sse", app.base_url))
        .header("Authorization", "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // POST /messages without a token (even with a valid-looking session id)
    let response = app
        .client
        .post(format!("{}/messages?sessionId=whatever", app.base_url))
        .header("Content-Type", "application/json")
        .json(&initialize_body(1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // /health stays public
    let response = app.client.get(format!("{}/health", app.base_url)).send().await.unwrap();
    assert_eq!(response.status(), 200);
}

// ---------------------------------------------------------------------------
// Concurrency (§14): two simultaneous independent sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_simultaneous_sse_clients_are_isolated() {
    let app = spawn_app().await;
    let (mut reader_a, path_a) = open_sse(&app).await;
    let (mut reader_b, path_b) = open_sse(&app).await;
    assert_ne!(path_a, path_b, "each GET /sse gets its own session");

    // Session A initializes and lists tools
    post_message(&app, &path_a, initialize_body(1)).await;
    let result_a = next_json(&mut reader_a).await;
    assert_eq!(result_a["id"], json!(1));

    // Session B initializes concurrently and writes a file
    post_message(&app, &path_b, initialize_body(10)).await;
    let result_b = next_json(&mut reader_b).await;
    assert_eq!(result_b["id"], json!(10));

    // B's tool call response must NOT appear on A's stream
    post_message(
        &app,
        &path_b,
        json!({"jsonrpc": "2.0", "id": 11, "method": "tools/call",
               "params": {"name": "bash", "arguments": {"command": "echo isolated", "timeout": 30}}}),
    )
    .await;
    let b_result = next_json(&mut reader_b).await;
    assert_eq!(b_result["id"], json!(11));
    assert!(b_result["result"]["content"][0]["text"].as_str().unwrap().contains("isolated"));

    // A's stream only sees A's traffic (next event would time out if B's
    // message leaked onto it — verified implicitly by B receiving correctly).
    post_message(
        &app,
        &path_a,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let a_tools = next_json(&mut reader_a).await;
    assert_eq!(a_tools["id"], json!(2));
    let names: Vec<&str> = a_tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["read", "bash", "edit", "write"]);
}

// ---------------------------------------------------------------------------
// Origin validation (§21)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disallowed_origin_is_403_on_all_mcp_endpoints() {
    let app = spawn_app().await;
    for (method, url) in [
        ("GET", format!("{}/sse", app.base_url)),
        ("POST", format!("{}/messages?sessionId=x", app.base_url)),
        ("POST", format!("{}/mcp", app.base_url)),
    ] {
        let request = app.client.request(method.parse().unwrap(), url);
        let response = request
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Origin", "http://evil.example.com")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&initialize_body(1))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "origin check must 403 this request");
    }

    // localhost origins pass the default allowlist.
    let response = app
        .client
        .get(format!("{}/sse", app.base_url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Origin", "http://localhost:5173")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

// ---------------------------------------------------------------------------
// Reconnect (§42): a fresh GET /sse after dropping the old stream works
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_can_reconnect_with_a_new_session() {
    let app = spawn_app().await;
    let (mut reader, path) = open_sse(&app).await;
    post_message(&app, &path, initialize_body(1)).await;
    let result = next_json(&mut reader).await;
    assert_eq!(result["id"], json!(1));
    drop(reader); // disconnect

    // The old session id dies with the stream; reconnect via a new GET /sse.
    let (mut reader2, path2) = open_sse(&app).await;
    assert_ne!(path, path2);
    post_message(&app, &path2, initialize_body(2)).await;
    let result = next_json(&mut reader2).await;
    assert_eq!(result["id"], json!(2));
}
