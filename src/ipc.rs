//! GlazeWM WebSocket IPC client: connection, initial sync, subscription,
//! commands, calibration queries and exponential backoff reconnection.
//!
//! Runs on a dedicated thread with its own Tokio runtime. State snapshots are
//! pushed to the UI thread through a channel; the UI coalesces them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::config::Config;
use crate::protocol::{WsResponse, error_message};
use crate::reducer::{QueryKind, Reducer, ReducerInput};
use crate::state::{AppSnapshot, ConnectionState};

/// Messages from the UI thread to the IPC task.
#[derive(Debug)]
pub enum UiToIpc {
    /// Run a raw GlazeWM command (already encoded).
    Command { id: u64, text: String },
    /// Re-run the focused/tiling-direction calibration queries.
    Calibrate,
    /// Force an immediate reconnect (resets backoff).
    Reconnect,
    /// Shut the task down.
    Shutdown,
}

/// Shared mailbox between the IPC task and the UI thread.
///
/// - Snapshots are latest-wins: only the most recent one is kept, so a slow
///   UI can never cause the IPC task to busy-loop or drop newer state.
/// - Command results are queued and never dropped (they are rare, and losing
///   one would make the UI report a spurious command failure).
#[derive(Default)]
pub struct IpcToUiState {
    pub latest: Option<Arc<AppSnapshot>>,
    pub results: VecDeque<(u64, bool, Option<String>)>,
}

pub type SharedIpcToUi = Arc<Mutex<IpcToUiState>>;

/// Connection settings shared with the app for hot reload.
pub type SharedGlazeWmConfig = Arc<RwLock<crate::config::GlazeWmConfig>>;

/// Wake-up token for the UI thread (a `PostMessage` to its message window).
#[derive(Clone)]
pub struct IpcNotify {
    hwnd: isize,
    msg: u32,
}

unsafe impl Send for IpcNotify {}
unsafe impl Sync for IpcNotify {}

impl IpcNotify {
    pub fn new(hwnd: isize, msg: u32) -> Self {
        Self { hwnd, msg }
    }

    pub fn poke(&self) {
        if self.hwnd != 0 {
            let hwnd = windows::Win32::Foundation::HWND(self.hwnd as *mut core::ffi::c_void);
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(hwnd),
                    self.msg,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
                .ok();
            }
        }
    }
}

/// Handle for the UI thread to talk to the IPC task.
pub struct IpcHandle {
    pub tx: mpsc::Sender<UiToIpc>,
    pub shared: SharedIpcToUi,
    glazewm_cfg: SharedGlazeWmConfig,
}

impl IpcHandle {
    /// Access to the shared GlazeWM connection settings (for hot reload).
    pub fn glazewm_cfg(&self) -> Option<SharedGlazeWmConfig> {
        Some(self.glazewm_cfg.clone())
    }
}

/// Spawn the IPC thread. The returned handle is used from the UI thread.
pub fn spawn(config: Arc<Config>, notify: IpcNotify) -> IpcHandle {
    let shared: SharedIpcToUi = Arc::new(Mutex::new(IpcToUiState::default()));
    // The GlazeWM connection settings live behind a shared lock so config
    // hot-reload can update them without restarting the IPC task.
    let glazewm_cfg: SharedGlazeWmConfig = Arc::new(RwLock::new(config.glazewm.clone()));
    let (ipc_tx, ipc_rx) = mpsc::channel::<UiToIpc>(64);
    let shared_task = shared.clone();
    let glazewm_task = glazewm_cfg.clone();
    std::thread::Builder::new()
        .name("glazetray-ipc".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "failed to build tokio runtime");
                    return;
                }
            };
            rt.block_on(run(glazewm_task, ipc_rx, shared_task, notify));
        })
        .expect("failed to spawn ipc thread");
    IpcHandle {
        tx: ipc_tx,
        shared,
        glazewm_cfg,
    }
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

struct Backoff {
    initial: Duration,
    max: Duration,
    attempt: u32,
    state: u64,
}

