//! Integration tests over real HTTP: MCP protocol, authentication, the
//! Pi-style tool surface (read offset/limit, edit edits[], write, bash),
//! and workspace path containment.

use std::sync::atomic::{AtomicI64, Ordering};

use mcpod::config::Config;
use mcpod::server;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "integration-token";
static REQUEST_ID: AtomicI64 = AtomicI64::new(1);

#[derive(Clone)]
struct TestApp {
    client: reqwest::Client,
    base_url: String,
    /// Canonical workspace path; the backing tempdir is intentionally kept
    /// (not deleted) for the process lifetime.
    workspace: std::path::PathBuf,
}

impl TestApp {
    fn workspace(&self) -> String {
        self.workspace.display().to_string()
    }

    async fn raw_post(&self, body: &str, token: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base_url))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.body(body.to_string()).send().await.unwrap()
    }

    /// Authenticated JSON-RPC request; panics unless HTTP 200 + JSON body.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": REQUEST_ID.fetch_add(1, Ordering::SeqCst),
            "method": method,
            "params": params,
        });
        let response = self.raw_post(&body.to_string(), Some(TOKEN)).await;
        assert_eq!(response.status(), 200, "{method} should be 200");
        response.json().await.unwrap()
    }

    async fn initialize(&self) -> Value {
        self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcpod-test", "version": "0.1.0"},
            }),
        )
        .await
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await["result"]
            .clone()
    }

    fn tool_text(&self, result: &Value) -> String {
        result["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    async fn write_file_via_tool(&self, path: &str, content: &str) -> Value {
        self.call_tool("write", json!({"path": path, "content": content})).await
    }
}

async fn spawn_app() -> TestApp {
    let workspace = tempfile::tempdir().unwrap().keep();
    let workspace = workspace.canonicalize().unwrap();
    let config = std::sync::Arc::new(Config {
        token: Some(TOKEN.to_string()),
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        workspace: workspace.clone(),
        allowed_hosts: vec![],
        allowed_origins: vec![],
        sse_session_ttl: std::time::Duration::from_secs(1800),
        sse_keepalive: std::time::Duration::from_secs(15),
    });
    let app = server::build_router(config, CancellationToken::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestApp { client: reqwest::Client::new(), base_url: format!("http://{address}"), workspace }
}

// ---------------------------------------------------------------------------
// Health (§11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_is_public_and_returns_ok() {
    let app = spawn_app().await;
    let response = app.client.get(format!("{}/health", app.base_url)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), json!({"status": "ok"}));
}

// ---------------------------------------------------------------------------
// Authentication (§10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_requires_bearer_token() {
    let app = spawn_app().await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"},
        },
    })
    .to_string();

    // Missing header, wrong token, wrong scheme: all 401.
    let response = app.raw_post(&body, None).await;
    assert_eq!(response.status(), 401);
    let response = app.raw_post(&body, Some("wrong-token")).await;
    assert_eq!(response.status(), 401);
    let response = app
        .client
        .post(format!("{}/mcp", app.base_url))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // Correct token passes.
    let response = app.raw_post(&body, Some(TOKEN)).await;
    assert_eq!(response.status(), 200);
}

// ---------------------------------------------------------------------------
// MCP protocol
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_returns_server_info_and_instructions() {
    let app = spawn_app().await;
    let response = app.initialize().await;
    let result = &response["result"];
    assert!(result["protocolVersion"].is_string());
    assert_eq!(result["serverInfo"]["name"], "mcpod");
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("MCPod development container"));
    assert!(instructions.contains(&app.workspace()));
    assert!(instructions.contains("bash"));
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["capabilities"]["resources"].is_object());
}

