use std::io::Read;
use std::time::Duration;

#[derive(Debug)]
pub enum InputEvent {
    KeyUp,
    KeyDown,
    KeyEnter,
    KeyQuit,
    MouseClick { y: u32 },
    Resize,
    None,
}

/// Enable SGR mouse mode.
pub fn enable_mouse() {
    print!("\x1b[?1000h\x1b[?1006h");
}

/// Disable SGR mouse mode.
pub fn disable_mouse() {
    print!("\x1b[?1000l\x1b[?1006l");
}

/// Poll stdin for input with a timeout. Returns the parsed event.
pub fn poll_input(timeout: Duration) -> InputEvent {
    let fd = libc::STDIN_FILENO;

    let ready = unsafe {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pfd, 1, timeout.as_millis() as i32)
    };

    if ready < 0 {
        // poll was interrupted by a signal (EINTR) — likely SIGWINCH
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EINTR {
            return InputEvent::Resize;
        }
        return InputEvent::None;
    }

    if ready == 0 {
        return InputEvent::None;
    }

    let mut buf = [0u8; 64];
    let n = std::io::stdin().lock().read(&mut buf).unwrap_or(0);
    if n == 0 {
        return InputEvent::None;
    }

    parse_input(&buf[..n])
}

fn parse_input(buf: &[u8]) -> InputEvent {
    if buf.is_empty() {
        return InputEvent::None;
    }

    // Single byte keys
    if buf.len() == 1 {
        return match buf[0] {
            b'q' | b'Q' => InputEvent::KeyQuit,
            b'k' => InputEvent::KeyUp,
            b'j' => InputEvent::KeyDown,
            13 => InputEvent::KeyEnter, // Enter
            _ => InputEvent::None,
        };
    }

    // Escape sequences
    if buf[0] == 0x1b && buf.len() >= 3 && buf[1] == b'[' {
        // Arrow keys: ESC [ A/B
        if buf.len() == 3 {
            return match buf[2] {
                b'A' => InputEvent::KeyUp,
                b'B' => InputEvent::KeyDown,
                _ => InputEvent::None,
            };
        }

        // SGR mouse: ESC [ < button ; x ; y M/m
        if buf[2] == b'<' {
            return parse_sgr_mouse(&buf[3..]);
        }
    }

    InputEvent::None
}

/// Parse SGR mouse event: "button;x;yM" or "button;x;ym"
fn parse_sgr_mouse(buf: &[u8]) -> InputEvent {
    let s = std::str::from_utf8(buf).unwrap_or("");

    // Only handle press events (ending with 'M'), not release ('m')
    if !s.ends_with('M') {
        return InputEvent::None;
    }

    let s = &s[..s.len() - 1]; // strip trailing M
    let parts: Vec<&str> = s.split(';').collect();
    if parts.len() != 3 {
        return InputEvent::None;
    }

    let button: u32 = parts[0].parse().unwrap_or(u32::MAX);
    let _x: u32 = parts[1].parse().unwrap_or(0);
    let y: u32 = parts[2].parse().unwrap_or(0);

    // Button 0 = left click
    if button == 0 {
        InputEvent::MouseClick { y }
    } else {
        InputEvent::None
    }
}

/// Convert a click y-coordinate to an agent index.
/// Header = 3 rows. All items = 5 rows each (margin+name+cwd+activity+margin).
pub fn click_to_agent_index(y: u32, agent_count: usize, _selected: usize) -> Option<usize> {
    use crate::sidebar::render::{HEADER_ROWS, ITEM_ROWS};
    if y <= HEADER_ROWS || agent_count == 0 {
        return None;
    }
    let idx = ((y - HEADER_ROWS - 1) / ITEM_ROWS) as usize;
    if idx < agent_count { Some(idx) } else { None }
}
