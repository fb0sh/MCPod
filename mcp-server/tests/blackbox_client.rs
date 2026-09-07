//! Black-box integration test with the official rmcp MCP client (transport-
//! spec §41, §43): a real SDK client connects to `/mcp`, initializes, lists
//! tools, calls tools, and reads resources — no hand-rolled HTTP here.

use std::sync::Arc;
use std::time::Duration;

use mcpod::config::Config;
use mcpod::server;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "blackbox-token";

async fn spawn_server() -> (String, std::path::PathBuf) {
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
    (format!("http://{address}/mcp"), workspace)
}

fn client_transport(
    url: &str,
    token: &str,
) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut headers = std::collections::HashMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let config =
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    StreamableHttpClientTransport::from_config(config)
}

/// The full client lifecycle against the real server: initialize →
/// tools/list → tools/call → resources.
#[tokio::test]
async fn official_client_end_to_end_over_streamable_http() {
    let (url, workspace) = spawn_server().await;
    let client = ().serve(client_transport(&url, TOKEN)).await.expect("client initializes");

    // tools/list: same order and schemas the raw-HTTP tests assert.
    let tools = client.peer().list_tools(Default::default()).await.expect("tools/list");
    let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, vec!["read", "bash", "edit", "write"]);

    // tools/call bash via the real client.
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("bash").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "command": "echo blackbox; pwd",
                    "timeout": 30
                }))
                .unwrap(),
            ),
        )
        .await
        .expect("bash tool call");
    let text = &result.content[0];
    let text = match text {
        rmcp::model::ContentBlock::Text(text) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(text.contains("blackbox"), "bash output: {text}");
    assert!(text.contains(&workspace.display().to_string()), "pwd output: {text}");

    // tools/call write + read roundtrip.
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new("write").with_arguments(
                serde_json::from_value(serde_json::json!({
                    "path": "client.txt",
                    "content": "from official client"
                }))
                .unwrap(),
            ),
        )
        .await
        .expect("write tool call");
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("read").with_arguments(
                serde_json::from_value(serde_json::json!({"path": "client.txt"})).unwrap(),
            ),
        )
        .await
        .expect("read tool call");
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => {
            assert_eq!(text.text, "from official client")
        }
        other => panic!("expected text content, got {other:?}"),
    }

    // resources/list + resources/read.
    let resources = client
        .peer()
        .list_resources(Default::default())
        .await
        .expect("resources/list");
    assert_eq!(resources.resources[0].uri, "resource://environment");
    let read = client
        .peer()
        .read_resource(ReadResourceRequestParams::new("resource://environment"))
        .await
        .expect("resources/read");
    match &read.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
            let environment: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(environment["runtime_manager"]["name"], "mise");
        }
        other => panic!("expected text resource, got {other:?}"),
    }

    client.cancel().await.expect("client shutdown");
}

/// A client with a bad token must fail to initialize (server answers 401).
#[tokio::test]
async fn official_client_with_bad_token_is_rejected() {
    let (url, _workspace) = spawn_server().await;
    let result = ().serve(client_transport(&url, "wrong-token")).await;
    assert!(result.is_err(), "initialization with a bad token must fail");
}

/// Two concurrent official clients share the server cleanly.
#[tokio::test]
async fn two_official_clients_concurrently() {
    let (url, _workspace) = spawn_server().await;
    let client_a = ().serve(client_transport(&url, TOKEN)).await.unwrap();
    let client_b = ().serve(client_transport(&url, TOKEN)).await.unwrap();

    let (a, b) = tokio::join!(
        client_a.peer().list_tools(Default::default()),
        client_b.peer().list_tools(Default::default())
    );
    assert_eq!(a.unwrap().tools.len(), 4);
    assert_eq!(b.unwrap().tools.len(), 4);

    client_a.cancel().await.unwrap();
    client_b.cancel().await.unwrap();
}
