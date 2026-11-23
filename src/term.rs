//! Terminal setup and teardown.
//!
//! vortex runs in the alternate screen with raw mode on.  The cursor is *never*
//! hidden — vim users need to see where they are in normal and visual modes —
//! only its shape changes with the mode.

use std::io::{self, IsTerminal, Write};
use std::panic;
use std::time::{Duration, Instant};

use crossterm::{ExecutableCommand, execute};
use ratatui::crossterm as ct;

use crate::error::{Error, Result};
use crate::theme::{Observed, Rgb};

pub type Backend = ratatui::backend::CrosstermBackend<io::Stdout>;
pub type Terminal = ratatui::Terminal<Backend>;

/// Cursor shape requested by the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    /// normal / visual: a block over the character under the cursor.
    Block,
    /// insert: a bar before the character under the cursor.
    Bar,
    /// command / pending operator: an underline.
    Underline,
}

/// Restores the terminal when dropped, so early returns and `?` are safe.
#[derive(Debug)]
pub struct TerminalGuard {
    entered: bool,
    mouse: bool,
}

impl TerminalGuard {
    /// Enter the alternate screen and raw mode.
    pub fn enter(mouse: bool) -> Result<Self> {
        let mut out = io::stdout().lock();
        ct::terminal::enable_raw_mode().map_err(|e| Error::Terminal(e.to_string()))?;
        let mut enable_mouse = mouse;
        if enable_mouse && std::env::var("VORTEX_NO_MOUSE").is_ok() {
            enable_mouse = false;
        }
        execute!(
            out,
            ct::terminal::EnterAlternateScreen,
            ct::event::EnableBracketedPaste,
            ct::cursor::Hide,
        )
        .map_err(|e| Error::Terminal(e.to_string()))?;
        if enable_mouse {
            execute!(out, ct::event::EnableMouseCapture)
                .map_err(|e| Error::Terminal(e.to_string()))?;
        }
        io::stdout().flush().ok();
        Ok(Self {
            entered: true,
            mouse: enable_mouse,
        })
    }

    /// Whether mouse capture was actually enabled.
    pub fn mouse_enabled(&self) -> bool {
        self.mouse
    }

    fn leave(&mut self) {
        if !self.entered {
            return;
        }
        self.entered = false;
        let mut out = io::stdout().lock();
        // Best effort: if the terminal is gone there is nothing to restore.
        let _ = execute!(
            out,
            ct::event::DisableMouseCapture,
            ct::cursor::Show,
            ct::cursor::SetCursorStyle::DefaultUserShape,
            ct::terminal::LeaveAlternateScreen,
        );
        let _ = ct::terminal::disable_raw_mode();
        let _ = io::stdout().flush();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Open a terminal for drawing.  Call after [`TerminalGuard::enter`].
pub fn terminal() -> Result<Terminal> {
    let stdout = io::stdout();
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| Error::Terminal(e.to_string()))
}

/// Change the cursor shape.  Safe to call every frame; writes are skipped when
/// the shape is unchanged.
/// How long to wait for the terminal to answer the colour query.
///
/// Long enough for a link with some latency, short enough that a terminal which
/// never answers does not make startup feel slow.
pub const COLOR_QUERY_TIMEOUT: Duration = Duration::from_millis(120);

/// A gap this long in arriving bytes means the terminal is done answering.
const REPLY_QUIET: Duration = Duration::from_millis(30);

/// How long to keep swallowing bytes that are still on their way, so a reply
/// the query gave up on cannot stall startup.
const SWALLOW_LIMIT: Duration = Duration::from_millis(300);

/// Ask the terminal for its own background and foreground colours (OSC 11/10).
///
/// This reads raw bytes from stdin, so it has to run while vortex still owns
/// the keyboard — after raw mode is on, before the input reader starts.  If
/// anything is already waiting to be read it declines to ask rather than steal
/// a keystroke, and a terminal that never answers costs only
/// [`COLOR_QUERY_TIMEOUT`].
pub fn query_colors(timeout: Duration) -> Observed {
    let mut observed = Observed::UNKNOWN;
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return observed;
    }
    if std::env::var_os("TERM").is_some_and(|term| term == "dumb") {
        return observed;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return observed;
    }
    if stdin_readable(Duration::ZERO) {
        return observed;
    }

    // The BEL form of the query is answered by the widest set of terminals;
    // the reply comes back as `ESC ] 11 ; rgb:rrrr/gggg/bbbb BEL`.
    let mut out = io::stdout();
    if write!(out, "\x1b]11;?\x07\x1b]10;?\x07")
        .and_then(|()| out.flush())
        .is_err()
    {
        return observed;
    }

