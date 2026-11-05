//! Scripted input in a simple key-line syntax, read by OunceCore itself (`--ounce-input`).
//!
//! A line is `key[+key][~hold seconds]` (hold 0.15 s by default). Keys: `A B X Y LS RS L R ZL ZR Plus Minus Left Up Right
//! Down` (NpadButton bits 0..15), `LLeft LUp LRight LDown` / `RLeft RUp RRight RDown` (a stick pushed fully) and
//! `L<x>/<y>` / `R<x>/<y>` (a stick tilted part way, each -1..1, y up). Any other line (`shot <path>`, `dump`, ...) is
//! not input: the host acts on it.

#![allow(dead_code)]

/// One key line: the controller state to hold, and for how long.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyStep {
    pub buttons: u64,
    pub lx: i32,
    pub ly: i32,
    pub rx: i32,
    pub ry: i32,
    pub hold_secs: f64,
}

pub const KEY_NAMES: [&str; 16] =
    ["A", "B", "X", "Y", "LS", "RS", "L", "R", "ZL", "ZR", "Plus", "Minus", "Left", "Up", "Right", "Down"];
const MAX: i32 = 32767;

/// Parses a key line; `None` for lines that are not input (commands, comments, blank lines, unknown keys).
pub fn parse_key_line(line: &str) -> Option<KeyStep> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.contains(' ') {
        return None;
    }
    let (keys, hold) = match line.split_once('~') {
        Some((k, h)) => (k, h.parse::<f64>().ok()?),
        None => (line, 0.15),
    };
    let mut st = KeyStep { buttons: 0, lx: 0, ly: 0, rx: 0, ry: 0, hold_secs: hold };
    for k in keys.split('+') {
        if let Some(b) = KEY_NAMES.iter().position(|n| n.eq_ignore_ascii_case(k)) {
            st.buttons |= 1 << b;
            continue;
        }
        let dir = |d: &str| match d.to_ascii_lowercase().as_str() {
            "left" => Some((-MAX, 0)),
            "up" => Some((0, MAX)),
            "right" => Some((MAX, 0)),
            "down" => Some((0, -MAX)),
            _ => None,
        };
        let (stick, rest) = k.split_at(k.len().min(1));
        let left = match stick {
            "L" | "l" => true,
            "R" | "r" => false,
            _ => return None,
        };
        let (x, y) = if let Some(xy) = dir(rest) {
            xy
        } else {
            let (fx, fy) = rest.split_once('/')?;
            let (fx, fy) = (fx.parse::<f32>().ok()?, fy.parse::<f32>().ok()?);
            if fx.abs() > 1.0 || fy.abs() > 1.0 {
                return None;
            }
            ((fx * MAX as f32).round() as i32, (fy * MAX as f32).round() as i32)
        };
        if left {
            (st.lx, st.ly) = (x, y);
        } else {
            (st.rx, st.ry) = (x, y);
        }
    }
    Some(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dm_ctl_key_lines() {
        assert_eq!(parse_key_line("A").unwrap().buttons, 1);
        let s = parse_key_line("LUp~3").unwrap();
        assert_eq!((s.buttons, s.lx, s.ly, s.hold_secs), (0, 0, MAX, 3.0));
        let s = parse_key_line("Y+ZR~0.5").unwrap();
        assert_eq!(s.buttons, (1 << 3) | (1 << 9));
        let s = parse_key_line("R-0.5/1").unwrap();
        assert_eq!((s.rx, s.ry), (-16384, MAX));
        assert!(parse_key_line("shot /tmp/a.png").is_none());
        assert!(parse_key_line("dump").is_none());
        assert!(parse_key_line("# note").is_none());
    }
}
