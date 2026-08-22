//! Remote control: the streamer-side server (accepts one authorised viewer and
//! feeds its input to an injector) and the viewer-side client.
//!
//! Design: `docs/REMOTE-CONTROL.md`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use meshcast_signal::control::{
    read_frame, validate_token, write_frame, ControlGrant, ControlMsg, NamedKey, PointerButton,
    CONTROL_ALPN, CONTROL_VERSION, HELLO_TIMEOUT_SECS, IDLE_REVOKE_SECS, MAX_EVENTS_PER_SEC,
};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// One input action for the platform backend.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectCmd {
    /// Pointer position as a fraction (0..1) of the captured surface.
    Move {
        x: f32,
        y: f32,
    },
    Button {
        button: PointerButton,
        pressed: bool,
    },
    /// Scroll in wheel "lines" (positive dy = down).
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        key: NamedKey,
        pressed: bool,
    },
    Text(String),
    /// Release every button/key the backend believes is held.
    ReleaseAll,
}

/// Handle to a running injector backend task.
#[derive(Debug, Clone)]
pub struct InjectorHandle {
    tx: mpsc::Sender<InjectCmd>,
    /// Size of the captured surface in pixels (what pointer fractions map onto).
    pub size: (u32, u32),
}

impl InjectorHandle {
    pub fn new(tx: mpsc::Sender<InjectCmd>, size: (u32, u32)) -> Self {
        Self { tx, size }
    }

    /// Deliver a command. Moves/scroll/presses/text are best-effort (dropped
    /// if the backend is saturated); *releases* always get through so nothing
    /// can stick on the streamer's desktop.
    pub async fn deliver(&self, cmd: InjectCmd) {
        let must_deliver = matches!(
            cmd,
            InjectCmd::Button { pressed: false, .. }
                | InjectCmd::Key { pressed: false, .. }
                | InjectCmd::ReleaseAll
        );
        if must_deliver {
            if self.tx.send(cmd).await.is_err() {
                tracing::debug!("injector gone; release not delivered");
            }
        } else if let Err(e) = self.tx.try_send(cmd) {
            tracing::debug!("injector queue full, dropping event: {e}");
        }
    }

    pub async fn release_all(&self) {
        self.deliver(InjectCmd::ReleaseAll).await;
    }
}

/// Tracks what a backend currently holds so `ReleaseAll` can undo it.
#[derive(Debug, Default)]
pub struct HeldState {
    pub buttons: HashSet<PointerButton>,
    pub keys: HashSet<NamedKey>,
}

impl HeldState {
    pub fn note_button(&mut self, b: PointerButton, pressed: bool) {
        if pressed {
            self.buttons.insert(b);
        } else {
            self.buttons.remove(&b);
        }
    }
    pub fn note_key(&mut self, k: NamedKey, pressed: bool) {
        if pressed {
            self.keys.insert(k);
        } else {
            self.keys.remove(&k);
        }
    }
    pub fn drain(&mut self) -> (Vec<PointerButton>, Vec<NamedKey>) {
        (self.buttons.drain().collect(), self.keys.drain().collect())
    }
}

/// Accumulates fractional scroll into whole wheel clicks.
#[derive(Debug, Default)]
pub struct ScrollAccumulator {
    x: f32,
    y: f32,
}

impl ScrollAccumulator {
    /// Returns whole steps to emit for each axis.
    pub fn add(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let sx = self.x.trunc();
        let sy = self.y.trunc();
        self.x -= sx;
        self.y -= sy;
        (sx as i32, sy as i32)
    }
}

// ---------------------------------------------------------------------------
// Server (streamer side)
// ---------------------------------------------------------------------------

/// Events the server reports to the daemon loop.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    ControllerConnected { controller: String },
    ControllerDisconnected { controller: String, reason: String },
}

struct Armed {
    token: String,
    controller: String,
    injector: InjectorHandle,
}

struct ActiveConn {
    conn: Connection,
}

#[derive(Default)]
struct ServerState {
    armed: Option<Armed>,
    active: Option<ActiveConn>,
}

