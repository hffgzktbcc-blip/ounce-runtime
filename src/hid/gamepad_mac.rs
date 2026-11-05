//! Physical controllers on the Mac: Apple's GameController.framework (src/hid/gc_mac.m) mapped to the Switch 2 layout.
//!
//! gc_mac.m reports face buttons by position (a Nintendo pad's labels already turned into positions), so the default
//! table puts each Switch button where a Nintendo game expects it: east A, south B, north X, west Y, whatever the pad
//! prints there (a DualSense's circle is A, its cross B). The table is
//! `~/Library/Application Support/OunceCore/controls.json`, written with the defaults on first use:
//! `{"buttons": {"<element>": "<Switch button>" | "", ...}, "deadzone": 0.12, "trigger": 0.4}` with the elements of
//! [`ELEMENTS`] and the Switch buttons of the DM_CTL key names (`A B X Y LS RS L R ZL ZR Plus Minus Left Up Right Down`).

use super::script::KEY_NAMES;
use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

#[repr(C)]
#[derive(Clone, Copy)]
struct GcState {
    pressed: u32,
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    lt: f32,
    rt: f32,
    generation: u32,
    connected: i32,
    name: [c_char; 128],
}

unsafe extern "C" {
    fn ounce_gc_start();
    fn ounce_gc_poll(s: *mut GcState) -> i32;
    fn ounce_gc_synthetic(name: *const c_char);
    fn ounce_gc_synthetic_set(pressed: u32, lx: f32, ly: f32, rx: f32, ry: f32);
}

/// gc_mac.m's elements, bit n of its `pressed` mask.
pub const ELEMENTS: [&str; 18] = [
    "south", "east", "west", "north", "l1", "r1", "l2", "r2", "l3", "r3", "menu", "options", "dleft", "dup", "dright",
    "ddown", "home", "share",
];
const L2: usize = 6;
const R2: usize = 7;
/// Switch button per element by default ("" = unused): Nintendo's positions.
const DEFAULT_MAP: [&str; 18] =
    ["B", "A", "Y", "X", "L", "R", "ZL", "ZR", "LS", "RS", "Plus", "Minus", "Left", "Up", "Right", "Down", "", ""];
const MAX: f32 = 32767.0;

/// The remap table.
#[derive(Debug, Clone, PartialEq)]
pub struct Controls {
    /// NpadButton bits per element.
    pub map: [u64; 18],
    /// Radial stick dead zone, 0..1.
    pub deadzone: f32,
    /// Analog trigger travel that presses ZL / ZR, 0..1.
    pub trigger: f32,
}

fn button_bit(name: &str) -> Option<u64> {
    if name.is_empty() {
        return Some(0);
    }
    KEY_NAMES.iter().position(|n| n.eq_ignore_ascii_case(name)).map(|b| 1u64 << b)
}

impl Default for Controls {
    fn default() -> Self {
        Self { map: DEFAULT_MAP.map(|n| button_bit(n).unwrap()), deadzone: 0.12, trigger: 0.4 }
    }
}

