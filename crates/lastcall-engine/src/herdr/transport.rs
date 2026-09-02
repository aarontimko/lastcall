//! Transport (docs/spec/00-spec.md §5.1): newline-JSON over a Unix socket, one request per
//! connection.
//!
//! [`Transport`] is the seam the client is written against, so an in-memory transport
//! (state-machine unit tests under paused time), the mock server over a real socket, and the
//! real herdr socket are interchangeable. [`SocketTransport`] is the production implementation.
//!
//! Rules: `params` is always present (`{}` for `ping`); request lines over 1 MiB are refused
//! client-side (the server would drop the connection with no JSON error); every await has a
//! timeout; an error line instead of the `subscription_started` ack is
//! [`TransportError::SubscribeRefused`] and the connection is closed — there are no partial
//! subscriptions.
//!
//! The async line reader here follows the same shape as herdr's `JsonLineReader`
//! (`tests/api_ping.rs`, Apache-2.0; see NOTICE).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::wire::{
    self, ErrorBody, Event, Request, Response, Subscription, WireError, parse_response, result_type,
};

/// herdr drops request lines over 1 MiB without a JSON error; refuse them before sending.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Errors from the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("cannot connect to {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("socket I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out after {after:?} waiting for {what}")]
    Timeout { what: &'static str, after: Duration },
    #[error("request line is {bytes} bytes, over the {MAX_REQUEST_BYTES}-byte limit")]
    RequestTooLarge { bytes: usize },
    #[error("connection closed before a response line arrived")]
    ClosedBeforeResponse,
    #[error("herdr error {0}")]
    Server(ErrorBody),
    /// An error line arrived instead of the `subscription_started` ack. The connection is
    /// closed; the caller must not expect any event.
    #[error("subscription refused: {0}")]
    SubscribeRefused(ErrorBody),
    #[error("unexpected ack `{0}` instead of subscription_started")]
    UnexpectedAck(String),
    #[error(transparent)]
    Wire(#[from] WireError),
}

impl TransportError {
    /// The server error code, for either error-response variant.
    pub fn code(&self) -> Option<&str> {
        match self {
            TransportError::Server(e) | TransportError::SubscribeRefused(e) => Some(&e.code),
            _ => None,
        }
    }

    /// `pane_not_found` from either error-response variant.
    pub fn is_pane_not_found(&self) -> bool {
        self.code() == Some(wire::error_code::PANE_NOT_FOUND)
    }

    /// A failure of the connection itself (as opposed to a server-side refusal) — the
    /// reconnect path.
    pub fn is_transport_failure(&self) -> bool {
        matches!(
            self,
            TransportError::Connect { .. }
                | TransportError::Io(_)
                | TransportError::Timeout { .. }
                | TransportError::ClosedBeforeResponse
        )
    }
}

/// The seam between the client state machine and the wire.
pub trait Transport: Send + Sync + 'static {
    /// One connection, one line out, one line in, close. Returns the `result` object; an error
    /// envelope becomes [`TransportError::Server`].
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, TransportError>> + Send;

    /// Open a subscription connection: write `events.subscribe`, await the
    /// `subscription_started` ack, then yield event lines through the stream.
    fn subscribe(
        &self,
        subscriptions: Vec<Subscription>,
    ) -> impl Future<Output = Result<EventStream, TransportError>> + Send;

    /// A human label for notices (the socket path in production).
    fn describe(&self) -> String;
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh request id (`lc-<n>`).
pub fn next_request_id() -> String {
    format!("lc-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// Why a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEnd {
    /// The peer closed the connection after the ack (herdr's slow-consumer kill, a restart).
    ClosedByPeer,
    /// An error line arrived mid-stream and the stream is now closed.
    ClosedAfterError(ErrorBody),
}

/// What [`EventStream::next`] yields.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// One event line, parsed (boxed: `Event` carries whole `PaneInfo`s).
    Event(Box<Event>),
    /// The stream ended.
    End(StreamEnd),
}

impl StreamItem {
    /// The event, if this item is one.
    pub fn into_event(self) -> Option<Event> {
        match self {
            StreamItem::Event(event) => Some(*event),
            StreamItem::End(_) => None,
        }
    }
}

/// An acknowledged subscription connection yielding event lines.
///
/// The stream owns the whole connection (both halves); dropping it closes the subscription.
pub struct EventStream {
    reader: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    buf: String,
    ended: Option<StreamEnd>,
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream")
            .field("ended", &self.ended)
            .finish()
    }
}

