//! Test input for the headless backend: with `HELIX_HEADLESS_INPUT=path`
//! the headless compositor listens on a Unix socket at `path` for keys,
//! pointer motion, buttons and scrolling, one command per line, and
//! answers each line with `ok` (or `error: …`) once it has been handled,
//! so a test knows its input arrived.
//!
//! Only the headless backend starts it (see `headless::init`); the DRM and
//! nested backends have no such socket, so nothing outside a test can
//! inject input this way.
//!
//! Commands (coordinates are global logical pixels, codes are evdev's):
//!
//! - `key CODE 1|0`: a key down or up.
//! - `keysym SYM 1|0`: the key for keysym `SYM` (decimal or `0x…`),
//!   with Shift around it when the keymap needs it.
//! - `type TEXT`: each character of `TEXT` (the rest of the line) pressed
//!   and released.
//! - `tap CODE…`: the keys pressed in order, then released in reverse
//!   (a chord such as `tap 29 46` for Ctrl+C).
//! - `move X Y`: the pointer to (X, Y).
//! - `button CODE 1|0`: a pointer button (272 left, 273 right, 274
//!   middle) down or up.
//! - `click X Y [CODE]`: move there, press and release (left by default).
//! - `scroll DX DY`: smooth scrolling by (DX, DY) pixels.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use helix_comp::remote_desktop::RemoteInput;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::utils::{Logical, Point};

use super::Helix;

/// One parsed command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Input(Vec<RemoteInput>),
    Move(f64, f64),
    Click(f64, f64, u32),
}

fn number<T: std::str::FromStr>(word: Option<&str>) -> Result<T, String> {
    let word = word.ok_or("missing argument")?;
    word.parse().map_err(|_| format!("not a number: {word}"))
}

fn pressed(word: Option<&str>) -> Result<bool, String> {
    match word {
        Some("1" | "down") => Ok(true),
        Some("0" | "up") => Ok(false),
        other => Err(format!("expected 1 or 0, not {other:?}")),
    }
}

/// A command line as [`Command`]s.
pub fn parse(line: &str) -> Result<Command, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let mut words = rest.split_whitespace();
    let command = match verb {
        "key" => Command::Input(vec![RemoteInput::Key { keycode: number(words.next())?, pressed: pressed(words.next())? }]),
        "keysym" => {
            let word = words.next().ok_or("missing keysym")?;
            let keysym = match word.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16).map_err(|_| format!("not a keysym: {word}"))?,
                None => number(Some(word))?,
            };
            Command::Input(vec![RemoteInput::Keysym { keysym, pressed: pressed(words.next())? }])
        }
        "type" => Command::Input(
            rest.chars()
                .flat_map(|c| {
                    let keysym = smithay::input::keyboard::xkb::utf32_to_keysym(c as u32).raw();
                    [RemoteInput::Keysym { keysym, pressed: true }, RemoteInput::Keysym { keysym, pressed: false }]
                })
                .collect(),
        ),
        "tap" => {
            let codes: Vec<u32> = words.by_ref().map(|w| number(Some(w))).collect::<Result<_, _>>()?;
            if codes.is_empty() {
                return Err("tap needs keys".into());
            }
            let down = codes.iter().map(|&keycode| RemoteInput::Key { keycode, pressed: true });
            let up = codes.iter().rev().map(|&keycode| RemoteInput::Key { keycode, pressed: false });
            Command::Input(down.chain(up).collect())
        }
        "move" => Command::Move(number(words.next())?, number(words.next())?),
        "button" => Command::Input(vec![RemoteInput::Button { button: number(words.next())?, pressed: pressed(words.next())? }]),
        "click" => {
            let (x, y) = (number(words.next())?, number(words.next())?);
            let button = words.next().map(|w| number(Some(w))).transpose()?.unwrap_or(0x110);
            Command::Click(x, y, button)
        }
        "scroll" => Command::Input(vec![
            RemoteInput::Axis { dx: number(words.next())?, dy: number(words.next())?, finish: false, source: 1 },
            RemoteInput::Axis { dx: 0.0, dy: 0.0, finish: true, source: 1 },
        ]),
        other => return Err(format!("unknown command {other:?}")),
    };
    if words.next().is_some() && verb != "type" {
        return Err("too many arguments".into());
    }
    Ok(command)
}