impl Backoff {
    fn new(initial_ms: u64, max_ms: u64) -> Self {
        Self {
            initial: Duration::from_millis(initial_ms.max(50)),
            max: Duration::from_millis(max_ms.max(initial_ms)),
            attempt: 0,
            state: 0x9E3779B97F4A7C15,
        }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Next delay with ±20% jitter, capped at `max`.
    fn next(&mut self) -> Duration {
        let base = self.initial.saturating_mul(1u32 << self.attempt.min(6));
        let base = base.min(self.max);
        self.attempt += 1;
        let ms = base.as_millis() as u64;
        let jitter = (self.next_u32() as u64 % (ms / 5 + 1)) as i64 - (ms / 10) as i64;
        let ms = (ms as i64 + jitter).max(10) as u64;
        Duration::from_millis(ms.min(self.max.as_millis() as u64))
    }

    fn next_u32(&mut self) -> u32 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        (x.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u32
    }
}

// ---------------------------------------------------------------------------
// Protocol flow
// ---------------------------------------------------------------------------

const SUBSCRIBE_EVENTS: &[&str] = &[
    "focus_changed",
    "focused_container_moved",
    "monitor_added",
    "monitor_updated",
    "monitor_removed",
    "tiling_direction_changed",
    "window_managed",
    "window_unmanaged",
    "workspace_activated",
    "workspace_deactivated",
    "workspace_updated",
    "pause_changed",
    "user_config_changed",
    "application_exiting",
];
/// Events after which a calibration query is warranted because the reducer
/// cannot always self-reconcile them.
const CALIBRATE_ON_EVENTS: &[&str] = &[
    "window_managed",
    "window_unmanaged",
    "focused_container_moved",
    "user_config_changed",
    "application_exiting",
];

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
enum Expect {
    Subscribe,
    Metadata,
    Query(QueryKind),
    Command { id: u64 },
}

fn is_loopback_url(url: &str) -> bool {
    let Ok(request) = url.into_client_request() else {
        return false;
    };
    let scheme = request.uri().scheme_str().unwrap_or("");
    if scheme != "ws" && scheme != "wss" {
        return false;
    }
    match request.uri().host() {
        Some(host) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "127.0.0.1" || host == "localhost" || host == "::1"
        }
        None => false,
    }
}