/// iroh protocol handler for [`CONTROL_ALPN`]. Cheap to clone; lives for the
/// daemon's lifetime and is armed/disarmed per grant.
#[derive(Clone)]
pub struct ControlServer {
    state: Arc<Mutex<ServerState>>,
    events: mpsc::UnboundedSender<ServerEvent>,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ControlServer {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<ServerEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Self {
                state: Arc::new(Mutex::new(ServerState::default())),
                events,
            },
            rx,
        )
    }

    /// Accept exactly one connection presenting `token`, feeding `injector`.
    /// Replaces any previous grant (and closes its connection).
    pub fn arm(&self, token: String, controller: String, injector: InjectorHandle) {
        let old = {
            let mut st = lock(&self.state);
            st.armed = Some(Armed {
                token,
                controller,
                injector,
            });
            st.active.take()
        };
        if let Some(old) = old {
            old.conn.close(1u32.into(), b"replaced");
        }
    }

    /// Drop the grant and disconnect the controller. Returns the controller's
    /// name if something was armed.
    pub fn disarm(&self, reason: &str) -> Option<String> {
        let (armed, active) = {
            let mut st = lock(&self.state);
            (st.armed.take(), st.active.take())
        };
        if let Some(active) = active {
            active.conn.close(1u32.into(), reason.as_bytes());
        }
        armed.map(|a| a.controller)
    }

    pub fn controller(&self) -> Option<String> {
        lock(&self.state)
            .armed
            .as_ref()
            .map(|a| a.controller.clone())
    }

    /// Is a controller currently connected (as opposed to merely granted)?
    pub fn is_connected(&self) -> bool {
        lock(&self.state).active.is_some()
    }

    async fn handle(&self, conn: Connection) -> Result<()> {
        let remote = conn.remote_id();
        let (mut send, mut recv) = conn
            .accept_bi()
            .await
            .context("controller did not open a stream")?;

        // Hello must come first and quickly.
        let hello = tokio::time::timeout(
            Duration::from_secs(HELLO_TIMEOUT_SECS),
            read_frame(&mut recv),
        )
        .await
        .context("no Hello within timeout")??;
        let (version, token) = match hello {
            Some(ControlMsg::Hello { version, token }) => (version, token),
            other => {
                write_frame(
                    &mut send,
                    &ControlMsg::Denied {
                        reason: "expected Hello".into(),
                    },
                )
                .await
                .ok();
                anyhow::bail!("first frame was {other:?}");
            }
        };
        if version != CONTROL_VERSION {
            write_frame(
                &mut send,
                &ControlMsg::Denied {
                    reason: format!("unsupported protocol version {version}"),
                },
            )
            .await
            .ok();
            anyhow::bail!("version mismatch");
        }

        // Authorise (no awaits while the lock is held).
        enum Auth {
            NotArmed,
            BadToken,
            Ok {
                controller: String,
                injector: InjectorHandle,
                replaced: Option<ActiveConn>,
            },
        }
        let auth = {
            let mut st = lock(&self.state);
            match st.armed.as_ref() {
                None => Auth::NotArmed,
                Some(armed)
                    if !validate_token(&token)
                        || !constant_time_eq(armed.token.as_bytes(), token.as_bytes()) =>
                {
                    Auth::BadToken
                }
                Some(armed) => {
                    let controller = armed.controller.clone();
                    let injector = armed.injector.clone();
                    let replaced = st.active.replace(ActiveConn { conn: conn.clone() });
                    Auth::Ok {
                        controller,
                        injector,
                        replaced,
                    }
                }
            }
        };
        let (controller, injector, replaced) = match auth {
            Auth::NotArmed => {
                write_frame(
                    &mut send,
                    &ControlMsg::Denied {
                        reason: "remote control is not enabled right now".into(),
                    },
                )
                .await
                .ok();
                anyhow::bail!("not armed");
            }
            Auth::BadToken => {
                write_frame(
                    &mut send,
                    &ControlMsg::Denied {
                        reason: "invalid token".into(),
                    },
                )
                .await
                .ok();
                anyhow::bail!("bad token from {}", remote.fmt_short());
            }
            Auth::Ok {
                controller,
                injector,
                replaced,
            } => (controller, injector, replaced),
        };
        if let Some(old) = replaced {
            old.conn.close(1u32.into(), b"reconnected");
        }

        write_frame(
            &mut send,
            &ControlMsg::Welcome {
                width: injector.size.0,
                height: injector.size.1,
            },
        )
        .await?;
        let _ = self.events.send(ServerEvent::ControllerConnected {
            controller: controller.clone(),
        });
        tracing::info!(controller = %controller, peer = %remote.fmt_short(), "Controller connected");

        // Event loop.
        let result = self.pump(&mut send, &mut recv, &injector).await;
        injector.release_all().await;

        // Forget this connection — but only if it is still the current one. If a
        // newer connection replaced it (same token, e.g. a second viewer window)
        // or the daemon disarmed us, this one is already history and must not
        // be reported as "the controller disconnected" (that would revoke the
        // live replacement).
        let was_current = {
            let mut st = lock(&self.state);
            if matches!(&st.active, Some(a) if a.conn.stable_id() == conn.stable_id()) {
                st.active = None;
                true
            } else {
                false
            }
        };
        let reason = match &result {
            Ok(r) => r.clone(),
            Err(e) => format!("{e}"),
        };
        if was_current {
            let _ = self.events.send(ServerEvent::ControllerDisconnected {
                controller: controller.clone(),
                reason: reason.clone(),
            });
        }
        tracing::info!(controller = %controller, "Controller disconnected ({reason})");
        conn.close(0u32.into(), b"bye");
        Ok(())
    }

    /// Forward input until the stream ends; returns the reason.
    async fn pump(
        &self,
        send: &mut iroh::endpoint::SendStream,
        recv: &mut iroh::endpoint::RecvStream,
        injector: &InjectorHandle,
    ) -> Result<String> {
        let mut window_start = Instant::now();
        let mut window_count: u32 = 0;
        let idle = Duration::from_secs(IDLE_REVOKE_SECS);
        loop {
            let frame = tokio::time::timeout(idle, read_frame(recv)).await;
            let msg = match frame {
                Err(_) => {
                    let _ = write_frame(
                        send,
                        &ControlMsg::Revoked {
                            reason: "idle".into(),
                        },
                    )
                    .await;
                    return Ok("idle timeout".into());
                }
                Ok(Ok(Some(m))) => m,
                Ok(Ok(None)) => return Ok("viewer closed".into()),
                Ok(Err(e)) => return Ok(format!("connection lost: {e}")),
            };

            if msg.is_input() {
                // Simple per-second rate limit — but never drop a *release*,
                // or the streamer ends up with a stuck key/button.
                if window_start.elapsed() >= Duration::from_secs(1) {
                    window_start = Instant::now();
                    window_count = 0;
                }
                window_count += 1;
                let is_release = matches!(
                    msg,
                    ControlMsg::PointerButton { pressed: false, .. }
                        | ControlMsg::Key { pressed: false, .. }
                );
                if window_count > MAX_EVENTS_PER_SEC && !is_release {
                    continue;
                }
            }

            match msg {
                ControlMsg::PointerMove { x, y } => {
                    injector
                        .deliver(InjectCmd::Move {
                            x: x.clamp(0.0, 1.0),
                            y: y.clamp(0.0, 1.0),
                        })
                        .await
                }
                ControlMsg::PointerButton { button, pressed } => {
                    injector
                        .deliver(InjectCmd::Button { button, pressed })
                        .await
                }
                ControlMsg::Scroll { dx, dy } => {
                    injector
                        .deliver(InjectCmd::Scroll {
                            dx: dx.clamp(-50.0, 50.0),
                            dy: dy.clamp(-50.0, 50.0),
                        })
                        .await
                }
                ControlMsg::Key { key, pressed } => {
                    injector.deliver(InjectCmd::Key { key, pressed }).await
                }
                ControlMsg::Text { text } => {
                    // Bound typed bursts; strip control characters.
                    let cleaned: String =
                        text.chars().filter(|c| !c.is_control()).take(256).collect();
                    if !cleaned.is_empty() {
                        injector.deliver(InjectCmd::Text(cleaned)).await;
                    }
                }
                ControlMsg::Release => injector.deliver(InjectCmd::ReleaseAll).await,
                ControlMsg::Ping => {
                    let _ = write_frame(send, &ControlMsg::Pong).await;
                }
                ControlMsg::Hello { .. }
                | ControlMsg::Welcome { .. }
                | ControlMsg::Denied { .. }
                | ControlMsg::Revoked { .. }
                | ControlMsg::Pong => {}
            }
        }
    }
}