impl EventStream {
    /// Wrap an acknowledged connection. The ack line must already have been consumed.
    pub fn new(conn: Box<dyn AsyncRead + Send + Unpin>) -> Self {
        Self {
            reader: BufReader::new(conn),
            buf: String::new(),
            ended: None,
        }
    }

    /// Whether the stream has ended, and how.
    pub fn ended(&self) -> Option<&StreamEnd> {
        self.ended.as_ref()
    }

    /// The next raw line (without the newline), or `None` once the peer closed. `timeout`
    /// bounds the wait; `None` waits indefinitely.
    pub async fn next_line(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<String>, TransportError> {
        if self.ended.is_some() {
            return Ok(None);
        }
        loop {
            self.buf.clear();
            let n = read_line_with_timeout(&mut self.reader, &mut self.buf, timeout, "event line")
                .await?;
            if n == 0 {
                self.ended = Some(StreamEnd::ClosedByPeer);
                return Ok(None);
            }
            let line = self.buf.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                continue;
            }
            return Ok(Some(line.to_string()));
        }
    }

    /// The next parsed item. A mid-stream error line ends the stream with
    /// [`StreamEnd::ClosedAfterError`]; a line that is neither an event nor an error is a
    /// [`WireError`].
    pub async fn next(&mut self, timeout: Option<Duration>) -> Result<StreamItem, TransportError> {
        if let Some(end) = &self.ended {
            return Ok(StreamItem::End(end.clone()));
        }
        let Some(line) = self.next_line(timeout).await? else {
            return Ok(StreamItem::End(StreamEnd::ClosedByPeer));
        };
        let raw: Value = serde_json::from_str(&line).map_err(WireError::from)?;
        if raw.get("event").is_some() {
            let event_line: wire::EventLine =
                serde_json::from_value(raw).map_err(WireError::from)?;
            return Ok(StreamItem::Event(Box::new(Event::from_event_line(
                event_line,
            )?)));
        }
        if let Some(error) = raw.get("error") {
            let body: ErrorBody = serde_json::from_value(error.clone()).map_err(WireError::from)?;
            let end = StreamEnd::ClosedAfterError(body);
            self.ended = Some(end.clone());
            return Ok(StreamItem::End(end));
        }
        Err(WireError::NeitherResultNorError.into())
    }
}

async fn read_line_with_timeout<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut String,
    timeout: Option<Duration>,
    what: &'static str,
) -> Result<usize, TransportError> {
    match timeout {
        Some(after) => tokio::time::timeout(after, reader.read_line(buf))
            .await
            .map_err(|_| TransportError::Timeout { what, after })?
            .map_err(TransportError::from),
        None => reader.read_line(buf).await.map_err(TransportError::from),
    }
}

/// Build the request line, enforcing the size limit.
pub fn encode_request(method: &str, params: Value) -> Result<(String, String), TransportError> {
    let id = next_request_id();
    let line = Request::new(id.clone(), method, params)
        .to_line()
        .map_err(WireError::from)?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(TransportError::RequestTooLarge { bytes: line.len() });
    }
    Ok((id, line))
}

/// Interpret the ack line of `events.subscribe`.
pub fn interpret_ack(line: &str) -> Result<(), TransportError> {
    match parse_response(line)? {
        (_, Response::Error(body)) => Err(TransportError::SubscribeRefused(body)),
        (_, Response::Result(result)) => match result_type(&result) {
            Some(wire::SUBSCRIPTION_STARTED) => Ok(()),
            other => Err(TransportError::UnexpectedAck(
                other.unwrap_or("<none>").to_string(),
            )),
        },
    }
}

/// Interpret a one-shot response line.
pub fn interpret_response(line: &str) -> Result<Value, TransportError> {
    match parse_response(line)? {
        (_, Response::Error(body)) => Err(TransportError::Server(body)),
        (_, Response::Result(result)) => Ok(result),
    }
}

/// The production transport: a Unix socket path plus a per-await timeout.
#[derive(Debug, Clone)]
pub struct SocketTransport {
    path: PathBuf,
    timeout: Duration,
}