async fn run(
    glazewm_cfg: SharedGlazeWmConfig,
    mut rx: mpsc::Receiver<UiToIpc>,
    shared: SharedIpcToUi,
    notify: IpcNotify,
) {
    let mut reducer = Reducer::new();
    let read_cfg = || -> crate::config::GlazeWmConfig {
        glazewm_cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    let mut last_url: Option<String> = None;
    let mut backoff_params = (250u64, 10_000u64);
    let mut backoff = Backoff::new(backoff_params.0, backoff_params.1);
    let mut shutdown = false;

    while !shutdown {
        // ---------------- disconnected phase ----------------
        let cfg = read_cfg();
        // Recreate the backoff only when the configured parameters changed
        // (recreating it every iteration would reset the exponential growth).
        let params = (cfg.reconnect_initial_ms, cfg.reconnect_max_ms);
        if params != backoff_params {
            backoff_params = params;
            backoff = Backoff::new(params.0, params.1);
        }
        reducer.set_connection(ConnectionState::Connecting {
            attempt: backoff.attempt,
        });
        publish(&shared, &reducer, &notify);

        let delay = backoff.next();
        let mut proceed = false;
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(UiToIpc::Reconnect) => { backoff.reset(); proceed = true; }
                Some(UiToIpc::Shutdown) => shutdown = true,
                Some(UiToIpc::Command { id, .. }) => {
                    push_result(&shared, id, false, Some("尚未连接 GlazeWM".into()), &notify);
                }
                Some(UiToIpc::Calibrate) => {}
                None => shutdown = true,
            },
            _ = tokio::time::sleep(delay) => proceed = true,
        }
        if shutdown {
            break;
        }
        if !proceed {
            continue;
        }

        let cfg = read_cfg();
        let url = cfg.url.clone();
        // The URL changed since the last successful connection: reset the
        // backoff so we attempt the new endpoint immediately.
        if last_url.as_deref() != Some(url.as_str()) {
            backoff.reset();
            last_url = Some(url.clone());
        }
        let mut ws = match connect(&url).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::debug!(error = %e, "connection failed, retrying");
                if !is_loopback_url(&url) {
                    reducer.set_connection(ConnectionState::Degraded {
                        reason: "配置的 GlazeWM 地址不是本机地址，已拒绝连接".into(),
                    });
                } else {
                    reducer.set_connection(ConnectionState::Disconnected);
                }
                publish(&shared, &reducer, &notify);
                continue;
            }
        };
        tracing::info!("connected to GlazeWM at {url}");
        reducer.set_connection(ConnectionState::Synchronizing);
        publish(&shared, &reducer, &notify);
        backoff.reset();

        // ---------------- handshake ----------------
        let mut expects: VecDeque<Expect> = VecDeque::new();
        // GlazeWM 3.10: `sub -e <events>` (subscriptions with explicit event
        // lists; `all` subscribes to everything).
        let sub = format!("sub -e {}", SUBSCRIBE_EVENTS.join(" "));
        if ws.send(Message::Text(sub.into())).await.is_ok() {
            expects.push_back(Expect::Subscribe);
        }
        send_query(
            &mut ws,
            "query app-metadata",
            Expect::Metadata,
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query monitors",
            Expect::Query(QueryKind::Monitors),
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query workspaces",
            Expect::Query(QueryKind::Workspaces),
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query focused",
            Expect::Query(QueryKind::Focused),
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query tiling-direction",
            Expect::Query(QueryKind::TilingDirection),
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query paused",
            Expect::Query(QueryKind::Paused),
            &mut expects,
        )
        .await;
        // Calibration pass covering changes during the sync window.
        send_query(
            &mut ws,
            "query focused",
            Expect::Query(QueryKind::Focused),
            &mut expects,
        )
        .await;
        send_query(
            &mut ws,
            "query tiling-direction",
            Expect::Query(QueryKind::TilingDirection),
            &mut expects,
        )
        .await;

        let sync_deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(sync_deadline);
        while !expects.is_empty() {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(UiToIpc::Reconnect) | Some(UiToIpc::Shutdown) => break,
                        Some(UiToIpc::Calibrate) => {}
                        Some(UiToIpc::Command { id, text }) => {
                            if ws.send(Message::Text(text.into())).await.is_ok() {
                                expects.push_back(Expect::Command { id });
                            }
                        }
                        None => break,
                    }
                }
                frame = ws.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if !handle_frame(
                                &text, &mut ws, &mut reducer, &mut expects, &shared, &notify,
                            ).await {
                                break; // connection error
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "websocket error during sync");
                            break;
                        }
                        None => break,
                    }
                }
                _ = &mut sync_deadline => {
                    tracing::warn!("initial sync timed out");
                    break;
                }
            }
        }
        if !expects.is_empty() {
            // sync was interrupted (disconnect / timeout)
            ws.close(None).await.ok();
            reducer.set_connection(ConnectionState::Disconnected);
            publish(&shared, &reducer, &notify);
            continue;
        }

        reducer.mark_ready();
        publish(&shared, &reducer, &notify);
        tracing::info!("initial sync complete");

        // ---------------- steady state ----------------
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(UiToIpc::Command { id, text }) => {
                            if let Err(e) = ws.send(Message::Text(text.into())).await {
                                tracing::warn!(error = %e, "failed to send command");
                                push_result(
                                    &shared,
                                    id,
                                    false,
                                    Some("发送命令失败".into()),
                                    &notify,
                                );
                            } else {
                                expects.push_back(Expect::Command { id });
                            }
                        }
                        Some(UiToIpc::Calibrate) => {
                            send_query(&mut ws, "query focused", Expect::Query(QueryKind::Focused), &mut expects).await;
                            send_query(&mut ws, "query tiling-direction", Expect::Query(QueryKind::TilingDirection), &mut expects).await;
                            send_query(&mut ws, "query paused", Expect::Query(QueryKind::Paused), &mut expects).await;
                        }
                        Some(UiToIpc::Reconnect) => break,
                        Some(UiToIpc::Shutdown) => { shutdown = true; break; }
                        None => { shutdown = true; break; }
                    }
                }
                frame = ws.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if !handle_frame(
                                &text, &mut ws, &mut reducer, &mut expects, &shared, &notify,
                            ).await {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "websocket error");
                            break;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if !expects.is_empty() && last_response_expired() {
                        tracing::warn!("no response for 5s; forcing resync");
                        break;
                    }
                    // Config hot reload changed the endpoint: reconnect.
                    let current_url = read_cfg().url;
                    if last_url.as_deref() != Some(current_url.as_str()) {
                        tracing::info!(
                            url = %current_url,
                            "glazewm url changed; reconnecting"
                        );
                        break;
                    }
                }
            }
            if shutdown {
                break;
            }
        }
        if shutdown {
            break;
        }
        tracing::warn!("disconnected from GlazeWM");
        ws.close(None).await.ok();
        reducer.set_connection(ConnectionState::Disconnected);
        publish(&shared, &reducer, &notify);
    }

    tracing::info!("ipc task shutting down");
    publish(&shared, &reducer, &notify);
}