    let deadline = Instant::now() + timeout;
    let mut reply: Vec<u8> = Vec::with_capacity(64);
    let mut any_reply = false;
    loop {
        // How long to wait for the next byte.  The overall timeout governs the
        // wait for the first answer; once bytes are arriving, a 30 ms gap means
        // the terminal is done.  Stopping as soon as one report is in would
        // leave the other one in the tty buffer, where the input reader hands it
        // back as typed characters — `11;rgb:1a1a/…` appearing on the screen is
        // exactly what a half-read colour query looks like.
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Two questions were asked, so wait for both answers: a terminal that
        // takes a moment over the second report leaves it in the tty buffer,
        // and the input reader hands those bytes back as typed characters.  The
        // short idle gap only applies once there is nothing left to wait for.
        let owed = observed.background.is_none() || observed.foreground.is_none();
        let wait = if any_reply && !owed {
            REPLY_QUIET.min(remaining)
        } else {
            remaining
        };
        if wait.is_zero() || !stdin_readable(wait) {
            break;
        }
        let Some(byte) = read_byte() else { break };
        any_reply = true;
        reply.push(byte);
        // Reports end with BEL, or with the string terminator `ESC \`.
        let complete = byte == 0x07 || reply.ends_with(b"\x1b\\");
        if !complete {
            if reply.len() > 96 {
                // Not a report at all: stop reading rather than swallow input.
                reply.clear();
                break;
            }
            continue;
        }
        match parse_report(&reply) {
            Some((10, rgb)) => observed.foreground = Some(rgb),
            Some((_, rgb)) => observed.background = Some(rgb),
            None => {}
        }
        reply.clear();
        if observed.background.is_some() && observed.foreground.is_some() {
            break;
        }
    }
    // Bytes we walked away from are still on their way.  This is the only place
    // they can be caught: the tail of a report carries no `ESC ]`, so once the
    // input reader turns it into keys nothing downstream can tell it from
    // typing — and in normal mode `rgb:` opens the command line and paints the
    // channels in it.
    if any_reply && (observed.background.is_none() || observed.foreground.is_none()) {
        swallow_leftovers();
    }
    observed
}

/// Read and throw away whatever is still arriving, until the tty goes quiet.
#[cfg(unix)]
fn swallow_leftovers() {
    let deadline = Instant::now() + SWALLOW_LIMIT;
    while Instant::now() < deadline && stdin_readable(REPLY_QUIET) {
        if read_byte().is_none() {
            break;
        }
    }
}

#[cfg(not(unix))]
fn swallow_leftovers() {}

/// Parse `ESC ] <index> ; <colour> BEL|ST`, the reply to a colour query.
pub fn parse_report(bytes: &[u8]) -> Option<(u16, Rgb)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let payload = text
        .strip_prefix("\u{1b}]")?
        .trim_end_matches("\u{1b}\\")
        .trim_end_matches('\u{7}');
    let (index, value) = payload.split_once(';')?;
    let index: u16 = index.trim().parse().ok()?;
    Some((index, parse_color_value(value.trim())?))
}

/// `rgb:1234/5678/9abc`, `#123456`, with the optional VT340 `1;` prefix.
fn parse_color_value(value: &str) -> Option<Rgb> {
    let value = value.strip_prefix("1;").unwrap_or(value);
    let parts: Vec<&str> = if let Some(rest) = value.strip_prefix("rgb:") {
        rest.split('/').collect()
    } else if let Some(hex) = value.strip_prefix('#')
        && hex.len() == 6
    {
        vec![&hex[0..2], &hex[2..4], &hex[4..6]]
    } else {
        return None;
    };
    if parts.len() != 3 {
        return None;
    }
    Some((
        scale_channel(parts[0])?,
        scale_channel(parts[1])?,
        scale_channel(parts[2])?,
    ))
}

/// Terminals report one to four hex digits per channel; scale to 0..=255.
fn scale_channel(hex: &str) -> Option<u8> {
    let hex = hex.trim();
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len())) - 1;
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

/// Read exactly one byte from stdin, unbuffered.
///
/// This must not go through `io::stdin()`'s buffer: `poll` reports on the file
/// descriptor, and a buffered read would swallow the rest of the reply into a
/// userspace buffer that `poll` can no longer see.
#[cfg(unix)]
fn read_byte() -> Option<u8> {
    use std::os::fd::AsRawFd;
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: writing one byte into `byte`, on a descriptor we own.
        let read = unsafe {
            libc::read(
                io::stdin().as_raw_fd(),
                byte.as_mut_ptr().cast::<libc::c_void>(),
                1,
            )
        };
        if read == 1 {
            return Some(byte[0]);
        }
        if read < 0 && interrupted() {
            // A resize lands here as a signal; giving up now would leave half a
            // report in the tty buffer for the input reader to hand back.
            continue;
        }
        return None;
    }
}