impl SocketTransport {
    pub fn new(path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            path: path.into(),
            timeout,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn connect(&self) -> Result<UnixStream, TransportError> {
        let connect = UnixStream::connect(&self.path);
        let stream = tokio::time::timeout(self.timeout, connect)
            .await
            .map_err(|_| TransportError::Timeout {
                what: "connect",
                after: self.timeout,
            })?
            .map_err(|source| TransportError::Connect {
                path: self.path.clone(),
                source,
            })?;
        Ok(stream)
    }

    async fn write_line(&self, stream: &mut UnixStream, line: &str) -> Result<(), TransportError> {
        tokio::time::timeout(self.timeout, async {
            stream.write_all(line.as_bytes()).await?;
            stream.flush().await
        })
        .await
        .map_err(|_| TransportError::Timeout {
            what: "write",
            after: self.timeout,
        })??;
        Ok(())
    }
}

impl Transport for SocketTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        let (_, line) = encode_request(method, params)?;
        let mut stream = self.connect().await?;
        self.write_line(&mut stream, &line).await?;
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let n =
            read_line_with_timeout(&mut reader, &mut buf, Some(self.timeout), "response").await?;
        if n == 0 {
            return Err(TransportError::ClosedBeforeResponse);
        }
        interpret_response(buf.trim_end())
    }

    async fn subscribe(
        &self,
        subscriptions: Vec<Subscription>,
    ) -> Result<EventStream, TransportError> {
        let params = serde_json::to_value(wire::EventsSubscribeParams { subscriptions })
            .map_err(WireError::from)?;
        let (_, line) = encode_request(wire::method::EVENTS_SUBSCRIBE, params)?;
        let mut stream = self.connect().await?;
        self.write_line(&mut stream, &line).await?;
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let n = read_line_with_timeout(
            &mut reader,
            &mut buf,
            Some(self.timeout),
            "subscription ack",
        )
        .await?;
        if n == 0 {
            return Err(TransportError::ClosedBeforeResponse);
        }
        interpret_ack(buf.trim_end())?;
        // Keep the whole connection alive inside the stream: the already-buffered reader owns
        // the socket, so nothing read ahead of the ack is lost.
        Ok(EventStream::new(Box::new(reader)))
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// Convenience: `ping` a socket path once with a short timeout (used by discovery).
pub async fn socket_answers_ping(path: &Path, timeout: Duration) -> bool {
    let transport = SocketTransport::new(path, timeout);
    super::guard::ping(&transport).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::mock_herdr::{MockHerdr, ScriptedEvent};
    use lastcall_testkit::tmp::TempDir;

    fn snapshot_fixture() -> Value {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lastcall-testkit/fixtures/herdr/snapshot_two_panes.json"
        ))
        .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn transport_rejects_request_over_one_mib() {
        let big = "x".repeat(MAX_REQUEST_BYTES);
        let err = encode_request("pane.get", serde_json::json!({ "pane_id": big })).unwrap_err();
        assert!(matches!(err, TransportError::RequestTooLarge { .. }));
        assert!(encode_request("ping", serde_json::json!({})).is_ok());
    }

    #[test]
    fn transport_interprets_ack_and_error_lines() {
        assert!(interpret_ack(r#"{"id":"x","result":{"type":"subscription_started"}}"#).is_ok());
        let err = interpret_ack(r#"{"id":"x","error":{"code":"pane_not_found","message":"m"}}"#)
            .unwrap_err();
        assert!(matches!(err, TransportError::SubscribeRefused(_)));
        assert!(err.is_pane_not_found());
        assert!(!err.is_transport_failure());
        let err = interpret_ack(r#"{"id":"x","result":{"type":"ok"}}"#).unwrap_err();
        assert!(matches!(err, TransportError::UnexpectedAck(ref t) if t == "ok"));
        let err =
            interpret_response(r#"{"id":"x","error":{"code":"weird","message":"m"}}"#).unwrap_err();
        assert_eq!(err.code(), Some("weird"));
    }

    // The three transport tests over the real-socket mock run with real time (never
    // `time::pause` over a real socket: the paused clock would auto-advance on the read).

    #[tokio::test]
    async fn transport_one_shot_request_round_trip() {
        let dir = TempDir::socket_dir();
        let sock = dir.join("herdr.sock");
        let mock = MockHerdr::builder()
            .snapshot(snapshot_fixture())
            .serve(&sock)
            .await
            .unwrap();
        let t = SocketTransport::new(&sock, Duration::from_secs(2));

        let pong = super::super::guard::ping(&t).await.unwrap();
        assert_eq!(pong.protocol, 21);
        assert_eq!(pong.version, "0.8.2");

        let result = t
            .request(wire::method::SESSION_SNAPSHOT, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result_type(&result), Some("session_snapshot"));
        let snap: wire::SnapshotResult = serde_json::from_value(result).unwrap();
        assert_eq!(snap.snapshot.panes.len(), 2);

        let err = t
            .request(
                wire::method::PANE_GET,
                serde_json::json!({"pane_id": "nope"}),
            )
            .await
            .unwrap_err();
        assert!(err.is_pane_not_found());

        // Each request was one connection with `params` present.
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 3);
        assert!(reqs.iter().all(|r| r.params.is_object()));
        assert_eq!(reqs[0].method, "ping");
        assert_eq!(reqs[1].method, "session.snapshot");
        assert_eq!(reqs[2].method, "pane.get");
        assert!(socket_answers_ping(&sock, Duration::from_secs(1)).await);
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn transport_subscribe_acks_then_streams() {
        let dir = TempDir::socket_dir();
        let sock = dir.join("herdr.sock");
        let mock = MockHerdr::builder()
            .snapshot(snapshot_fixture())
            .lifecycle_events(vec![
                ScriptedEvent::after_ms(
                    0,
                    r#"{"event":"tab_focused","data":{"type":"tab_focused","tab_id":"w:t1","workspace_id":"w"}}"#,
                ),
                ScriptedEvent::after_ms(
                    10,
                    r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w:p1","workspace_id":"w","agent_status":"done","agent":"demo"}}"#,
                ),
            ])
            .close_lifecycle_after(2)
            .serve(&sock)
            .await
            .unwrap();
        let t = SocketTransport::new(&sock, Duration::from_secs(2));
        let mut stream = t.subscribe(wire::lifecycle_subscriptions()).await.unwrap();
        let first = stream
            .next(Some(Duration::from_secs(2)))
            .await
            .unwrap()
            .into_event()
            .unwrap();
        assert!(matches!(first, Event::TabFocused { ref tab_id, .. } if tab_id == "w:t1"));
        let second = stream
            .next(Some(Duration::from_secs(2)))
            .await
            .unwrap()
            .into_event()
            .unwrap();
        assert!(matches!(
            second,
            Event::PaneAgentStatusChanged(ref e) if e.agent_status == wire::AgentStatus::Done
        ));
        let end = stream.next(Some(Duration::from_secs(2))).await.unwrap();
        assert_eq!(end, StreamItem::End(StreamEnd::ClosedByPeer));
        assert_eq!(stream.ended(), Some(&StreamEnd::ClosedByPeer));
        // Idempotent after the end.
        assert_eq!(
            stream.next(Some(Duration::from_millis(10))).await.unwrap(),
            StreamItem::End(StreamEnd::ClosedByPeer)
        );
        let subs = mock.subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].len(), 15);
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn transport_subscribe_error_then_close() {
        let dir = TempDir::socket_dir();
        let sock = dir.join("herdr.sock");
        let mock = MockHerdr::builder()
            .snapshot(snapshot_fixture())
            .refuse_lifecycle_subscription("invalid_params", "unknown subscription type")
            .serve(&sock)
            .await
            .unwrap();
        let t = SocketTransport::new(&sock, Duration::from_secs(2));
        let err = t
            .subscribe(wire::lifecycle_subscriptions())
            .await
            .unwrap_err();
        match err {
            TransportError::SubscribeRefused(body) => {
                assert_eq!(body.code, "invalid_params");
            }
            other => panic!("{other:?}"),
        }
        // A per-pane subscription for an unknown pane: pane_not_found, then close.
        let err = t
            .subscribe(vec![Subscription::pane_agent_status_changed(
                "no-such-pane",
            )])
            .await
            .unwrap_err();
        assert!(err.is_pane_not_found());
        assert!(matches!(err, TransportError::SubscribeRefused(_)));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn transport_connect_failure_is_a_transport_failure() {
        let dir = TempDir::socket_dir();
        let t = SocketTransport::new(dir.join("absent.sock"), Duration::from_millis(50));
        let err = t.request("ping", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, TransportError::Connect { .. }));
        assert!(err.is_transport_failure());
        assert!(!socket_answers_ping(&dir.join("absent.sock"), Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn transport_stall_times_out_and_closed_before_response_is_reported() {
        let dir = TempDir::socket_dir();
        let sock = dir.join("herdr.sock");
        let mock = MockHerdr::builder()
            .snapshot(snapshot_fixture())
            .stall()
            .serve(&sock)
            .await
            .unwrap();
        // The only real-time wait in the unit tier; kept under the 50 ms budget.
        let t = SocketTransport::new(&sock, Duration::from_millis(40));
        let err = t.request("ping", serde_json::json!({})).await.unwrap_err();
        assert!(
            matches!(
                err,
                TransportError::Timeout {
                    what: "response",
                    ..
                }
            ),
            "{err:?}"
        );
        mock.shutdown().await;

        let mock = MockHerdr::builder()
            .snapshot(snapshot_fixture())
            .close_without_response()
            .serve(&sock)
            .await
            .unwrap();
        let err = t.request("ping", serde_json::json!({})).await.unwrap_err();
        assert!(
            matches!(err, TransportError::ClosedBeforeResponse),
            "{err:?}"
        );
        mock.shutdown().await;
    }
}