static LAST_RESPONSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn last_response_expired() -> bool {
    let last = LAST_RESPONSE.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    now.saturating_sub(last) > 5000
}

async fn send_query(ws: &mut Ws, query: &str, expect: Expect, expects: &mut VecDeque<Expect>) {
    if ws
        .send(Message::Text(query.to_string().into()))
        .await
        .is_ok()
    {
        expects.push_back(expect);
    }
}

/// Handles one incoming text frame. Returns false when the connection is
/// unusable and the caller should reconnect.
async fn handle_frame(
    text: &str,
    ws: &mut Ws,
    reducer: &mut Reducer,
    expects: &mut VecDeque<Expect>,
    shared: &SharedIpcToUi,
    notify: &IpcNotify,
) -> bool {
    LAST_RESPONSE.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, raw = %text.chars().take(200).collect::<String>(), "invalid JSON from GlazeWM");
            return true;
        }
    };

    // Subscription event?
    if let Some(name) = parse_event_name(&v) {
        let data = v.get("data").cloned();
        reducer.apply(ReducerInput::Event {
            name: name.clone(),
            data,
        });
        if !SUBSCRIBE_EVENTS.contains(&name.as_str()) {
            tracing::warn!(event = name, "unknown event; issuing calibration");
            calibrate(ws, expects).await;
        } else if CALIBRATE_ON_EVENTS.contains(&name.as_str()) {
            calibrate(ws, expects).await;
        }
        publish(shared, reducer, notify);
        return true;
    }

    // Response to a query/command.
    let resp: WsResponse = match serde_json::from_value(v) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "unparseable response");
            return true;
        }
    };

    match expects.pop_front() {
        Some(Expect::Subscribe) => {
            if !resp.success.unwrap_or(false) {
                tracing::warn!("explicit subscription rejected; falling back to `sub -e all`");
                if ws
                    .send(Message::Text("sub -e all".to_string().into()))
                    .await
                    .is_ok()
                {
                    expects.push_back(Expect::Subscribe);
                }
            }
        }
        Some(Expect::Metadata) => {
            if let Some(v) = &resp.data {
                let version = v
                    .get("version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| v.as_str().map(|s| s.to_string()));
                if let Some(ver) = version {
                    reducer.apply(ReducerInput::Version { version: ver });
                }
            }
        }
        Some(Expect::Query(kind)) => {
            reducer.apply(ReducerInput::Query {
                kind,
                data: resp.data,
            });
        }
        Some(Expect::Command { id }) => {
            let success = resp.success.unwrap_or(false);
            let message = error_message(&resp.error);
            push_result(shared, id, success, message, notify);
        }
        None => {
            tracing::warn!(raw = %text.chars().take(200).collect::<String>(), "unexpected response");
        }
    }
    publish(shared, reducer, notify);
    true
}

