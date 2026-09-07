//! Legacy HTTP+SSE transport (compatibility, transport-spec §3, §10–§17,
//! §28–§30): MCP `2024-11-05` HTTP+SSE wire protocol.
//!
//! A thin adapter: SSE framing, session routing, and connection lifecycle
//! live here; ALL MCP semantics (JSON-RPC, tools, resources, instructions)
//! come from rmcp driving the shared [`crate::server::McpodServer`].
//!
//! Flow: `GET /sse` opens one session and emits `event: endpoint` with a
//! `/messages?sessionId=<uuid>` URL; the client POSTs JSON-RPC messages to
//! that endpoint (202 Accepted); responses stream back as `event: message`
//! over the SSE connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::{once, StreamExt as _};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::{RoleServer, ServiceExt as _};
use rmcp::transport::Transport;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::Config;
use crate::server::McpodServer;

/// Outbound channel capacity (§30): bounded so a client that stops reading
/// applies backpressure instead of consuming unbounded memory.
const OUTBOUND_CAPACITY: usize = 64;
/// A stuck client that stops draining for this long is dropped (§30).
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Session manager (§13)
// ---------------------------------------------------------------------------

struct SessionEntry {
    inbound_tx: mpsc::Sender<ClientJsonRpcMessage>,
    cancellation: CancellationToken,
    /// Set when the SSE stream ends; the sweeper removes the entry after the
    /// configured TTL (§16).
    disconnected_at: Option<Instant>,
}

#[derive(Default)]
struct SessionsInner {
    map: HashMap<String, SessionEntry>,
}

#[derive(Default)]
pub struct LegacySseManager {
    inner: Mutex<SessionsInner>,
    config: Option<Arc<Config>>,
}

impl LegacySseManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self { inner: Mutex::new(SessionsInner { map: HashMap::new() }), config: Some(config) }
    }

    fn config(&self) -> Arc<Config> {
        self.config.clone().expect("config set at construction")
    }

    /// Open a session: creates the id, channels, and spawn the rmcp service
    /// loop. Returns the session id and the outbound receiver the SSE stream
    /// will drain.
    pub fn open(&self) -> (String, mpsc::Receiver<ServerJsonRpcMessage>) {
        let config = self.config();
        let session_id = uuid::Uuid::new_v4().to_string();
        let (inbound_tx, inbound_rx) = mpsc::channel::<ClientJsonRpcMessage>(OUTBOUND_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<ServerJsonRpcMessage>(OUTBOUND_CAPACITY);
        let cancellation = CancellationToken::new();

        // One logical MCP connection per SSE session (§14): each session gets
        // its own McpodServer instance from the shared factory (§25) and its
        // own rmcp service loop.
        let server = McpodServer::new(config);
        let transport = LegacySseTransport::new(inbound_rx, outbound_tx);
        let service_ct = cancellation.clone();
        let session_for_log = session_id.clone();
        tokio::spawn(async move {
            let short = &session_for_log[..8.min(session_for_log.len())];
            match server.serve_with_ct(transport, service_ct.clone()).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(error) => {
                    // Pre-initialize disconnects and handshake errors are
                    // routine for legacy clients; log at debug level.
                    tracing::debug!(
                        transport = "legacy_sse",
                        session = %short,
                        %error,
                        "legacy session ended before initialize"
                    );
                }
            }
        });

        {
            let mut inner = self.inner.lock().expect("legacy session map poisoned");
            inner.map.insert(
                session_id.clone(),
                SessionEntry { inbound_tx, cancellation, disconnected_at: None },
            );
        }
        info!(
            transport = "legacy_sse",
            session = %&session_id[..8.min(session_id.len())],
            active = self.active_count(),
            "legacy sse session opened"
        );
        (session_id, outbound_rx)
    }

    /// Deliver a client JSON-RPC message to its session. Unknown or dead
    /// sessions yield `None` (mapped to 404, §17 — never implicitly created).
    pub async fn deliver(&self, session_id: &str, message: ClientJsonRpcMessage) -> Option<()> {
        let sender = {
            let inner = self.inner.lock().expect("legacy session map poisoned");
            inner.map.get(session_id).map(|entry| entry.inbound_tx.clone())
        };
        match sender {
            Some(tx) => tx.send(message).await.is_ok().then_some(()),
            None => None,
        }
    }

    /// Mark a session's SSE stream as gone (§15): cancels the service loop.
    pub fn disconnect(&self, session_id: &str) {
        let mut inner = self.inner.lock().expect("legacy session map poisoned");
        if let Some(entry) = inner.map.get_mut(session_id) {
            entry.cancellation.cancel();
            entry.disconnected_at.get_or_insert_with(Instant::now);
        }
        drop(inner);
        info!(
            transport = "legacy_sse",
            session = %&session_id[..8.min(session_id.len())],
            active = self.active_count(),
            "legacy sse session disconnected"
        );
    }

    /// Remove sessions whose connection has been gone longer than the TTL
    /// (§16). Active sessions are never touched.
    pub fn sweep(&self) {
        let ttl = self.config().sse_session_ttl;
        let mut inner = self.inner.lock().expect("legacy session map poisoned");
        let before = inner.map.len();
        inner.map.retain(|_, entry| {
            !matches!(entry.disconnected_at, Some(at) if at.elapsed() > ttl)
        });
        let removed = before - inner.map.len();
        if removed > 0 {
            info!(transport = "legacy_sse", removed, remaining = inner.map.len(), "session sweep");
        }
    }

    /// Cancel every session (graceful shutdown, §31).
    pub fn close_all(&self) {
        let mut inner = self.inner.lock().expect("legacy session map poisoned");
        for (_, entry) in inner.map.iter_mut() {
            entry.cancellation.cancel();
        }
        inner.map.clear();
    }

    pub fn active_count(&self) -> usize {
        let inner = self.inner.lock().expect("legacy session map poisoned");
        inner.map.values().filter(|entry| entry.disconnected_at.is_none()).count()
    }
}

