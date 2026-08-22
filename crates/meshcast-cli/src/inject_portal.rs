//! Linux: screen capture + input injection through one xdg-desktop-portal
//! session (RemoteDesktop + ScreenCast). Pointer positions are absolute in the
//! shared stream's coordinates, so what the viewer clicks is where the click
//! lands regardless of monitor layout.

use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use meshcast_signal::control::{NamedKey, PointerButton};
use moq_media::capture::PipeWireScreenCapturer;
use tokio::sync::{mpsc, oneshot};

use crate::control::{HeldState, InjectCmd, InjectorHandle, ScrollAccumulator};

const INFRA_TIMEOUT: Duration = Duration::from_secs(10);
const DIALOG_TIMEOUT: Duration = Duration::from_secs(120);

/// Why the combined portal session couldn't be started.
#[derive(Debug)]
pub enum PortalError {
    /// No RemoteDesktop/ScreenCast portal — caller may fall back to plain capture.
    Unavailable(anyhow::Error),
    /// The session was created but failed or was cancelled by the user. Do
    /// *not* fall back (that would pop a second dialog); the stream fails.
    Failed(anyhow::Error),
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortalError::Unavailable(e) | PortalError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// Result of a combined portal session: a capturer for iroh-live and an
/// injector handle. Dropping `guard` ends the portal session (and therefore
/// the stream) — keep it alive with the stream.
pub struct PortalCapture {
    pub capturer: PipeWireScreenCapturer,
    pub injector: InjectorHandle,
    pub guard: PortalGuard,
}

/// Tells the injector task to release everything and close the portal session.
/// (Not an abort: the release/close tail must run.)
pub struct PortalGuard {
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for PortalGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn step<T, E: std::fmt::Display>(
    what: &str,
    dur: Duration,
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    tokio::time::timeout(dur, fut)
        .await
        .map_err(|_| anyhow::anyhow!("{what}: timed out"))?
        .map_err(|e| anyhow::anyhow!("{what}: {e}"))
}

/// Open a RemoteDesktop session with screen-cast, start capture on its stream
/// and spawn the injector task.
pub async fn start(fps: u32) -> std::result::Result<PortalCapture, PortalError> {
    let rd = step("RemoteDesktop portal", INFRA_TIMEOUT, RemoteDesktop::new())
        .await
        .map_err(PortalError::Unavailable)?;
    let sc = step("ScreenCast portal", INFRA_TIMEOUT, Screencast::new())
        .await
        .map_err(PortalError::Unavailable)?;

    let inner = async {
        let session = step(
            "RemoteDesktop: create_session",
            INFRA_TIMEOUT,
            rd.create_session(),
        )
        .await?;
        step(
            "RemoteDesktop: select_devices",
            INFRA_TIMEOUT,
            rd.select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::DoNot,
            ),
        )
        .await?
        .response()
        .context("RemoteDesktop: select_devices rejected")?;
        step(
            "ScreenCast: select_sources",
            INFRA_TIMEOUT,
            sc.select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            ),
        )
        .await?
        .response()
        .context("ScreenCast: select_sources rejected")?;

        tracing::info!("Waiting for the screen-share / remote-control dialog…");
        let devices = step(
            "RemoteDesktop: start",
            DIALOG_TIMEOUT,
            rd.start(&session, None),
        )
        .await?
        .response()
        .context("Screen sharing was cancelled")?;

        let streams = devices.streams().unwrap_or_default();
        let stream = streams.first().context("portal returned no stream")?;
        let node_id = stream.pipe_wire_node_id();

        let fd = step(
            "ScreenCast: open_pipe_wire_remote",
            INFRA_TIMEOUT,
            sc.open_pipe_wire_remote(&session),
        )
        .await?;

        let capturer = tokio::task::spawn_blocking(move || {
            PipeWireScreenCapturer::from_portal_stream(fd, node_id, fps as f32)
        })
        .await
        .context("capture thread panicked")??;

        // Pointer coordinates for NotifyPointerMotionAbsolute are in the
        // *stream's* pixel space (what PipeWire negotiated), not the portal's
        // logical size — they differ on HiDPI.
        let [fw, fh] = moq_media::capture::VideoSource::format(&capturer).dimensions;
        let size = (fw.max(1), fh.max(1));
        tracing::info!(
            node_id,
            width = size.0,
            height = size.1,
            "Portal session ready (with remote control)"
        );
        anyhow::Ok((session, capturer, node_id, size))
    };
    let (session, capturer, node_id, size) = inner.await.map_err(PortalError::Failed)?;

    let (tx, rx) = mpsc::channel::<InjectCmd>(256);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(inject_loop(rd, session, node_id, size, rx, shutdown_rx));

    Ok(PortalCapture {
        capturer,
        injector: InjectorHandle::new(tx, size),
        guard: PortalGuard {
            shutdown: Some(shutdown_tx),
        },
    })
}