/// Detect whether `v` is a subscription event and return its name.
fn parse_event_name(v: &Value) -> Option<String> {
    // GlazeWM 3.10: `{ "messageType": "event_subscription", "data": { "eventType": ... } }`
    if v.get("messageType").and_then(|m| m.as_str()) == Some("event_subscription")
        && let Some(name) = v
            .get("data")
            .and_then(|d| d.get("eventType"))
            .and_then(|e| e.as_str())
    {
        return Some(name.to_string());
    }
    // Legacy: `{ "event": ... }`
    v.get("event")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

async fn calibrate(ws: &mut Ws, expects: &mut VecDeque<Expect>) {
    send_query(
        ws,
        "query focused",
        Expect::Query(QueryKind::Focused),
        expects,
    )
    .await;
    send_query(
        ws,
        "query tiling-direction",
        Expect::Query(QueryKind::TilingDirection),
        expects,
    )
    .await;
    send_query(
        ws,
        "query paused",
        Expect::Query(QueryKind::Paused),
        expects,
    )
    .await;
}

fn publish(shared: &SharedIpcToUi, reducer: &Reducer, notify: &IpcNotify) {
    // Latest-wins: overwrite the previous snapshot. Never busy-loops.
    if let Ok(mut st) = shared.lock() {
        st.latest = Some(Arc::new(reducer.snapshot()));
    }
    notify.poke();
}

/// Queue a command result without ever dropping it.
fn push_result(
    shared: &SharedIpcToUi,
    id: u64,
    success: bool,
    message: Option<String>,
    notify: &IpcNotify,
) {
    if let Ok(mut st) = shared.lock() {
        st.results.push_back((id, success, message));
    }
    notify.poke();
}

async fn connect(url: &str) -> anyhow::Result<Ws> {
    if !is_loopback_url(url) {
        anyhow::bail!("非 loopback 地址拒绝连接: {url}（仅允许 ws://127.0.0.1 等本机地址）");
    }
    let request = url
        .into_client_request()
        .map_err(|e| anyhow::anyhow!("invalid websocket url: {e}"))?;
    let (ws, _) = connect_async(request)
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
    Ok(ws)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_bounds_and_reset() {
        let mut b = Backoff::new(250, 10_000);
        let mut prev = Duration::ZERO;
        for _ in 0..12 {
            let d = b.next();
            assert!(d >= Duration::from_millis(10));
            assert!(d <= Duration::from_millis(10_000));
            assert!(d >= prev / 3, "delay should not shrink drastically");
            prev = d;
        }
        assert!(
            prev >= Duration::from_millis(9_000),
            "should approach max: {prev:?}"
        );
        b.reset();
        assert_eq!(b.attempt, 0);
        let d = b.next();
        assert!(d <= Duration::from_millis(300), "first delay ~250ms: {d:?}");
    }

    #[test]
    fn backoff_jitter_variety() {
        let mut b = Backoff::new(250, 1000);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            seen.insert(b.next());
        }
        assert!(
            seen.len() > 5,
            "jitter too weak: {} distinct delays",
            seen.len()
        );
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_url("ws://127.0.0.1:6123"));
        assert!(is_loopback_url("ws://localhost:6123"));
        assert!(is_loopback_url("ws://[::1]:6123"));
        assert!(!is_loopback_url("ws://::1:6123")); // invalid URL syntax
        assert!(!is_loopback_url("ws://192.168.1.5:6123"));
        assert!(!is_loopback_url("ws://glazewm.example:6123"));
        assert!(!is_loopback_url("http://127.0.0.1:6123"));
    }
}

// ---------------------------------------------------------------------------
// IPC integration test: mock GlazeWM WebSocket server
// ---------------------------------------------------------------------------

#[cfg(test)]
mod integration {
    use super::*;
    use crate::config::{
        Config, FlyoutConfig, GlazeWmConfig, LoggingConfig, StartupConfig, TrayConfig,
    };
    use crate::state::TilingDirection;
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn mock_config(url: String) -> Arc<Config> {
        Arc::new(Config {
            glazewm: GlazeWmConfig {
                url,
                reconnect_initial_ms: 50,
                reconnect_max_ms: 200,
            },
            flyout: FlyoutConfig::default(),
            startup: StartupConfig::default(),
            logging: LoggingConfig::default(),
            tray: TrayConfig::default(),
        })
    }