impl ProtocolHandler for ControlServer {
    async fn accept(&self, conn: Connection) -> std::result::Result<(), AcceptError> {
        if let Err(e) = self.handle(conn).await {
            tracing::debug!("control connection ended: {e:#}");
        }
        Ok(())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Client (viewer side)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ClientStatus {
    Connecting,
    Active { width: u32, height: u32 },
    Denied(String),
    Ended(String),
}

/// Viewer-side control connection. Created from the viewer's iroh endpoint;
/// input is queued with [`ControlClient::send`] and forwarded by a task.
#[derive(Clone)]
pub struct ControlClient {
    tx: mpsc::UnboundedSender<ControlMsg>,
    status: Arc<Mutex<ClientStatus>>,
}

impl ControlClient {
    pub fn connect(endpoint: Endpoint, grant: ControlGrant) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(ClientStatus::Connecting));
        let client = Self {
            tx,
            status: status.clone(),
        };
        tokio::spawn(async move {
            let outcome = run_client(endpoint, grant.addr, grant.token, rx, status.clone()).await;
            let mut st = lock(&status);
            if matches!(*st, ClientStatus::Connecting | ClientStatus::Active { .. }) {
                *st = match outcome {
                    Ok(reason) => ClientStatus::Ended(reason),
                    Err(e) => ClientStatus::Ended(format!("{e:#}")),
                };
            }
        });
        client
    }