/// Did the last call fail only because a signal arrived?
#[cfg(unix)]
fn interrupted() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

#[cfg(not(unix))]
fn read_byte() -> Option<u8> {
    None
}

/// Is stdin ready within `timeout`?  Polling, so nothing is consumed.
#[cfg(unix)]
fn stdin_readable(timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    // A signal makes `poll` come back early, which is not the same answer as
    // "nothing is waiting".  A resize right after entering the alternate screen
    // is the usual candidate, and treating it as a timeout abandons a reply
    // half-read.
    let until = Instant::now() + timeout;
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        let mut descriptor = libc::pollfd {
            fd: io::stdin().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: polling one descriptor that we do not read unless poll says it
        // is ready.  `stdin` is valid for the whole call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            if interrupted() && remaining > Duration::ZERO {
                continue;
            }
            return false;
        }
        return ready > 0 && descriptor.revents & libc::POLLIN != 0;
    }
}

/// Without `poll` there is no way to ask without blocking, so do not try.
#[cfg(not(unix))]
fn stdin_readable(_timeout: Duration) -> bool {
    true
}

pub fn set_cursor_shape(shape: CursorShape) {
    use ct::cursor::SetCursorStyle as S;
    if LAST_SHAPE.with(|last| {
        let previous = last.get();
        if previous == shape {
            false
        } else {
            last.set(shape);
            true
        }
    }) {
        let style = match shape {
            CursorShape::Block => S::BlinkingBlock,
            CursorShape::Bar => S::BlinkingBar,
            CursorShape::Underline => S::BlinkingUnderScore,
        };
        let mut out = io::stdout();
        let _ = out.execute(style);
        let _ = out.flush();
    }
}

std::thread_local! {
    static LAST_SHAPE: std::cell::Cell<CursorShape> = const { std::cell::Cell::new(CursorShape::Underline) };
}

/// Show the terminal cursor (it is only hidden while a pane has nowhere to put
/// it, or while the picker owns it).
pub fn show_cursor() {
    set_cursor_visible(true);
}

/// Hide the terminal cursor.
pub fn hide_cursor() {
    set_cursor_visible(false);
}

fn set_cursor_visible(visible: bool) {
    let changed = LAST_VISIBLE.with(|last| {
        let previous = last.get();
        if previous == visible {
            false
        } else {
            last.set(visible);
            true
        }
    });
    if !changed {
        return;
    }
    let mut out = io::stdout();
    if visible {
        let _ = execute!(out, ct::cursor::Show);
    } else {
        let _ = execute!(out, ct::cursor::Hide);
    }
    let _ = out.flush();
}

std::thread_local! {
    static LAST_VISIBLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Enter the alternate screen and raw mode.
pub fn enter_terminal(mouse: bool) -> Result<TerminalGuard> {
    TerminalGuard::enter(mouse)
}

/// Leave the alternate screen (the guard does this too; this is for explicit
/// teardown before printing to the shell).
pub fn leave_terminal() {
    let mut out = io::stdout().lock();
    let _ = execute!(
        out,
        ct::event::DisableMouseCapture,
        ct::cursor::Show,
        ct::cursor::SetCursorStyle::DefaultUserShape,
        ct::terminal::LeaveAlternateScreen,
    );
    let _ = ct::terminal::disable_raw_mode();
    let _ = io::stdout().flush();
}

/// Turn mouse capture on or off (`:set mouse`).  Returns whether it is on now.
pub fn set_mouse_capture(enable: bool) -> bool {
    let result = if enable {
        execute!(io::stdout(), ct::event::EnableMouseCapture)
    } else {
        execute!(io::stdout(), ct::event::DisableMouseCapture)
    };
    result.is_ok()
}

/// Copy to the terminal clipboard with OSC-52, base64 encoded.
///
/// Terminals that disallow clipboard writes simply ignore the sequence, so this
/// is best-effort by design.
pub fn set_clipboard(text: &str) {
    let encoded = base64(text.as_bytes());
    let mut out = io::stdout();
    let _ = out.write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes());
    let _ = out.flush();
}