    /// A scripted GlazeWM server. Responds to queries and, on the focus
    /// command for workspace "2", emits a focus_changed event.
    async fn run_mock_server(
        listener: tokio::net::TcpListener,
        on_focus: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    ) -> Vec<String> {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws accept");
        let mut received = Vec::new();
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => {
                    let text = t.to_string();
                    received.push(text.clone());
                    let resp = if text.starts_with("sub -e") {
                        r#"{"messageType":"client_response","clientMessage":"sub -e all","success":true,"data":{"subscriptionId":"s1"},"error":null}"#
                            .to_string()
                    } else if text.starts_with("query app-metadata") {
                        r#"{"messageType":"client_response","success":true,"data":{"version":"3.9.0"},"error":null}"#.into()
                    } else if text.starts_with("query monitors") {
                        r#"{"messageType":"client_response","success":true,"data":{"monitors":[{"type":"monitor","id":"m0","x":0,"y":0,"width":1920,"height":1080,"deviceName":"\\\\\\.\\DISPLAY1","children":[]}]},"error":null}"#.into()
                    } else if text.starts_with("query workspaces") {
                        r#"{"messageType":"client_response","success":true,"data":{"workspaces":[{"type":"workspace","id":"w1","name":"1","parentId":"m0","isDisplayed":true,"hasFocus":true,"tilingDirection":"horizontal","x":0,"y":40,"width":1920,"height":1040,"children":[]}]},"error":null}"#.into()
                    } else if text.starts_with("query focused") {
                        r#"{"messageType":"client_response","success":true,"data":{"focused":{"type":"workspace","id":"w1"}},"error":null}"#.into()
                    } else if text.starts_with("query tiling-direction") {
                        r#"{"messageType":"client_response","success":true,"data":{"tilingDirection":"horizontal","directionContainer":{"type":"workspace","id":"w1"}},"error":null}"#.into()
                    } else if text.starts_with("query paused") {
                        r#"{"messageType":"client_response","success":true,"data":false,"error":null}"#.into()
                    } else if text.starts_with("command focus --workspace 2") {
                        if let Some(tx) = &on_focus {
                            let _ = tx.send(());
                        }
                        ws.send(WsMessage::Text(
                            r#"{"messageType":"client_response","success":true,"data":null,"error":null}"#.into(),
                        ))
                        .await
                        .unwrap();
                        ws.send(WsMessage::Text(
                            r#"{"messageType":"event_subscription","data":{"eventType":"focus_changed","focusedContainer":{"type":"workspace","id":"w2"}}}"#.into(),
                        ))
                        .await
                        .unwrap();
                        continue;
                    } else if text.starts_with("command") {
                        r#"{"messageType":"client_response","success":true,"data":null,"error":null}"#.into()
                    } else {
                        r#"{"success":false,"data":null,"error":"unknown command"}"#.into()
                    };
                    ws.send(WsMessage::Text(resp.into())).await.unwrap();
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
        received
    }

    fn spawn_client(cfg: Arc<Config>) -> (mpsc::Sender<UiToIpc>, SharedIpcToUi) {
        let shared: SharedIpcToUi = Arc::new(Mutex::new(IpcToUiState::default()));
        let glazewm_cfg: SharedGlazeWmConfig = Arc::new(RwLock::new(cfg.glazewm.clone()));
        let (ipc_tx, ipc_rx) = mpsc::channel::<UiToIpc>(64);
        let shared_task = shared.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rt");
            rt.block_on(run(glazewm_cfg, ipc_rx, shared_task, IpcNotify::new(0, 0)));
        });
        (ipc_tx, shared)
    }

    fn wait_ready(shared: &SharedIpcToUi) -> crate::state::AppSnapshot {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for Ready snapshot"
            );
            if let Some(s) = shared.lock().unwrap().latest.clone()
                && s.connection == crate::state::ConnectionState::Ready
            {
                return (*s).clone();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Poll until a snapshot satisfies `pred`.
    fn wait_snapshot(
        shared: &SharedIpcToUi,
        pred: impl Fn(&crate::state::AppSnapshot) -> bool,
    ) -> crate::state::AppSnapshot {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "snapshot condition not met"
            );
            if let Some(s) = shared.lock().unwrap().latest.clone()
                && pred(&s)
            {
                return (*s).clone();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Poll until a command result with `id` is present (and consume it).
    fn wait_result(shared: &SharedIpcToUi, id: u64) -> (bool, Option<String>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "command result {id} not received"
            );
            let mut st = shared.lock().unwrap();
            if let Some(pos) = st.results.iter().position(|(rid, _, _)| *rid == id) {
                let (_, success, message) = st.results.remove(pos).unwrap();
                return (success, message);
            }
            drop(st);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_and_command_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (focus_tx, mut focus_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let server = tokio::spawn(run_mock_server(listener, Some(focus_tx)));

        let cfg = mock_config(format!("ws://{addr}"));
        let (ipc_tx, shared) = spawn_client(cfg);

        // 1. Initial sync.
        let snap = wait_ready(&shared);
        assert_eq!(snap.monitors.len(), 1);
        assert_eq!(snap.focused_workspace_id.as_deref(), Some("w1"));
        assert_eq!(snap.focused_direction, Some(TilingDirection::Horizontal));
        assert_eq!(snap.glazewm_version.as_deref(), Some("3.9.0"));
        assert!(snap.monitors[0].workspaces.iter().any(|w| w.id == "w1"));

        // 2. Command → CommandResult + event-driven snapshot update.
        ipc_tx
            .try_send(UiToIpc::Command {
                id: 42,
                text: "command focus --workspace 2".into(),
            })
            .unwrap();

        let (success, _) = wait_result(&shared, 42);
        assert!(success);
        // Event must have arrived and updated the snapshot.
        let s = wait_snapshot(&shared, |s| s.focused_workspace_id.as_deref() == Some("w2"));
        assert_eq!(s.focused_workspace_id.as_deref(), Some("w2"));
        assert!(
            focus_rx.try_recv().is_ok(),
            "server should have received the focus command"
        );
        drop(ipc_tx);
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnects_after_server_restart() {
        // A single listener serves two "GlazeWM instances" back to back:
        // connection 1 syncs state A then drops; connection 2 syncs state B.
        // (Windows TIME_WAIT prevents rebinding the same port quickly, so the
        // listener stays alive — this mirrors a real GlazeWM restart.)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut served = 0u32;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                served += 1;
                let (version, ws_id, ws_name, dir, mw, mh) = if served == 1 {
                    ("1.0.0", "w1", "1", "vertical", 1920, 1080)
                } else {
                    ("2.0.0", "w9", "9", "horizontal", 2560, 1440)
                };
                let mut requests = 0u32;
                while let Some(Ok(WsMessage::Text(t))) = ws.next().await {
                    let text = t.to_string();
                    requests += 1;
                    let resp = if text.starts_with("sub -e") {
                        r#"{"success":true,"data":{"subscriptionId":"s1"},"error":null}"#
                            .to_string()
                    } else if text.starts_with("query app-metadata") {
                        format!(
                            r#"{{"success":true,"data":{{"version":"{version}"}},"error":null}}"#
                        )
                    } else if text.starts_with("query monitors") {
                        format!(
                            r#"{{"success":true,"data":{{"monitors":[{{"type":"monitor","id":"m{served}","x":0,"y":0,"width":{mw},"height":{mh},"children":[]}}]}},"error":null}}"#
                        )
                    } else if text.starts_with("query workspaces") {
                        format!(
                            r#"{{"success":true,"data":{{"workspaces":[{{"type":"workspace","id":"{ws_id}","name":"{ws_name}","parentId":"m{served}","isDisplayed":true,"hasFocus":true,"tilingDirection":"{dir}","x":0,"y":40,"width":{mw},"height":{mh},"children":[]}}]}},"error":null}}"#
                        )
                    } else if text.starts_with("query focused") {
                        format!(
                            r#"{{"success":true,"data":{{"focused":{{"type":"workspace","id":"{ws_id}"}}}},"error":null}}"#
                        )
                    } else if text.starts_with("query tiling-direction") {
                        format!(
                            r#"{{"success":true,"data":{{"tilingDirection":"{dir}","directionContainer":{{"type":"workspace","id":"{ws_id}"}}}},"error":null}}"#
                        )
                    } else if text.starts_with("query paused") {
                        r#"{"success":true,"data":false,"error":null}"#.into()
                    } else {
                        r#"{"success":true,"data":null,"error":null}"#.into()
                    };
                    ws.send(WsMessage::Text(resp.into())).await.unwrap();
                    // Simulate a GlazeWM restart: drop the first connection
                    // once the initial sync (9 requests) is complete. Keep
                    // state A observable long enough for the test to see it.
                    if served == 1 && requests >= 9 {
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        break;
                    }
                }
            }
        });

        let cfg = mock_config(format!("ws://{addr}"));
        let (ipc_tx, shared) = spawn_client(cfg);

        // Connection 1: state A.
        let snap = wait_ready(&shared);
        assert_eq!(snap.focused_direction, Some(TilingDirection::Vertical));
        assert_eq!(snap.focused_workspace_id.as_deref(), Some("w1"));

        // The old state must be replaced atomically by the new sync.
        let s = wait_snapshot(&shared, |s| {
            s.connection == crate::state::ConnectionState::Ready
                && s.focused_workspace_id.as_deref() == Some("w9")
        });
        assert_eq!(s.monitors.len(), 1);
        assert_eq!(s.monitors[0].id, "m2");
        assert_eq!(s.focused_direction, Some(TilingDirection::Horizontal));
        assert_eq!(s.glazewm_version.as_deref(), Some("2.0.0"));
        drop(ipc_tx);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_fails_when_disconnected() {
        let cfg = mock_config("ws://127.0.0.1:1".into()); // nothing listens here
        let (ipc_tx, shared) = spawn_client(cfg);
        ipc_tx
            .try_send(UiToIpc::Command {
                id: 7,
                text: "command focus --workspace 1".into(),
            })
            .unwrap();
        let (success, _) = wait_result(&shared, 7);
        assert!(!success);
        drop(ipc_tx);
    }
}