impl Controls {
    pub fn file() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/OunceCore/controls.json")
    }

    /// Parses controls.json; a missing or unknown entry keeps its default (and is reported).
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut c = Self::default();
        let mut bad = Vec::new();
        let v: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return (c, vec![format!("not JSON ({e}): defaults used")]),
        };
        if let Some(b) = v.get("buttons").and_then(|b| b.as_object()) {
            for (k, val) in b {
                let Some(i) = ELEMENTS.iter().position(|e| e.eq_ignore_ascii_case(k)) else {
                    bad.push(format!("unknown element {k:?}"));
                    continue;
                };
                match val.as_str().and_then(button_bit) {
                    Some(bit) => c.map[i] = bit,
                    None => bad.push(format!("{k}: unknown Switch button {val}")),
                }
            }
        }
        if let Some(d) = v.get("deadzone").and_then(|d| d.as_f64()) {
            c.deadzone = (d as f32).clamp(0.0, 0.9);
        }
        if let Some(t) = v.get("trigger").and_then(|t| t.as_f64()) {
            c.trigger = (t as f32).clamp(0.05, 1.0);
        }
        (c, bad)
    }

    pub fn to_json(&self) -> String {
        let name = |bit: u64| if bit == 0 { "" } else { KEY_NAMES[bit.trailing_zeros() as usize] };
        let buttons: serde_json::Map<String, serde_json::Value> =
            ELEMENTS.iter().zip(self.map).map(|(e, b)| (e.to_string(), name(b).into())).collect();
        let round = |f: f32| (f as f64 * 1000.0).round() / 1000.0;
        let v = serde_json::json!({ "buttons": buttons, "deadzone": round(self.deadzone), "trigger": round(self.trigger) });
        serde_json::to_string_pretty(&v).unwrap() + "\n"
    }

    /// controls.json, written with the defaults when there is none.
    pub fn load() -> Self {
        let path = Self::file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (c, bad) = Self::parse(&text);
                for b in bad {
                    println!("[OunceCore] controls.json: {b}");
                }
                println!("[OunceCore] controls: {}", path.display());
                c
            }
            Err(_) => {
                let c = Self::default();
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                match std::fs::write(&path, c.to_json()) {
                    Ok(()) => println!("[OunceCore] controls: wrote the default table to {}", path.display()),
                    Err(e) => println!("[OunceCore] controls: defaults ({}: {e})", path.display()),
                }
                c
            }
        }
    }

    fn stick(&self, x: f32, y: f32) -> (i32, i32) {
        let m = (x * x + y * y).sqrt();
        if m <= self.deadzone {
            return (0, 0);
        }
        // Rescaled past the dead zone so a small push still moves a little.
        let k = ((m - self.deadzone) / (1.0 - self.deadzone)).min(1.0) / m;
        ((x * k * MAX).round() as i32, (y * k * MAX).round() as i32)
    }

    /// One pad reading as Switch state: (buttons, lx, ly, rx, ry).
    fn apply(&self, s: &GcState) -> (u64, i32, i32, i32, i32) {
        let mut b = 0;
        for (i, bit) in self.map.iter().enumerate() {
            let on = match i {
                L2 => s.lt >= self.trigger,
                R2 => s.rt >= self.trigger,
                _ => s.pressed >> i & 1 != 0,
            };
            if on {
                b |= bit;
            }
        }
        let (lx, ly) = self.stick(s.lx, s.ly);
        let (rx, ry) = self.stick(s.rx, s.ry);
        (b, lx, ly, rx, ry)
    }
}

/// Player 1's pad, mapped. Hot-plug is followed by gc_mac.m; [`GcPads::poll`] logs each change of pad.
pub struct GcPads {
    controls: Controls,
    generation: u32,
    name: Option<String>,
}

/// A pad's mapped state: (buttons, lx, ly, rx, ry).
pub type PadState = (u64, i32, i32, i32, i32);

impl GcPads {
    pub fn new() -> Self {
        // SAFETY: plain C call; idempotent.
        unsafe { ounce_gc_start() };
        Self { controls: Controls::load(), generation: u32::MAX, name: None }
    }

