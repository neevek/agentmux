use std::io::Read;
use std::time::Duration;

#[derive(Debug)]
pub enum InputEvent {
    KeyUp,
    KeyDown,
    KeyEnter,
    KeyQuit,
    MouseClick { y: u32 },
    MouseScrollUp,
    MouseScrollDown,
    Resize,
    None,
}

pub fn enable_mouse() {
    // 1000 = button events, 1002 = button + motion, 1006 = SGR extended mode
    print!("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
}

pub fn disable_mouse() {
    print!("\x1b[?1000l\x1b[?1002l\x1b[?1006l");
}

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

    if buf.len() == 1 {
        return match buf[0] {
            b'q' | b'Q' => InputEvent::KeyQuit,
            b'k' => InputEvent::KeyUp,
            b'j' => InputEvent::KeyDown,
            13 => InputEvent::KeyEnter,
            _ => InputEvent::None,
        };
    }

    if buf[0] == 0x1b && buf.len() >= 3 && buf[1] == b'[' {
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

fn parse_sgr_mouse(buf: &[u8]) -> InputEvent {
    let s = std::str::from_utf8(buf).unwrap_or("");

    // Only handle press events (ending with 'M'), not release ('m')
    if !s.ends_with('M') {
        return InputEvent::None;
    }

    let s = &s[..s.len() - 1];
    let parts: Vec<&str> = s.split(';').collect();
    if parts.len() != 3 {
        return InputEvent::None;
    }

    let button: u32 = parts[0].parse().unwrap_or(u32::MAX);
    let _x: u32 = parts[1].parse().unwrap_or(0);
    let y: u32 = parts[2].parse().unwrap_or(0);

    match button {
        0 => InputEvent::MouseClick { y },
        64 => InputEvent::MouseScrollUp,
        65 => InputEvent::MouseScrollDown,
        _ => InputEvent::None,
    }
}

/// Convert a click y-coordinate to an agent index, accounting for scroll offset
/// and adaptive item heights.
pub fn click_to_agent_index(
    y: u32,
    agents: &[crate::detect::AgentInfo],
    scroll_offset: usize,
) -> Option<usize> {
    use crate::sidebar::render::{HEADER_ROWS, item_row_count};
    if y <= HEADER_ROWS || agents.is_empty() {
        return None;
    }
    let click_row = y - HEADER_ROWS;
    let mut cumulative = 0u32;
    for (vi, agent) in agents.iter().skip(scroll_offset).enumerate() {
        cumulative += item_row_count(agent);
        if click_row <= cumulative {
            return Some(scroll_offset + vi);
        }
    }
    None
}