#[tokio::test]
async fn tools_list_exposes_the_four_primitives_in_pi_order() {
    let app = spawn_app().await;
    app.initialize().await;
    let result = app.rpc("tools/list", json!({})).await["result"].clone();
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["read", "bash", "edit", "write"]);
    for tool in result["tools"].as_array().unwrap() {
        let description = tool["description"].as_str().unwrap();
        assert!(description.len() > 30, "{} needs a detailed description", tool["name"]);
        assert!(tool["inputSchema"]["properties"].is_object(), "{} missing schema", tool["name"]);
    }
    // edit schema uses edits[] with oldText/newText
    let edit = result["tools"].as_array().unwrap().iter().find(|t| t["name"] == "edit").unwrap();
    assert!(edit["inputSchema"]["properties"]["edits"].is_object());
    assert!(edit["inputSchema"]["$defs"]["EditEntry"]["properties"]["oldText"].is_object());
    assert!(edit["inputSchema"]["$defs"]["EditEntry"]["properties"]["newText"].is_object());
    assert_eq!(
        edit["inputSchema"]["properties"]["edits"]["items"]["$ref"],
        "#/$defs/EditEntry"
    );
    // read schema uses offset/limit
    let read = result["tools"].as_array().unwrap().iter().find(|t| t["name"] == "read").unwrap();
    assert!(read["inputSchema"]["properties"]["offset"].is_object());
    assert!(read["inputSchema"]["properties"]["limit"].is_object());
    // bash schema has optional timeout
    let bash = result["tools"].as_array().unwrap().iter().find(|t| t["name"] == "bash").unwrap();
    assert!(bash["inputSchema"]["properties"]["timeout"].is_object());

    // every tool advertises an outputSchema (structured output contract)
    for tool in result["tools"].as_array().unwrap() {
        let schema = &tool["outputSchema"];
        assert!(schema.is_object(), "{} must expose outputSchema", tool["name"]);
        assert_eq!(schema["type"], json!("object"));
    }
    let bash_schema = &result["tools"].as_array().unwrap().iter().find(|t| t["name"] == "bash").unwrap()["outputSchema"];
    assert!(bash_schema["properties"]["exit_code"].is_object());
    assert!(bash_schema["properties"]["stdout"].is_object());
    let edit_schema = &result["tools"].as_array().unwrap().iter().find(|t| t["name"] == "edit").unwrap()["outputSchema"];
    assert!(edit_schema["properties"]["diff"].is_object());
    assert!(edit_schema["properties"]["firstChangedLine"].is_object());
}

#[tokio::test]
async fn resources_list_and_read_environment() {
    let app = spawn_app().await;
    app.initialize().await;

    let listed = app.rpc("resources/list", json!({})).await["result"].clone();
    assert_eq!(listed["resources"][0]["uri"], "resource://environment");

    let read = app
        .rpc("resources/read", json!({"uri": "resource://environment"}))
        .await["result"]
        .clone();
    let text = read["contents"][0]["text"].as_str().unwrap();
    let environment: Value = serde_json::from_str(text).unwrap();
    assert_eq!(environment["workspace"], json!(app.workspace()));
    assert!(environment["runtime_manager"]["name"] == "mise");

    // Unknown resource -> protocol-level error
    let response = app.rpc("resources/read", json!({"uri": "resource://nope"})).await;
    assert!(response["error"].is_object());
}

#[tokio::test]
async fn protocol_rejects_malformed_and_unknown_requests() {
    let app = spawn_app().await;

    // Malformed JSON body -> 4xx with an explanatory body, never a crash
    let response = app.raw_post("{not json", Some(TOKEN)).await;
    assert!(response.status().is_client_error());

    // Unsupported method -> JSON-RPC error -32601
    let response = app.rpc("wat/nope", json!({})).await;
    let error = &response["error"];
    assert!(error.is_object(), "expected JSON-RPC error, got: {response}");
    assert_eq!(error["code"], -32601);

    // Malformed JSON-RPC (invalid envelope) -> 4xx with an explanatory
    // body, never a crash or a 5xx.
    let body = json!({"jsonrpc": "2.0", "id": 5, "method": 42}).to_string();
    let response = app.raw_post(&body, Some(TOKEN)).await;
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(
        status.is_client_error() && !text.trim().is_empty(),
        "malformed JSON-RPC should fail cleanly: {status} {text}"
    );
}

#[tokio::test]
async fn mcp_accepts_arbitrary_host_headers() {
    // No MCPOD_ALLOWED_HOSTS configured: the server must be reachable via
    // any Host (IP, domain, or proxy), not just localhost.
    let app = spawn_app().await;
    for host in ["203.0.113.10:3000", "mcpod.internal", "build.mycorp.io:8443"] {
        let response = app
            .client
            .post(format!("{}/mcp", app.base_url))
            .header("Host", host)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "t", "version": "0"},
                },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "Host {host} should be accepted");
    }
}