/// Producer-side tests for the shared mailbox: no busy loop under a slow
/// consumer, and command results are never dropped.
#[cfg(test)]
mod mailbox_tests {
    use super::*;
    use crate::state::ConnectionState;

    fn fake_snapshot(rev: u64) -> Arc<AppSnapshot> {
        Arc::new(AppSnapshot {
            connection: ConnectionState::Ready,
            glazewm_version: None,
            monitors: vec![],
            focused_monitor_id: None,
            focused_workspace_id: None,
            focused_direction: None,
            is_paused: false,
            last_ui_change: None,
            revision: rev,
            stale: false,
        })
    }

    #[test]
    fn publish_never_busy_loops_with_slow_consumer() {
        let shared: SharedIpcToUi = Arc::new(Mutex::new(IpcToUiState::default()));
        let notify = IpcNotify::new(0, 0);
        let shared_p = shared.clone();
        let producer = std::thread::spawn(move || {
            let reducer = Reducer::new();
            // Simulate a fast IPC task flooding snapshots while the UI is slow.
            for _ in 0..10_000 {
                publish(&shared_p, &reducer, &notify);
            }
        });
        // Slow consumer: drains at a fraction of the producer rate.
        let mut saw_snapshot = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let mut st = shared.lock().unwrap();
            if st.latest.take().is_some() {
                saw_snapshot = true;
            }
            st.results.clear();
        }
        // If publish busy-looped, the producer thread would never finish.
        producer.join().expect("publish must terminate");
        assert!(saw_snapshot, "snapshots were delivered");
    }

    #[test]
    fn command_results_are_never_dropped() {
        let shared: SharedIpcToUi = Arc::new(Mutex::new(IpcToUiState::default()));
        let notify = IpcNotify::new(0, 0);
        let n = 200;
        let shared_p = shared.clone();
        let producer = std::thread::spawn(move || {
            for i in 0..n {
                push_result(&shared_p, i, true, Some(format!("r{i}")), &notify);
                // Interleave snapshots to simulate a busy stream.
                let mut st = shared_p.lock().unwrap();
                st.latest = Some(fake_snapshot(i));
            }
        });
        producer.join().unwrap();
        let mut st = shared.lock().unwrap();
        assert_eq!(st.results.len(), n as usize, "no result may be dropped");
        let first = st.results.pop_front().unwrap();
        assert_eq!(first.0, 0);
        let last = st.results.pop_back().unwrap();
        assert_eq!(last.0, n - 1);
    }

    #[test]
    fn latest_snapshot_wins() {
        let shared: SharedIpcToUi = Arc::new(Mutex::new(IpcToUiState::default()));
        let notify = IpcNotify::new(0, 0);
        for rev in 0..100 {
            let mut st = shared.lock().unwrap();
            st.latest = Some(fake_snapshot(rev));
            drop(st);
            notify.poke();
        }
        let st = shared.lock().unwrap();
        assert_eq!(st.latest.as_ref().unwrap().revision, 99);
    }
}
