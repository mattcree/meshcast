//! macOS / Windows: input injection via `enigo` (CGEvent / SendInput).
//! Pointer fractions map onto the main display.

use anyhow::{Context, Result};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use meshcast_signal::control::{NamedKey, PointerButton};
use tokio::sync::mpsc;

use crate::control::{HeldState, InjectCmd, InjectorHandle, ScrollAccumulator};

/// Create the injector on a dedicated thread (enigo isn't necessarily `Send`).
pub fn start() -> Result<InjectorHandle> {
    let (tx, rx) = mpsc::channel::<InjectCmd>(256);
    let (size_tx, size_rx) = std::sync::mpsc::channel::<Result<(u32, u32)>>();
    std::thread::Builder::new()
        .name("meshcast-inject".into())
        .spawn(move || run(rx, size_tx))
        .context("spawn injector thread")?;
    let size = size_rx.recv().context("injector thread died")??;
    Ok(InjectorHandle::new(tx, size))
}

fn run(mut rx: mpsc::Receiver<InjectCmd>, size_tx: std::sync::mpsc::Sender<Result<(u32, u32)>>) {
    let settings = Settings {
        release_keys_when_dropped: true,
        ..Default::default()
    };
    let mut enigo = match Enigo::new(&settings) {
        Ok(e) => e,
        Err(e) => {
            let _ = size_tx.send(Err(anyhow::anyhow!("input injection unavailable: {e}")));
            return;
        }
    };
    let size = match enigo.main_display() {
        Ok((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
        _ => (1920, 1080),
    };
    let _ = size_tx.send(Ok(size));

    let mut held = HeldState::default();
    let mut scroll = ScrollAccumulator::default();
    while let Some(cmd) = rx.blocking_recv() {
        let res: enigo::InputResult<()> = match cmd {
            InjectCmd::Move { x, y } => enigo.move_mouse(
                (x * size.0 as f32) as i32,
                (y * size.1 as f32) as i32,
                Coordinate::Abs,
            ),
            InjectCmd::Button { button, pressed } => {
                held.note_button(button, pressed);
                enigo.button(map_button(button), direction(pressed))
            }
            InjectCmd::Scroll { dx, dy } => {
                let (sx, sy) = scroll.add(dx, dy);
                let mut r = Ok(());
                if sy != 0 {
                    r = enigo.scroll(sy, Axis::Vertical);
                }
                if sx != 0 && r.is_ok() {
                    r = enigo.scroll(sx, Axis::Horizontal);
                }
                r
            }
            InjectCmd::Key { key, pressed } => {
                held.note_key(key, pressed);
                enigo.key(map_key(key), direction(pressed))
            }
            InjectCmd::Text(text) => enigo.text(&text),
            InjectCmd::ReleaseAll => {
                let (buttons, keys) = held.drain();
                let mut r = Ok(());
                for b in buttons {
                    r = enigo.button(map_button(b), Direction::Release);
                }
                for k in keys {
                    r = enigo.key(map_key(k), Direction::Release);
                }
                r
            }
        };
        if let Err(e) = res {
            tracing::warn!("Input injection failed: {e}");
        }
    }
    let (buttons, keys) = held.drain();
    for b in buttons {
        let _ = enigo.button(map_button(b), Direction::Release);
    }
    for k in keys {
        let _ = enigo.key(map_key(k), Direction::Release);
    }
}

fn direction(pressed: bool) -> Direction {
    if pressed {
        Direction::Press
    } else {
        Direction::Release
    }
}

fn map_button(b: PointerButton) -> Button {
    match b {
        PointerButton::Left => Button::Left,
        PointerButton::Right => Button::Right,
        PointerButton::Middle => Button::Middle,
        PointerButton::Back => Button::Back,
        PointerButton::Forward => Button::Forward,
    }
}

fn map_key(k: NamedKey) -> Key {
    match k {
        NamedKey::Escape => Key::Escape,
        NamedKey::Enter => Key::Return,
        NamedKey::Tab => Key::Tab,
        NamedKey::Backspace => Key::Backspace,
        NamedKey::Delete => Key::Delete,
        NamedKey::Insert => Key::Insert,
        NamedKey::Space => Key::Space,
        NamedKey::ArrowLeft => Key::LeftArrow,
        NamedKey::ArrowRight => Key::RightArrow,
        NamedKey::ArrowUp => Key::UpArrow,
        NamedKey::ArrowDown => Key::DownArrow,
        NamedKey::Home => Key::Home,
        NamedKey::End => Key::End,
        NamedKey::PageUp => Key::PageUp,
        NamedKey::PageDown => Key::PageDown,
        NamedKey::Shift => Key::Shift,
        NamedKey::Control => Key::Control,
        NamedKey::Alt => Key::Alt,
        NamedKey::Super => Key::Meta,
        NamedKey::CapsLock => Key::CapsLock,
        NamedKey::F1 => Key::F1,
        NamedKey::F2 => Key::F2,
        NamedKey::F3 => Key::F3,
        NamedKey::F4 => Key::F4,
        NamedKey::F5 => Key::F5,
        NamedKey::F6 => Key::F6,
        NamedKey::F7 => Key::F7,
        NamedKey::F8 => Key::F8,
        NamedKey::F9 => Key::F9,
        NamedKey::F10 => Key::F10,
        NamedKey::F11 => Key::F11,
        NamedKey::F12 => Key::F12,
        NamedKey::Char(c) => Key::Unicode(c),
    }
}