#[tokio::test]
async fn get_mcp_is_method_not_allowed_for_stateless_server() {
    let app = spawn_app().await;
    let response = app
        .client
        .get(format!("{}/mcp", app.base_url))
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
}

// ---------------------------------------------------------------------------
// read (pi-spec §4–§9)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_whole_small_file_and_line_window() {
    let app = spawn_app().await;
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    app.write_file_via_tool("src/app.txt", &content).await;

    // whole file
    let result = app.call_tool("read", json!({"path": "src/app.txt"})).await;
    assert_eq!(result["isError"], json!(false));
    let text = app.tool_text(&result);
    assert!(text.starts_with("line 1\n"));
    assert!(text.ends_with("line 100\n"));

    // offset+limit window: lines 10..=24
    let result = app
        .call_tool("read", json!({"path": "src/app.txt", "offset": 10, "limit": 15}))
        .await;
    let text = app.tool_text(&result);
    assert!(text.starts_with("line 10\n"));
    assert!(text.contains("line 24\n"));
    assert!(!text.contains("line 25\n"));
    assert!(text.ends_with("[76 more lines in file. Use offset=25 to continue.]"));

    // offset beyond EOF
    let result = app.call_tool("read", json!({"path": "src/app.txt", "offset": 500})).await;
    assert_eq!(result["isError"], json!(true));
    assert_eq!(app.tool_text(&result), "Offset 500 is beyond end of file (100 lines total)");

    // missing file
    let result = app.call_tool("read", json!({"path": "src/missing.txt"})).await;
    assert_eq!(app.tool_text(&result), "File not found: src/missing.txt");
}

#[tokio::test]
async fn read_paginates_a_large_file() {
    let app = spawn_app().await;
    let content: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    app.write_file_via_tool("big.txt", &content).await;

    let result = app.call_tool("read", json!({"path": "big.txt"})).await;
    let text = app.tool_text(&result);
    assert!(text.contains("line 2000\n"));
    assert!(!text.contains("line 2001\n"));
    assert!(text.ends_with("[Showing lines 1-2000 of 5000. Use offset=2001 to continue.]"));

    let result = app.call_tool("read", json!({"path": "big.txt", "offset": 2001})).await;
    let text = app.tool_text(&result);
    assert!(text.starts_with("line 2001\n"));
    assert!(text.ends_with("[Showing lines 2001-4000 of 5000. Use offset=4001 to continue.]"));
}

#[tokio::test]
async fn read_returns_image_content() {
    let app = spawn_app().await;
    // 1x1 PNG written as real bytes on disk.
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
        0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
        0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
        0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    tokio::fs::write(app.workspace.join("pixel.png"), png).await.unwrap();

    let result = app.call_tool("read", json!({"path": "pixel.png"})).await;
    assert_eq!(result["isError"], json!(false));
    let blocks = &result["content"];
    assert_eq!(blocks[0]["text"], "Read image file [image/png]");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["mimeType"], "image/png");
}

// ---------------------------------------------------------------------------
// write (pi-spec §10–§12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_creates_and_replaces_files() {
    let app = spawn_app().await;
    let result = app.write_file_via_tool("src/deep/nested/new.txt", "first").await;
    assert_eq!(result["isError"], json!(false));
    let text = app.tool_text(&result);
    assert_eq!(text, "Successfully wrote 5 bytes to src/deep/nested/new.txt");
    assert_eq!(result["structuredContent"]["bytes"], json!(5));
    assert_eq!(result["structuredContent"]["path"], json!("src/deep/nested/new.txt"));
    assert_eq!(result["structuredContent"]["success"], json!(true));

    let result = app.write_file_via_tool("src/deep/nested/new.txt", "second").await;
    assert_eq!(
        app.tool_text(&result),
        "Successfully wrote 6 bytes to src/deep/nested/new.txt"
    );

    let result = app.call_tool("read", json!({"path": "src/deep/nested/new.txt"})).await;
    assert_eq!(app.tool_text(&result), "second");
}