/// Background sweeper: periodically drop dead sessions past their TTL.
fn spawn_sweeper(manager: Arc<LegacySseManager>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => manager.sweep(),
                () = shutdown.cancelled() => {
                    manager.close_all();
                    break;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// rmcp Transport impl (§5: protocol handling stays in rmcp)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LegacySseTransportError {
    #[error("session closed")]
    Closed,
    #[error("client stopped reading SSE stream")]
    SendTimeout,
}

struct LegacySseTransport {
    inbound_rx: mpsc::Receiver<ClientJsonRpcMessage>,
    outbound_tx: mpsc::Sender<ServerJsonRpcMessage>,
}

impl LegacySseTransport {
    fn new(
        inbound_rx: mpsc::Receiver<ClientJsonRpcMessage>,
        outbound_tx: mpsc::Sender<ServerJsonRpcMessage>,
    ) -> Self {
        Self { inbound_rx, outbound_tx }
    }
}

impl Transport<RoleServer> for LegacySseTransport {
    type Error = LegacySseTransportError;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let tx = self.outbound_tx.clone();
        async move {
            match tokio::time::timeout(SEND_TIMEOUT, tx.send(item)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(LegacySseTransportError::Closed),
                Err(_) => Err(LegacySseTransportError::SendTimeout),
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<ClientJsonRpcMessage>> + Send {
        self.inbound_rx.recv()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(())) // channels close when the transport drops
    }
}

// ---------------------------------------------------------------------------
// Axum handlers (§10–§12, §17, §28)
// ---------------------------------------------------------------------------

/// `GET /sse`: open the stream, send `event: endpoint`, then relay rmcp
/// messages as `event: message`.
async fn sse_handler(
    State(manager): State<Arc<LegacySseManager>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (session_id, outbound_rx) = manager.open();
    let messages_path = format!("/messages?sessionId={session_id}");

    // The endpoint event is the first thing on the wire (§12).
    let endpoint_event = once(async move {
        Ok::<_, std::convert::Infallible>(
            Event::default().event("endpoint").data(messages_path),
        )
    });

    // Then every server message, framed as `event: message`. When the SSE
    // response is dropped (client disconnect), cancel the session (§15).
    let disconnect_guard = DisconnectGuard {
        manager: manager.clone(),
        session_id: session_id.clone(),
        cancelled: false,
    };
    let message_stream = ReceiverStream::new(outbound_rx).map(move |message| {
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event("message")
                .data(serde_json::to_string(&message).expect("JSON-RPC message serializes")),
        )
    });

    let stream = endpoint_event
        .chain(SseMessageStream {
            inner: message_stream,
            guard: disconnect_guard,
        })
        // Tiny yield so the endpoint event flushes before messages can race.
        ;

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(manager.config().sse_keepalive)
            .text("keepalive"),
    )
}

/// Stream wrapper whose drop marks the session disconnected (§15).
struct SseMessageStream<S> {
    inner: S,
    /// Held for its `Drop`: marks the session disconnected when the SSE
    /// response stream goes away (client disconnect, §15).
    #[expect(dead_code, reason = "drop-only cleanup guard")]
    guard: DisconnectGuard,
}

impl<S> futures_util::Stream for SseMessageStream<S>
where
    S: futures_util::Stream<Item = Result<Event, std::convert::Infallible>> + Unpin,
{
    type Item = Result<Event, std::convert::Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

struct DisconnectGuard {
    manager: Arc<LegacySseManager>,
    session_id: String,
    cancelled: bool,
}

impl DisconnectGuard {
    fn fire(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            self.manager.disconnect(&self.session_id);
        }
    }
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        self.fire();
    }
}

#[derive(serde::Deserialize)]
pub struct MessagesQuery {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// `POST /messages?sessionId=<id>`: ingest one client JSON-RPC message
/// (§10, §11). Responds 202 Accepted; unknown sessions get 404 (§17).
async fn messages_handler(
    State(manager): State<Arc<LegacySseManager>>,
    Query(query): Query<MessagesQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
    {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Content-Type must be application/json")
            .into_response();
    }
    let message: ClientJsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON-RPC message: {error}"),
            )
                .into_response();
        }
    };
    match manager.deliver(&query.session_id, message).await {
        Some(()) => (StatusCode::ACCEPTED, "Accepted").into_response(),
        None => (StatusCode::NOT_FOUND, "Session not found").into_response(),
    }
}

/// The `/sse` + `/messages` endpoints behind bearer auth and Origin
/// validation (§18, §21).
pub fn router(manager: Arc<LegacySseManager>, shutdown: CancellationToken) -> Router {
    let config = manager.config();
    spawn_sweeper(manager.clone(), shutdown);
    Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(manager)
        .layer(axum::middleware::from_fn_with_state(
            config,
            crate::auth::require_bearer,
        ))
}