/// Minimal standard-alphabet base64, so clipboard support costs no dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let value = (buffer[0] as u32) << 16 | (buffer[1] as u32) << 8 | buffer[2] as u32;
        let codes = [
            (value >> 18) & 63,
            (value >> 12) & 63,
            (value >> 6) & 63,
            value & 63,
        ];
        for (index, code) in codes.iter().enumerate() {
            let used = index < chunk.len() + 1;
            if used {
                out.push(ALPHABET[*code as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The cursor shape that matches a mode.
pub fn cursor_shape_for(mode: crate::input::Mode) -> CursorShape {
    match mode {
        crate::input::Mode::Insert => CursorShape::Bar,
        crate::input::Mode::Command => CursorShape::Underline,
        crate::input::Mode::Normal | crate::input::Mode::Visual(_) => CursorShape::Block,
    }
}

/// Make a panic leave the terminal in a usable state.
pub fn install_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout().lock();
        let _ = execute!(
            out,
            ct::event::DisableMouseCapture,
            ct::cursor::Show,
            ct::cursor::SetCursorStyle::DefaultUserShape,
            ct::terminal::LeaveAlternateScreen,
        );
        let _ = ct::terminal::disable_raw_mode();
        let _ = io::stdout().flush();
        hook(info);
    }));
}

/// A printable rendering of a key event, in the kebab-case spelling `[keymap.*]` uses.
pub fn describe_key(key: ct::event::KeyEvent) -> String {
    let mut out = String::new();
    let modifiers = key.modifiers;
    for (name, bit) in [
        ("ctrl", ct::event::KeyModifiers::CONTROL),
        ("alt", ct::event::KeyModifiers::ALT),
        ("shift", ct::event::KeyModifiers::SHIFT),
    ] {
        if modifiers.contains(bit) {
            out.push_str(name);
            out.push('-');
        }
    }
    use ct::event::KeyCode as K;
    let code = match key.code {
        K::Char(c) => c.to_string(),
        K::Enter => "enter".into(),
        K::Tab => "tab".into(),
        K::BackTab => "shift-tab".into(),
        K::Backspace => "backspace".into(),
        K::Delete => "delete".into(),
        K::Esc => "esc".into(),
        K::Up => "up".into(),
        K::Down => "down".into(),
        K::Left => "left".into(),
        K::Right => "right".into(),
        K::Home => "home".into(),
        K::End => "end".into(),
        K::PageUp => "pageup".into(),
        K::PageDown => "pagedown".into(),
        K::Insert => "insert".into(),
        K::F(n) => format!("f{n}"),
        K::Null => "nul".into(),
        K::Media(_)
        | K::Modifier(_)
        | K::Menu
        | K::KeypadBegin
        | K::PrintScreen
        | K::Pause
        | K::CapsLock
        | K::ScrollLock
        | K::NumLock => "unknown".into(),
    };
    out.push_str(&code);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(text: &str) -> Option<(u16, Rgb)> {
        parse_report(text.as_bytes())
    }

    #[test]
    fn parses_a_background_report() {
        // xterm answers with four hex digits per channel.
        assert_eq!(
            report("\x1b]11;rgb:1010/2020/3030\x07"),
            Some((11, (16, 32, 48)))
        );
        assert_eq!(
            report("\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some((11, (255, 255, 255)))
        );
    }

    #[test]
    fn parses_the_string_terminator_and_the_vt340_form() {
        assert_eq!(
            report("\x1b]10;1;rgb:ffff/0000/8080\x1b\\"),
            Some((10, (255, 0, 128)))
        );
    }

    #[test]
    fn short_hex_and_hash_forms_scale_up() {
        assert_eq!(report("\x1b]11;rgb:f/8/0\x07"), Some((11, (255, 136, 0))));
        assert_eq!(report("\x1b]11;#ff8800\x07"), Some((11, (255, 136, 0))));
    }

    #[test]
    fn anything_else_is_not_a_colour() {
        assert_eq!(report("\x1b]11;rgb:zz/0/0\x07"), None);
        assert_eq!(report("\x1b]11;rgb:1234/5678\x07"), None);
        assert_eq!(report("\x1b]nope;rgb:1/2/3\x07"), None);
        assert_eq!(report("not a report"), None);
        // A truncated reply (no terminator yet) still parses the payload, but a
        // stray keystroke does not.
        assert_eq!(report("\x1b"), None);
    }

    #[test]
    fn a_dark_background_is_dark() {
        let observed = Observed {
            background: Some((30, 30, 30)),
            foreground: None,
        };
        assert_eq!(observed.scheme(), crate::theme::Scheme::Dark);
    }
}