// ---------------------------------------------------------------------------
// edit (pi-spec §13–§23)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_multiple_disjoint_replacements_with_diff() {
    let app = spawn_app().await;
    app.write_file_via_tool(
        "src/main.rs",
        "const PORT: u16 = 3000;\n\nfn hello() {\n    println!(\"hello\");\n}\n\nfn goodbye() {\n    println!(\"bye\");\n}\n",
    )
    .await;

    let result = app
        .call_tool(
            "edit",
            json!({
                "path": "src/main.rs",
                "edits": [
                    {"oldText": "const PORT: u16 = 3000;", "newText": "const PORT: u16 = 8080;"},
                    {"oldText": "println!(\"bye\");", "newText": "println!(\"goodbye\");"}
                ]
            }),
        )
        .await;
    assert_eq!(result["isError"], json!(false));
    assert_eq!(
        app.tool_text(&result),
        "Successfully replaced 2 block(s) in src/main.rs."
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["success"], json!(true));
    assert_eq!(structured["replacements"], json!(2));
    assert_eq!(structured["firstChangedLine"], json!(1));
    assert!(structured["diff"].as_str().unwrap().contains("-const PORT: u16 = 3000;"));
    assert!(structured["patch"].as_str().unwrap().contains("a/src/main.rs"));

    let result = app.call_tool("read", json!({"path": "src/main.rs"})).await;
    let text = app.tool_text(&result);
    assert!(text.contains("const PORT: u16 = 8080;"));
    assert!(text.contains("println!(\"goodbye\");"));
    assert!(text.contains("println!(\"hello\");"));
}

#[tokio::test]
async fn edit_error_semantics() {
    let app = spawn_app().await;
    app.write_file_via_tool("f.txt", "alpha beta\n").await;

    // not found
    let result = app
        .call_tool("edit", json!({"path": "f.txt", "edits": [{"oldText": "delta", "newText": "x"}]}))
        .await;
    assert_eq!(result["isError"], json!(true));
    assert_eq!(
        app.tool_text(&result),
        "Could not find the text in f.txt.\nThe oldText must identify the intended text in the file."
    );

    // multiple matches
    app.write_file_via_tool("f.txt", "ha ha ha\n").await;
    let result = app
        .call_tool("edit", json!({"path": "f.txt", "edits": [{"oldText": "ha", "newText": "x"}]}))
        .await;
    assert!(app.tool_text(&result).starts_with("Found 3 occurrences"));

    // empty oldText
    let result = app
        .call_tool("edit", json!({"path": "f.txt", "edits": [{"oldText": "", "newText": "x"}]}))
        .await;
    assert_eq!(app.tool_text(&result), "oldText must not be empty");

    // no-op
    app.write_file_via_tool("f.txt", "same\n").await;
    let result = app
        .call_tool(
            "edit",
            json!({"path": "f.txt", "edits": [{"oldText": "same", "newText": "same"}]}),
        )
        .await;
    assert_eq!(
        app.tool_text(&result),
        "No changes made to f.txt.\nThe replacement produced identical content."
    );

    // overlapping edits: whole call rejected, file untouched
    app.write_file_via_tool("f.txt", "abcdef\n").await;
    let result = app
        .call_tool(
            "edit",
            json!({"path": "f.txt", "edits": [
                {"oldText": "abcd", "newText": "X"},
                {"oldText": "cdef", "newText": "Y"}
            ]}),
        )
        .await;
    assert_eq!(
        app.tool_text(&result),
        "edits[0] and edits[1] overlap in f.txt.\nMerge them into one edit or target disjoint regions."
    );
    let result = app.call_tool("read", json!({"path": "f.txt"})).await;
    assert_eq!(app.tool_text(&result), "abcdef\n");
}