    /// Player 1's pad state, or None with no pad.
    pub fn poll(&mut self) -> Option<PadState> {
        let mut s = std::mem::MaybeUninit::<GcState>::zeroed();
        // SAFETY: gc_mac.m fills the whole struct.
        let (have, s) = unsafe { (ounce_gc_poll(s.as_mut_ptr()) != 0, s.assume_init()) };
        if s.generation != self.generation {
            self.generation = s.generation;
            // SAFETY: gc_mac.m NUL-terminates the name (snprintf; zeroed when there is no pad).
            let name = have.then(|| unsafe { CStr::from_ptr(s.name.as_ptr()) }.to_string_lossy().into_owned());
            match &name {
                Some(n) => println!("[OunceCore] player 1 controller: {n} ({} connected)", s.connected),
                None => println!("[OunceCore] no controller: keyboard and DM_CTL only"),
            }
            self.name = name;
        }
        have.then(|| self.controls.apply(&s))
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl Default for GcPads {
    fn default() -> Self {
        Self::new()
    }
}

/// Connects a synthetic GameController pad (`Some(name)`) or disconnects it (`None`), through GameController's own
/// connect / disconnect notifications.
pub fn synthetic(name: Option<&str>) {
    let c = name.map(|n| CString::new(n).unwrap_or_default());
    // SAFETY: the string outlives the call.
    unsafe { ounce_gc_synthetic(c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())) }
}

/// Sets the synthetic pad's elements: `pressed` is a mask of [`ELEMENTS`] bits; sticks -1..1, y up.
pub fn synthetic_set(pressed: u32, l: (f32, f32), r: (f32, f32)) {
    // SAFETY: plain C call.
    unsafe { ounce_gc_synthetic_set(pressed, l.0, l.1, r.0, r.1) }
}

/// A synthetic pad line: elements joined by `+` (`east`, `south+l1`), sticks as DM_CTL has them (`LUp`, `L0.5/1`),
/// `~<hold seconds>` (0.15 s by default). Returns (pressed mask, left, right, hold).
pub fn parse_synthetic_line(line: &str) -> Option<(u32, (f32, f32), (f32, f32), f64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.contains(' ') {
        return None;
    }
    let (keys, hold) = match line.split_once('~') {
        Some((k, h)) => (k, h.parse::<f64>().ok()?),
        None => (line, 0.15),
    };
    let (mut pressed, mut l, mut r) = (0u32, (0.0, 0.0), (0.0, 0.0));
    for k in keys.split('+') {
        if let Some(i) = ELEMENTS.iter().position(|e| e.eq_ignore_ascii_case(k)) {
            pressed |= 1 << i;
            continue;
        }
        let st = super::script::parse_key_line(k)?;
        if st.buttons != 0 {
            return None;
        }
        if (st.lx, st.ly) != (0, 0) {
            l = (st.lx as f32 / MAX, st.ly as f32 / MAX);
        }
        if (st.rx, st.ry) != (0, 0) {
            r = (st.rx as f32 / MAX, st.ry as f32 / MAX);
        }
    }
    Some((pressed, l, r, hold))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::NpadButton;

    fn state(pressed: u32) -> GcState {
        GcState { pressed, lx: 0.0, ly: 0.0, rx: 0.0, ry: 0.0, lt: 0.0, rt: 0.0, generation: 0, connected: 1, name: [0; 128] }
    }

    #[test]
    fn nintendo_positions_by_default() {
        let c = Controls::default();
        let bit = |e: &str| 1u32 << ELEMENTS.iter().position(|x| *x == e).unwrap();
        assert_eq!(c.apply(&state(bit("east"))).0, NpadButton::A as u64);
        assert_eq!(c.apply(&state(bit("south"))).0, NpadButton::B as u64);
        assert_eq!(c.apply(&state(bit("north"))).0, NpadButton::X as u64);
        assert_eq!(c.apply(&state(bit("west"))).0, NpadButton::Y as u64);
        assert_eq!(c.apply(&state(bit("home"))).0, 0);
        let mut s = state(0);
        (s.rt, s.ly) = (0.9, 1.0);
        let (b, lx, ly, ..) = c.apply(&s);
        assert_eq!((b, lx, ly), (NpadButton::ZR as u64, 0, 32767));
        s.ly = 0.1; // inside the dead zone
        assert_eq!(c.apply(&s).2, 0);
    }

    #[test]
    fn controls_json_remaps_and_round_trips() {
        let (c, bad) = Controls::parse(r#"{"buttons": {"south": "A", "east": "B", "home": "Minus", "nope": "A", "west": "Q"}, "deadzone": 0.2}"#);
        assert_eq!(c.map[0], NpadButton::A as u64);
        assert_eq!(c.map[1], NpadButton::B as u64);
        assert_eq!(c.map[16], NpadButton::Minus as u64);
        assert_eq!(c.map[2], NpadButton::Y as u64); // bad value keeps the default
        assert_eq!(bad.len(), 2);
        assert_eq!(c.deadzone, 0.2);
        assert_eq!(Controls::parse(&Controls::default().to_json()).0, Controls::default());
    }

    #[test]
    fn synthetic_lines() {
        assert_eq!(parse_synthetic_line("east~0.5"), Some((2, (0.0, 0.0), (0.0, 0.0), 0.5)));
        assert_eq!(parse_synthetic_line("LUp+west~3"), Some((4, (0.0, 1.0), (0.0, 0.0), 3.0)));
        assert_eq!(parse_synthetic_line("connect Pad"), None);
        assert_eq!(parse_synthetic_line("A"), None); // a Switch button, not a pad element
    }
}