/// Listen on `path` for test input.
pub fn listen(event_loop: &mut EventLoop<'static, Helix>, path: &std::path::Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    tracing::info!(path = %path.display(), "headless test input");
    let handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(Generic::new(listener, Interest::READ, Mode::Level), move |_, listener, _| {
            while let Ok((stream, _)) = listener.accept() {
                if stream.set_nonblocking(true).is_err() {
                    continue;
                }
                let mut pending = Vec::new();
                let added = handle.insert_source(Generic::new(stream, Interest::READ, Mode::Level), move |_, stream, state: &mut Helix| {
                    // SAFETY: the stream stays in the source; it is not dropped here.
                    let stream = unsafe { stream.get_mut() };
                    Ok(state.headless_input_read(stream, &mut pending))
                });
                if let Err(err) = added {
                    tracing::warn!(%err, "cannot read test input");
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

impl Helix {
    /// Handle the complete lines that came on `stream` (`pending` keeps
    /// a partial one); remove the source at the end of the stream.
    fn headless_input_read(&mut self, stream: &mut UnixStream, pending: &mut Vec<u8>) -> PostAction {
        let mut chunk = [0u8; 4096];
        let end = loop {
            match stream.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) => pending.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break true,
            }
        };
        while let Some(at) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            let answer = match parse(&String::from_utf8_lossy(&line)) {
                Ok(command) => {
                    self.headless_command(command);
                    "ok\n".to_owned()
                }
                Err(err) => format!("error: {err}\n"),
            };
            let _ = stream.write_all(answer.as_bytes());
        }
        if end {
            PostAction::Remove
        } else {
            PostAction::Continue
        }
    }

    fn headless_command(&mut self, command: Command) {
        tracing::debug!(?command, "test input");
        match command {
            Command::Input(inputs) => {
                for input in inputs {
                    self.remote_input(input);
                }
            }
            Command::Move(x, y) => self.headless_move(x, y),
            Command::Click(x, y, button) => {
                self.headless_move(x, y);
                self.remote_input(RemoteInput::Button { button, pressed: true });
                self.remote_input(RemoteInput::Button { button, pressed: false });
            }
        }
        let _ = self.display_handle.flush_clients();
    }

    fn headless_move(&mut self, x: f64, y: f64) {
        self.note_activity();
        let pos = self.clamp_to_outputs(Point::<f64, Logical>::from((x, y)));
        let time = self.now_ms();
        self.pointer_to(pos, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse("key 30 1\n"), Ok(Command::Input(vec![RemoteInput::Key { keycode: 30, pressed: true }])));
        assert_eq!(parse("keysym 0xff0d 0"), Ok(Command::Input(vec![RemoteInput::Keysym { keysym: 0xff0d, pressed: false }])));
        assert_eq!(
            parse("type a b"),
            Ok(Command::Input(vec![
                RemoteInput::Keysym { keysym: 0x61, pressed: true },
                RemoteInput::Keysym { keysym: 0x61, pressed: false },
                RemoteInput::Keysym { keysym: 0x20, pressed: true },
                RemoteInput::Keysym { keysym: 0x20, pressed: false },
                RemoteInput::Keysym { keysym: 0x62, pressed: true },
                RemoteInput::Keysym { keysym: 0x62, pressed: false },
            ]))
        );
        assert_eq!(
            parse("tap 29 46"),
            Ok(Command::Input(vec![
                RemoteInput::Key { keycode: 29, pressed: true },
                RemoteInput::Key { keycode: 46, pressed: true },
                RemoteInput::Key { keycode: 46, pressed: false },
                RemoteInput::Key { keycode: 29, pressed: false },
            ]))
        );
        assert_eq!(parse("move 10 20.5"), Ok(Command::Move(10.0, 20.5)));
        assert_eq!(parse("click 1 2"), Ok(Command::Click(1.0, 2.0, 0x110)));
        assert_eq!(parse("click 1 2 274"), Ok(Command::Click(1.0, 2.0, 274)));
        assert!(parse("key 30").is_err());
        assert!(parse("key 30 1 2").is_err());
        assert!(parse("warp 1 2").is_err());
    }
}