    pub fn status(&self) -> ClientStatus {
        lock(&self.status).clone()
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status(), ClientStatus::Active { .. })
    }

    pub fn send(&self, msg: ControlMsg) {
        let _ = self.tx.send(msg);
    }

    /// Ask the task to finish (sends Release, then closes).
    pub fn disconnect(&self) {
        let _ = self.tx.send(ControlMsg::Release);
        // Dropping all senders ends the loop; the App holds the only clone, so
        // we signal via a special sentinel: closing happens when the App drops
        // the client. Explicit close:
        *lock(&self.status) = ClientStatus::Ended("released".into());
    }
}

async fn run_client(
    endpoint: Endpoint,
    addr: EndpointAddr,
    token: String,
    mut rx: mpsc::UnboundedReceiver<ControlMsg>,
    status: Arc<Mutex<ClientStatus>>,
) -> Result<String> {
    let conn = tokio::time::timeout(
        Duration::from_secs(20),
        endpoint.connect(addr, CONTROL_ALPN),
    )
    .await
    .context("timed out connecting to the streamer")?
    .context("connect failed")?;
    let (mut send, mut recv) = conn.open_bi().await.context("open stream")?;
    write_frame(
        &mut send,
        &ControlMsg::Hello {
            version: CONTROL_VERSION,
            token,
        },
    )
    .await?;

    match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv)).await {
        Ok(Ok(Some(ControlMsg::Welcome { width, height }))) => {
            *lock(&status) = ClientStatus::Active { width, height };
        }
        Ok(Ok(Some(ControlMsg::Denied { reason }))) => {
            *lock(&status) = ClientStatus::Denied(reason.clone());
            return Ok(format!("denied: {reason}"));
        }
        Ok(Ok(other)) => anyhow::bail!("unexpected reply {other:?}"),
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("no reply from streamer"),
    }

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break Ok("closed".into()) };
                // Treat Ended status as a request to stop.
                if matches!(*lock(&status), ClientStatus::Ended(_)) {
                    let _ = write_frame(&mut send, &ControlMsg::Release).await;
                    break Ok("released".into());
                }
                if let Err(e) = write_frame(&mut send, &msg).await {
                    break Ok(format!("connection lost: {e}"));
                }
            }
            incoming = read_frame(&mut recv) => {
                match incoming {
                    Ok(Some(ControlMsg::Revoked { reason })) => break Ok(format!("revoked ({reason})")),
                    Ok(Some(_)) => {}
                    Ok(None) => break Ok("streamer closed".into()),
                    Err(e) => break Ok(format!("connection lost: {e}")),
                }
            }
        }
    }
    .inspect(|_| conn.close(0u32.into(), b"bye"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_accumulates_whole_steps() {
        let mut acc = ScrollAccumulator::default();
        assert_eq!(acc.add(0.0, 0.4), (0, 0));
        assert_eq!(acc.add(0.0, 0.7), (0, 1));
        assert_eq!(acc.add(0.0, -1.2), (0, -1));
        assert_eq!(acc.add(2.5, 0.0), (2, 0));
    }

    #[test]
    fn held_state_tracks_and_drains() {
        let mut h = HeldState::default();
        h.note_button(PointerButton::Left, true);
        h.note_key(NamedKey::Shift, true);
        h.note_key(NamedKey::Shift, false);
        let (b, k) = h.drain();
        assert_eq!(b, vec![PointerButton::Left]);
        assert!(k.is_empty());
    }

    /// End-to-end over real iroh endpoints (needs network for relay bootstrap):
    /// `cargo test -p meshcast-cli -- --ignored control_roundtrip`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn control_roundtrip() {
        use iroh::endpoint::presets;
        use iroh::protocol::Router;
        use meshcast_signal::control::generate_token;

        let (server, mut events) = ControlServer::new();
        let ep_s = Endpoint::builder(presets::N0).bind().await.unwrap();
        let _router = Router::builder(ep_s.clone())
            .accept(CONTROL_ALPN, server.clone())
            .spawn();
        ep_s.online().await;
        let addr = ep_s.addr();

        let (inj_tx, mut inj_rx) = mpsc::channel::<InjectCmd>(64);
        let token = generate_token();
        server.arm(
            token.clone(),
            "Tester".into(),
            InjectorHandle::new(inj_tx, (1920, 1080)),
        );

        // Wrong token is denied.
        let ep_c = Endpoint::builder(presets::N0).bind().await.unwrap();
        ep_c.online().await;
        let bad = ControlClient::connect(
            ep_c.clone(),
            ControlGrant {
                ticket: "t".into(),
                token: generate_token(),
                addr: addr.clone(),
                streamer: "S".into(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match bad.status() {
                ClientStatus::Denied(_) | ClientStatus::Ended(_) => break,
                _ if Instant::now() > deadline => panic!("bad token not denied"),
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(
            matches!(bad.status(), ClientStatus::Denied(_)),
            "{:?}",
            bad.status()
        );

        // Right token: welcome, events flow, revoke ends it.
        let good = ControlClient::connect(
            ep_c.clone(),
            ControlGrant {
                ticket: "t".into(),
                token,
                addr,
                streamer: "S".into(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !good.is_active() {
            assert!(
                Instant::now() < deadline,
                "never active: {:?}",
                good.status()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            good.status(),
            ClientStatus::Active {
                width: 1920,
                height: 1080
            }
        );
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, ServerEvent::ControllerConnected { .. }));

        good.send(ControlMsg::PointerMove { x: 0.5, y: 0.25 });
        good.send(ControlMsg::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        });
        good.send(ControlMsg::Text { text: "hi".into() });
        let a = tokio::time::timeout(Duration::from_secs(5), inj_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a, InjectCmd::Move { x: 0.5, y: 0.25 });
        let b = inj_rx.recv().await.unwrap();
        assert_eq!(
            b,
            InjectCmd::Button {
                button: PointerButton::Left,
                pressed: true
            }
        );
        let c = inj_rx.recv().await.unwrap();
        assert_eq!(c, InjectCmd::Text("hi".into()));

        assert_eq!(server.disarm("test revoke"), Some("Tester".into()));
        // Server releases everything it thinks is held on disconnect.
        let d = tokio::time::timeout(Duration::from_secs(5), inj_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d, InjectCmd::ReleaseAll);
        let deadline = Instant::now() + Duration::from_secs(10);
        while good.is_active() {
            assert!(
                Instant::now() < deadline,
                "client still active after revoke"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(matches!(good.status(), ClientStatus::Ended(_)));
        // We initiated the disconnect via disarm, so no ControllerDisconnected
        // event is reported (that event is for viewer-initiated drops only).
        assert!(
            tokio::time::timeout(Duration::from_millis(500), events.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