async fn inject_loop(
    rd: RemoteDesktop<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    stream: u32,
    size: (u32, u32),
    mut rx: mpsc::Receiver<InjectCmd>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut held = HeldState::default();
    let mut scroll = ScrollAccumulator::default();
    loop {
        let cmd = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            cmd = rx.recv() => match cmd { Some(c) => c, None => break },
        };
        let res: ashpd::Result<()> = match cmd {
            InjectCmd::Move { x, y } => {
                rd.notify_pointer_motion_absolute(
                    &session,
                    stream,
                    (x * size.0 as f32) as f64,
                    (y * size.1 as f32) as f64,
                )
                .await
            }
            InjectCmd::Button { button, pressed } => {
                held.note_button(button, pressed);
                rd.notify_pointer_button(&session, evdev_button(button), key_state(pressed))
                    .await
            }
            InjectCmd::Scroll { dx, dy } => {
                let (sx, sy) = scroll.add(dx, dy);
                let mut r = Ok(());
                if sy != 0 {
                    r = rd
                        .notify_pointer_axis_discrete(&session, Axis::Vertical, sy)
                        .await;
                }
                if sx != 0 && r.is_ok() {
                    r = rd
                        .notify_pointer_axis_discrete(&session, Axis::Horizontal, sx)
                        .await;
                }
                r
            }
            InjectCmd::Key { key, pressed } => {
                held.note_key(key, pressed);
                rd.notify_keyboard_keysym(&session, keysym(key), key_state(pressed))
                    .await
            }
            InjectCmd::Text(text) => {
                let mut r = Ok(());
                for ch in text.chars() {
                    let ks = char_keysym(ch);
                    r = rd
                        .notify_keyboard_keysym(&session, ks, KeyState::Pressed)
                        .await;
                    if r.is_ok() {
                        r = rd
                            .notify_keyboard_keysym(&session, ks, KeyState::Released)
                            .await;
                    }
                    if r.is_err() {
                        break;
                    }
                }
                r
            }
            InjectCmd::ReleaseAll => release_held(&rd, &session, &mut held).await,
        };
        if let Err(e) = res {
            tracing::warn!("Input injection failed: {e}");
        }
    }
    // Drain queued releases, let go of everything we hold, close the session.
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            InjectCmd::Button {
                button,
                pressed: false,
            } => {
                held.note_button(button, false);
                let _ = rd
                    .notify_pointer_button(&session, evdev_button(button), KeyState::Released)
                    .await;
            }
            InjectCmd::Key {
                key,
                pressed: false,
            } => {
                held.note_key(key, false);
                let _ = rd
                    .notify_keyboard_keysym(&session, keysym(key), KeyState::Released)
                    .await;
            }
            _ => {}
        }
    }
    let _ = release_held(&rd, &session, &mut held).await;
    let _ = tokio::time::timeout(INFRA_TIMEOUT, session.close()).await;
    tracing::debug!("Portal session closed");
}

/// Release every button/key we believe is held.
async fn release_held(
    rd: &RemoteDesktop<'static>,
    session: &Session<'static, RemoteDesktop<'static>>,
    held: &mut HeldState,
) -> ashpd::Result<()> {
    let (buttons, keys) = held.drain();
    let mut r = Ok(());
    for b in buttons {
        r = rd
            .notify_pointer_button(session, evdev_button(b), KeyState::Released)
            .await;
    }
    for k in keys {
        r = rd
            .notify_keyboard_keysym(session, keysym(k), KeyState::Released)
            .await;
    }
    r
}

fn key_state(pressed: bool) -> KeyState {
    if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

/// Linux evdev button codes (input-event-codes.h).
fn evdev_button(b: PointerButton) -> i32 {
    match b {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
        PointerButton::Back => 0x113,
        PointerButton::Forward => 0x114,
    }
}

/// X11 keysyms (keysymdef.h) for named keys.
pub fn keysym(k: NamedKey) -> i32 {
    match k {
        NamedKey::Escape => 0xff1b,
        NamedKey::Enter => 0xff0d,
        NamedKey::Tab => 0xff09,
        NamedKey::Backspace => 0xff08,
        NamedKey::Delete => 0xffff,
        NamedKey::Insert => 0xff63,
        NamedKey::Space => 0x020,
        NamedKey::ArrowLeft => 0xff51,
        NamedKey::ArrowUp => 0xff52,
        NamedKey::ArrowRight => 0xff53,
        NamedKey::ArrowDown => 0xff54,
        NamedKey::Home => 0xff50,
        NamedKey::End => 0xff57,
        NamedKey::PageUp => 0xff55,
        NamedKey::PageDown => 0xff56,
        NamedKey::Shift => 0xffe1,
        NamedKey::Control => 0xffe3,
        NamedKey::Alt => 0xffe9,
        NamedKey::Super => 0xffeb,
        NamedKey::CapsLock => 0xffe5,
        NamedKey::F1 => 0xffbe,
        NamedKey::F2 => 0xffbf,
        NamedKey::F3 => 0xffc0,
        NamedKey::F4 => 0xffc1,
        NamedKey::F5 => 0xffc2,
        NamedKey::F6 => 0xffc3,
        NamedKey::F7 => 0xffc4,
        NamedKey::F8 => 0xffc5,
        NamedKey::F9 => 0xffc6,
        NamedKey::F10 => 0xffc7,
        NamedKey::F11 => 0xffc8,
        NamedKey::F12 => 0xffc9,
        NamedKey::Char(c) => char_keysym(c),
    }
}

/// Keysym for a printable character: Latin-1 maps directly, everything else
/// uses the Unicode keysym range (0x0100_0000 + code point).
pub fn char_keysym(ch: char) -> i32 {
    let cp = ch as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp as i32
    } else {
        (0x0100_0000 | cp) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(keysym(NamedKey::Enter), 0xff0d);
        assert_eq!(evdev_button(PointerButton::Left), 0x110);
    }
}