#[tokio::test]
async fn edit_crlf_and_smart_quote_fallback() {
    let app = spawn_app().await;
    // CRLF file: oldText with LF matches via normalization; CRLF preserved.
    tokio::fs::write(app.workspace.join("win.txt"), "a\r\nb\r\nc\r\n")
        .await
        .unwrap();
    let result = app
        .call_tool(
            "edit",
            json!({"path": "win.txt", "edits": [{"oldText": "a\nb", "newText": "X\nY"}]}),
        )
        .await;
    assert_eq!(result["isError"], json!(false));
    let bytes = tokio::fs::read(app.workspace.join("win.txt")).await.unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "X\r\nY\r\nc\r\n");

    // Smart quotes in the file, ASCII quotes in oldText: normalized fallback.
    app.write_file_via_tool("quotes.txt", "print(‘hello’);\n").await;
    let result = app
        .call_tool(
            "edit",
            json!({"path": "quotes.txt", "edits": [
                {"oldText": "print('hello');", "newText": "print(\"hi\");"}
            ]}),
        )
        .await;
    assert_eq!(result["isError"], json!(false));
    let result = app.call_tool("read", json!({"path": "quotes.txt"})).await;
    assert_eq!(app.tool_text(&result), "print(\"hi\");\n");
}

#[tokio::test]
async fn concurrent_edits_serialize_without_lost_updates() {
    let app = spawn_app().await;
    let content: String = (1..=20).map(|i| format!("item-{i:02}\n")).collect();
    app.write_file_via_tool("shared.txt", &content).await;

    let mut tasks = Vec::new();
    for i in 1..=20 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            app.call_tool(
                "edit",
                json!({"path": "shared.txt", "edits": [
                    {"oldText": format!("item-{i:02}"), "newText": format!("item-{i:02}-edited")}
                ]}),
            )
            .await
        }));
    }
    for task in tasks {
        let result = task.await.unwrap();
        assert_eq!(result["isError"], json!(false), "{}", result);
    }
    let result = app.call_tool("read", json!({"path": "shared.txt"})).await;
    let text = app.tool_text(&result);
    for i in 1..=20 {
        assert!(
            text.contains(&format!("item-{i:02}-edited")),
            "missing item-{i:02}-edited in:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// bash (pi-spec §25–§35)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_structured_result_and_working_directory() {
    let app = spawn_app().await;
    let result = app
        .call_tool(
            "bash",
            json!({"command": "echo hello; echo oops >&2; exit 7", "timeout": 30}),
        )
        .await;
    assert_eq!(result["isError"], json!(true), "non-zero exit marks the result as an error");
    let structured = &result["structuredContent"];
    assert_eq!(structured["exit_code"], 7);
    assert_eq!(structured["stdout"].as_str().unwrap().trim(), "hello");
    assert_eq!(structured["stderr"].as_str().unwrap().trim(), "oops");
    let text = app.tool_text(&result);
    assert!(text.contains("hello"));
    assert!(text.contains("oops"));
    assert!(text.contains("Command exited with code 7"));

    // pwd is the workspace
    let result = app.call_tool("bash", json!({"command": "pwd", "timeout": 30})).await;
    let structured = &result["structuredContent"];
    assert_eq!(structured["stdout"].as_str().unwrap().trim(), app.workspace());
}

#[tokio::test]
async fn bash_no_output_success_and_no_output_failure() {
    let app = spawn_app().await;
    let result = app.call_tool("bash", json!({"command": "true", "timeout": 10})).await;
    assert_eq!(result["isError"], json!(false));
    assert_eq!(app.tool_text(&result), "(no output)");

    let result = app.call_tool("bash", json!({"command": "exit 7", "timeout": 10})).await;
    assert_eq!(app.tool_text(&result), "Command exited with code 7");
}

#[tokio::test]
async fn bash_invalid_timeout_is_rejected() {
    let app = spawn_app().await;
    for bad in [0, -5] {
        let result = app.call_tool("bash", json!({"command": "true", "timeout": bad})).await;
        assert_eq!(
            app.tool_text(&result),
            "Invalid timeout: must be a finite number of seconds"
        );
    }
}

#[tokio::test]
async fn bash_timeout_kills_and_reports() {
    let app = spawn_app().await;
    let started = std::time::Instant::now();
    let result = app
        .call_tool("bash", json!({"command": "echo starting; sleep 60", "timeout": 1}))
        .await;
    assert!(started.elapsed().as_secs() < 30);
    assert_eq!(result["isError"], json!(true));
    let structured = &result["structuredContent"];
    assert_eq!(structured["exit_code"], 124);
    assert_eq!(structured["timed_out"], json!(true));
    let text = app.tool_text(&result);
    assert!(text.contains("starting"));
    assert!(text.contains("Command timed out after 1 seconds"));
}

#[tokio::test]
async fn bash_tail_truncation_saves_full_output_to_tmp_log() {
    let app = spawn_app().await;
    let result = app
        .call_tool("bash", json!({"command": "seq 1 30000", "timeout": 120}))
        .await;
    let text = app.tool_text(&result);
    assert!(text.contains("[Showing lines 28001-30000 of 30000. Full output: /tmp/mcpod-bash-"));
    assert!(text.contains(".log]"));
    assert!(text.contains("30000\n"));
    assert!(!text.contains("\n1\n2\n3\n"));
}

#[tokio::test]
async fn bash_sees_files_written_by_write_tool() {
    let app = spawn_app().await;
    app.write_file_via_tool("notes.txt", "mcpod").await;
    let result = app.call_tool("bash", json!({"command": "cat notes.txt", "timeout": 30})).await;
    assert_eq!(
        result["structuredContent"]["stdout"].as_str().unwrap().trim(),
        "mcpod"
    );
}

#[tokio::test]
async fn bash_can_read_container_files_by_design() {
    // The container is the security boundary: /etc/passwd via bash is allowed.
    let app = spawn_app().await;
    let result = app
        .call_tool("bash", json!({"command": "grep '^root:' /etc/passwd | wc -l", "timeout": 30}))
        .await;
    assert_eq!(result["structuredContent"]["exit_code"], 0);
    assert_eq!(result["structuredContent"]["stdout"].as_str().unwrap().trim(), "1");
}

// ---------------------------------------------------------------------------
// Workspace containment (PRD §17, pi-spec §36, §37)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_tools_reject_path_escapes() {
    let app = spawn_app().await;
    // A sibling directory with a tempting name prefix.
    let evil = app
        .workspace
        .parent()
        .unwrap()
        .join(format!("{}-evil", app.workspace.file_name().unwrap().to_str().unwrap()));
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(evil.join("secret.txt"), "secret").unwrap();

    let attempts = [
        json!({"path": "../evil.txt", "content": "x"}),
        json!({"path": "../../etc/passwd"}),
        json!({"path": "/etc/passwd"}),
        json!({"path": evil.join("secret.txt").display().to_string()}),
        json!({"path": "nested/../../escape.txt", "content": "x"}),
    ];
    for arguments in attempts {
        for tool in ["read", "write"] {
            let mut arguments = arguments.clone();
            if tool == "write" && arguments.get("content").is_none() {
                arguments["content"] = json!("x");
            }
            if tool == "read" {
                arguments.as_object_mut().unwrap().remove("content");
            }
            let result = app.call_tool(tool, arguments.clone()).await;
            assert_eq!(result["isError"], json!(true), "{tool} should reject {arguments}");
            assert!(
                app.tool_text(&result).contains("escapes the workspace"),
                "{tool} escape error should be explicit: {}",
                app.tool_text(&result)
            );
        }
    }
    assert!(!evil.join("escape.txt").exists());
    let _ = std::fs::remove_dir_all(&evil);
}

#[tokio::test]
async fn file_tools_reject_symlink_escape() {
    let app = spawn_app().await;
    let link = app.workspace.join("escape");
    std::os::unix::fs::symlink("/etc", &link).unwrap();

    let result = app.call_tool("read", json!({"path": "escape/passwd"})).await;
    assert_eq!(result["isError"], json!(true));
    assert!(app.tool_text(&result).contains("escapes the workspace"));

    let result = app
        .call_tool("write", json!({"path": "escape/newfile", "content": "x"}))
        .await;
    assert_eq!(result["isError"], json!(true));
    assert!(!std::path::Path::new("/etc/mcpod-escape-marker").exists());

    std::fs::remove_file(&link).unwrap();
}

#[tokio::test]
async fn edit_rejects_path_escape() {
    let app = spawn_app().await;
    let result = app
        .call_tool(
            "edit",
            json!({"path": "../../etc/passwd", "edits": [{"oldText": "a", "newText": "b"}]}),
        )
        .await;
    assert_eq!(result["isError"], json!(true));
    assert!(app.tool_text(&result).contains("escapes the workspace"));
}
