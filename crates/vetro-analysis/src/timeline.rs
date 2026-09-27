//! Input→effects timeline (M7, ADR 0023): the user's inputs
//! (keys, clicks and touches, console lines, file manager
//! commands) with their instruction number, and the effects that follow
//! (network requests, file writes seen by the observation, console
//! output), all in guest time in microseconds (10 ns per
//! instruction: `at_us = instructions / 100`, the same time as the frames of
//! [`crate::net::capture`]).
//!
//! **Attribution (heuristic).** An effect is attributed to the last
//! input preceding it (at the same instant it counts: the input reaches the
//! guest before the next instruction) if it is within `window_us`; otherwise
//! it stays without a cause. For network and file effects only *command*
//! inputs count (Enter, click, touch, file manager command, power
//! button): a character typed in the middle of a line does not "cause" a request.
//! For console output any input counts (the echo of a key is
//! the effect of that key). It is not a true causal relation (the guest
//! can make requests on its own during the window, for example a
//! DHCP renewal): it is the same approximation as the tools that line up
//! actions and traffic, and the window is configurable.
//!
//! No dependencies and deterministic, like the rest of the crate (compiles for
//! wasm32).

use std::fmt::Write as _;

use crate::net::dns;
use crate::net::inspector::NetworkAnalysis;
use crate::net::json::quote_into;

/// Default attribution window: 3 s of guest time.
pub const DEFAULT_WINDOW_US: u64 = 3_000_000;

/// Maximum inputs and effects kept (the oldest are dropped).
pub const MAX_INPUTS: usize = 20_000;
pub const MAX_EFFECTS: usize = 50_000;

/// Text characters kept for a console effect.
pub const CONSOLE_TEXT: usize = 160;

/// Microseconds of guest time at an instruction number.
pub fn step_us(step: u64) -> u64 {
    step / 100
}

/// Kind of user input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputKind {
    /// Keyboard key (virtio-input).
    Key,
    /// Pointer button (tablet).
    Pointer,
    /// Touchscreen contact.
    Touch,
    /// Byte to the serial console.
    Console,
    /// File manager command (from the user, not the panel
    /// updates).
    Files,
    /// Power button (GPIO).
    Power,
    /// Resolution requested for the screen.
    Display,
    Other,
}

impl InputKind {
    pub const ALL: [InputKind; 8] = [
        InputKind::Key,
        InputKind::Pointer,
        InputKind::Touch,
        InputKind::Console,
        InputKind::Files,
        InputKind::Power,
        InputKind::Display,
        InputKind::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            InputKind::Key => "key",
            InputKind::Pointer => "pointer",
            InputKind::Touch => "touch",
            InputKind::Console => "console",
            InputKind::Files => "file",
            InputKind::Power => "power",
            InputKind::Display => "display",
            InputKind::Other => "other",
        }
    }

    /// Numeric code (for the vetro-wasm C API): the position in
    /// [`InputKind::ALL`].
    pub fn code(self) -> u32 {
        Self::ALL.iter().position(|&k| k == self).unwrap_or(7) as u32
    }

    pub fn from_code(c: u32) -> InputKind {
        Self::ALL.get(c as usize).copied().unwrap_or(InputKind::Other)
    }
}

/// A user input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    /// Instruction number at which it reached the guest.
    pub step: u64,
    pub at_us: u64,
    pub kind: InputKind,
    pub label: String,
    /// Not a command (a character in the middle of a line, a resolution
    /// change): it does not cause network or file effects.
    pub weak: bool,
    /// Arrival order in the timeline (set by [`Timeline`]).
    pub seq: u64,
}

impl UserInput {
    pub fn new(step: u64, kind: InputKind, label: impl Into<String>, weak: bool) -> Self {
        UserInput { step, at_us: step_us(step), kind, label: label.into(), weak, seq: 0 }
    }
}

/// Kind of effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectKind {
    /// HTTP request (from the network inspector).
    Http,
    /// DNS query.
    Dns,
    /// TLS connection (name from the ClientHello).
    Tls,
    /// File created, written, moved or deleted (observation by the
    /// file manager).
    File,
    /// Console output.
    Console,
}

impl EffectKind {
    pub const ALL: [EffectKind; 5] =
        [EffectKind::Http, EffectKind::Dns, EffectKind::Tls, EffectKind::File, EffectKind::Console];

    pub fn name(self) -> &'static str {
        match self {
            EffectKind::Http => "http",
            EffectKind::Dns => "dns",
            EffectKind::Tls => "tls",
            EffectKind::File => "file",
            EffectKind::Console => "console",
        }
    }

    pub fn code(self) -> u32 {
        Self::ALL.iter().position(|&k| k == self).unwrap_or(0) as u32
    }

    pub fn from_code(c: u32) -> Option<EffectKind> {
        Self::ALL.get(c as usize).copied()
    }

    /// Do weak inputs cause it too?
    fn any_input(self) -> bool {
        self == EffectKind::Console
    }
}

/// An observed effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    pub at_us: u64,
    pub kind: EffectKind,
    pub label: String,
    /// Reference in its view (index of the request in the inspector).
    pub detail: Option<usize>,
    /// Bytes (console output, response body).
    pub bytes: u64,
    /// Arrival order in the timeline (set by [`Timeline`]): at the same
    /// instant, an input that arrived after the effect is not its cause
    /// (console output read at the end of a quantum comes before
    /// the inputs given at that boundary). `u64::MAX` for effects
    /// computed separately (network): they come after the inputs of their instant.
    pub seq: u64,
}

impl Effect {
    pub fn new(at_us: u64, kind: EffectKind, label: impl Into<String>) -> Self {
        Effect { at_us, kind, label: label.into(), detail: None, bytes: 0, seq: u64::MAX }
    }
}

/// The cause of an effect at instant `at_us` that arrived `seq`-th:
/// the index of the last input preceding it (earlier instant, or equal and
/// arrived before) and within `window_us` (only command inputs if
/// `strong_only`). `inputs` in (instant, arrival) order.
pub fn cause(inputs: &[UserInput], at_us: u64, seq: u64, window_us: u64, strong_only: bool) -> Option<usize> {
    let end = inputs.partition_point(|i| (i.at_us, i.seq) < (at_us, seq));
    inputs[..end]
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, i)| at_us - i.at_us <= window_us)
        .find(|(_, i)| !strong_only || !i.weak)
        .map(|(k, _)| k)
}

/// The printable text of console bytes (controls as `⏎`, `⌫`,
/// `^X`; escape sequences removed).
pub fn printable(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ parameters final letter; otherwise one character.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' | '\n' => out.push('⏎'),
            '\x7f' | '\x08' => out.push('⌫'),
            '\t' => out.push('⇥'),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(b'@' + c as u8));
            }
            c => out.push(c),
        }
    }
    out
}

/// Rebuilds the lines typed at the console (as a shell in canonical
/// mode sees them): characters appended, `DEL`/`BS` remove the last one, `^U` and
/// `^C` clear the line, CR or LF close it; escape sequences (arrows)
/// are ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEditor {
    line: String,
    esc: u8,
}

impl LineEditor {
    /// Adds typed bytes; returns the closed lines.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut done = Vec::new();
        for c in String::from_utf8_lossy(bytes).chars() {
            match (self.esc, c) {
                (1, '[') => self.esc = 2,
                (1, _) => self.esc = 0,
                (2, c) if ('@'..='~').contains(&c) => self.esc = 0,
                (2, _) => {}
                (_, '\x1b') => self.esc = 1,
                (_, '\r' | '\n') => done.push(core::mem::take(&mut self.line)),
                (_, '\x7f' | '\x08') => {
                    self.line.pop();
                }
                (_, '\x15' | '\x03') => self.line.clear(),
                (_, c) if (c as u32) < 0x20 => {}
                (_, c) => self.line.push(c),
            }
        }
        done
    }

    /// The line in progress.
    pub fn current(&self) -> &str {
        &self.line
    }
}

/// Name of a Linux key (most common `KEY_*` and `BTN_*`).
pub fn key_name(code: u16) -> String {
    const ROW1: &str = "1234567890";
    const QWERTY: [(u16, &str); 3] = [(16, "QWERTYUIOP"), (30, "ASDFGHJKL"), (44, "ZXCVBNM")];
    let named = match code {
        1 => "Esc",
        12 => "-",
        13 => "=",
        14 => "Backspace",
        15 => "Tab",
        26 => "[",
        27 => "]",
        28 => "Enter",
        29 => "Ctrl",
        39 => ";",
        40 => "'",
        41 => "`",
        42 => "Shift",
        43 => "\\",
        51 => ",",
        52 => ".",
        53 => "/",
        54 => "Right Shift",
        56 => "Alt",
        57 => "Space",
        58 => "CapsLock",
        96 => "Enter (keypad)",
        97 => "Right Ctrl",
        100 => "AltGr",
        102 => "Home",
        103 => "Up",
        104 => "PgUp",
        105 => "Left",
        106 => "Right",
        107 => "End",
        108 => "Down",
        109 => "PgDn",
        110 => "Ins",
        111 => "Del",
        116 => "Power",
        125 => "Meta",
        158 => "Back",
        172 => "Home page",
        0x110 => "left click",
        0x111 => "right click",
        0x112 => "middle click",
        0x14a => "touch",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if (2..=11).contains(&code) {
        return ROW1[(code - 2) as usize..(code - 1) as usize].to_string();
    }
    for (base, row) in QWERTY {
        if code >= base && code < base + row.len() as u16 {
            let i = (code - base) as usize;
            return row[i..=i].to_string();
        }
    }
    if (59..=68).contains(&code) {
        return format!("F{}", code - 58);
    }
    format!("key {code}")
}

/// The Enter key (`KEY_ENTER`, `KEY_KPENTER`): closes a command.
pub fn is_enter(code: u16) -> bool {
    code == 28 || code == 96
}

/// The effects of a network analysis: an HTTP request at the start of the
/// request (first byte, or SYN/DNS of the first one on the connection), a
/// DNS query, a TLS connection.
pub fn network_effects(a: &NetworkAnalysis) -> Vec<Effect> {
    let mut v = Vec::new();
    for x in &a.http {
        let status = x.status().map_or_else(|| "no response".to_string(), |s| s.to_string());
        v.push(Effect {
            detail: Some(x.index),
            bytes: x.response.as_ref().map_or(0, |r| r.body.decoded.len() as u64),
            ..Effect::new(
                x.timings.started_us,
                EffectKind::Http,
                format!("{} {} → {status}", x.request.method, x.url),
            )
        });
    }
    for d in &a.dns {
        let addrs: Vec<String> = d.ipv4().map(|a| a.to_string()).collect();
        let answer = if addrs.is_empty() { String::new() } else { format!(" → {}", addrs.join(", ")) };
        v.push(Effect::new(
            d.query_at,
            EffectKind::Dns,
            format!("DNS {} {}{answer}", dns::type_name(d.qtype), d.name),
        ));
    }
    for t in &a.tls {
        v.push(Effect::new(
            t.started_us,
            EffectKind::Tls,
            format!("TLS {} ({})", t.sni.as_deref().unwrap_or("no SNI"), t.server),
        ));
    }
    v
}

/// Non-network inputs and effects of a session (the network ones are
/// derived from the capture when needed: [`network_effects`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeline {
    inputs: Vec<UserInput>,
    effects: Vec<Effect>,
    /// Inputs and effects dropped because beyond the limits.
    pub dropped: u64,
    /// Grows at every change (for whoever redraws only when needed).
    pub version: u64,
    /// The last thing added is the console effect at the end: the output
    /// that follows joins it.
    console_open: bool,
    /// Arrival counter (`seq` of inputs and effects).
    seq: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Inserts `x` keeping the order by `key` (at the end among equals).
fn insert_sorted<T>(v: &mut Vec<T>, x: T, key: impl Fn(&T) -> u64) {
    let k = key(&x);
    let at = v.partition_point(|y| key(y) <= k);
    v.insert(at, x);
}

impl Timeline {
    pub fn new() -> Self {
        Timeline {
            inputs: Vec::new(),
            effects: Vec::new(),
            dropped: 0,
            version: 0,
            console_open: false,
            seq: 0,
        }
    }

    pub fn inputs(&self) -> &[UserInput] {
        &self.inputs
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    pub fn clear(&mut self) {
        self.inputs.clear();
        self.effects.clear();
        self.dropped = 0;
        self.version += 1;
        self.console_open = false;
    }

    /// Adds an input (in time order).
    pub fn push_input(&mut self, mut i: UserInput) {
        self.seq += 1;
        i.seq = self.seq;
        insert_sorted(&mut self.inputs, i, |i| i.at_us);
        if self.inputs.len() > MAX_INPUTS {
            self.inputs.remove(0);
            self.dropped += 1;
        }
        self.version += 1;
        self.console_open = false;
    }

    /// Adds an effect (in time order).
    pub fn push_effect(&mut self, mut e: Effect) {
        self.seq += 1;
        e.seq = self.seq;
        insert_sorted(&mut self.effects, e, |e| e.at_us);
        if self.effects.len() > MAX_EFFECTS {
            self.effects.remove(0);
            self.dropped += 1;
        }
        self.version += 1;
        self.console_open = false;
    }

    /// Console output at instant `at_us`: it joins the last
    /// console effect if in the meantime there were no inputs nor
    /// other effects, otherwise it is a new effect.
    pub fn push_console(&mut self, at_us: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = printable(bytes);
        if let Some(e) = self.effects.last_mut()
            && self.console_open
            && e.kind == EffectKind::Console
            && e.at_us <= at_us
        {
            e.bytes += bytes.len() as u64;
            let room = CONSOLE_TEXT.saturating_sub(e.label.chars().count());
            e.label.extend(text.chars().take(room));
            self.version += 1;
            return;
        }
        self.push_effect(Effect {
            bytes: bytes.len() as u64,
            ..Effect::new(at_us, EffectKind::Console, text.chars().take(CONSOLE_TEXT).collect::<String>())
        });
        self.console_open = true;
    }

    /// All effects (these plus `extra`, for example the network ones) in
    /// time order, with the cause of each.
    pub fn attributed(&self, extra: &[Effect], window_us: u64) -> Vec<(Effect, Option<usize>)> {
        let mut all: Vec<Effect> = self.effects.iter().chain(extra).cloned().collect();
        all.sort_by_key(|e| (e.at_us, e.seq, e.kind));
        all.into_iter()
            .map(|e| {
                let c = cause(&self.inputs, e.at_us, e.seq, window_us, !e.kind.any_input());
                (e, c)
            })
            .collect()
    }

    /// The timeline as JSON for the web app:
    ///
    /// ```json
    /// {"windowUs":3000000,"dropped":0,"version":7,
    ///  "inputs":[{"i":0,"step":123400,"atUs":1234,"kind":"console",
    ///             "label":"Enter: wget ...","weak":false,"effects":3}],
    ///  "effects":[{"atUs":1300,"kind":"http","label":"POST http://... → 200",
    ///              "ref":0,"bytes":0,"cause":0}]}
    /// ```
    pub fn to_json(&self, extra: &[Effect], window_us: u64) -> String {
        let effects = self.attributed(extra, window_us);
        let mut count = vec![0u64; self.inputs.len()];
        for (_, c) in &effects {
            if let Some(c) = c {
                count[*c] += 1;
            }
        }
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"windowUs\":{window_us},\"dropped\":{},\"version\":{},\"inputs\":[",
            self.dropped, self.version
        );
        for (k, i) in self.inputs.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"i\":{k},\"step\":{},\"atUs\":{},\"kind\":\"{}\",\"label\":",
                i.step,
                i.at_us,
                i.kind.name()
            );
            quote_into(&mut out, &i.label);
            let _ = write!(out, ",\"weak\":{},\"effects\":{}}}", i.weak, count[k]);
        }
        out.push_str("],\"effects\":[");
        for (k, (e, c)) in effects.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"atUs\":{},\"kind\":\"{}\",\"label\":", e.at_us, e.kind.name());
            quote_into(&mut out, &e.label);
            out.push_str(",\"ref\":");
            match e.detail {
                Some(d) => {
                    let _ = write!(out, "{d}");
                }
                None => out.push_str("null"),
            }
            let _ = write!(out, ",\"bytes\":{},\"cause\":", e.bytes);
            match c {
                Some(c) => {
                    let _ = write!(out, "{c}");
                }
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::inspector::tests::session;
    use crate::net::json::{self, Value};

    fn input(step: u64, label: &str, weak: bool) -> UserInput {
        UserInput::new(step, InputKind::Console, label, weak)
    }

    fn effect(at_us: u64, kind: EffectKind) -> Effect {
        Effect::new(at_us, kind, "")
    }

    /// The last input not after the effect, within the window; at the
    /// same instant the input comes first; weak ones only for the console.
    #[test]
    fn causa_entro_la_finestra() {
        let mut v = [input(100_000, "a", false), input(200_000, "b", true), input(300_000, "c", false)];
        for (k, i) in v.iter_mut().enumerate() {
            i.seq = k as u64 + 1;
        }
        assert_eq!((v[0].at_us, v[2].at_us), (1_000, 3_000));
        let m = u64::MAX;
        assert_eq!(cause(&v, 999, m, 10_000, false), None, "before any input");
        assert_eq!(cause(&v, 1_000, m, 10_000, false), Some(0), "same instant, computed separately");
        assert_eq!(cause(&v, 1_000, 1, 10_000, false), None, "same instant, arrived before the input");
        assert_eq!(cause(&v, 1_000, 2, 10_000, false), Some(0), "same instant, arrived after");
        assert_eq!(cause(&v, 2_500, m, 10_000, false), Some(1));
        assert_eq!(cause(&v, 2_500, m, 10_000, true), Some(0), "the weak one does not cause");
        assert_eq!(cause(&v, 2_500, m, 1_000, true), None, "the command is outside the window");
        assert_eq!(cause(&v, 3_000 + 10_000, m, 10_000, true), Some(2), "on the edge");
        assert_eq!(cause(&v, 3_000 + 10_001, m, 10_000, true), None);
        assert_eq!(cause(&[], 5, m, 10, false), None);
    }

    /// Console output joins until an input or another effect
    /// arrives; network effects are attributed to the command.
    #[test]
    fn console_unita_e_attribuzione() {
        let mut t = Timeline::new();
        t.push_console(10, b"login\r\n");
        t.push_console(20, b"# ");
        assert_eq!(t.effects().len(), 1);
        assert_eq!(t.effects()[0].label, "login⏎# ");
        assert_eq!(t.effects()[0].bytes, 9);
        t.push_input(input(3_000, "w", true));
        t.push_console(31, b"w");
        t.push_input(input(4_000, "Enter: wget x", false));
        t.push_console(41, b"\r\n");
        t.push_effect(Effect { label: "file".into(), ..effect(45, EffectKind::File) });
        t.push_console(50, b"done");
        // Output read at 60 before an input given at 60: it is not its own (it
        // joins the earlier one), and the output after the input is new.
        t.push_console(60, b"before");
        t.push_input(input(6_000, "after", false));
        t.push_console(60, b"echo");
        assert_eq!(t.effects().len(), 6, "{:?}", t.effects());
        let net = [Effect { detail: Some(0), ..effect(42, EffectKind::Http) }, effect(35, EffectKind::Dns)];
        let a = t.attributed(&net, 1_000);
        let got: Vec<(u64, &str, Option<usize>)> =
            a.iter().map(|(e, c)| (e.at_us, e.kind.name(), *c)).collect();
        assert_eq!(
            got,
            [
                (10, "console", None),
                (31, "console", Some(0)),
                (35, "dns", None),
                (41, "console", Some(1)),
                (42, "http", Some(1)),
                (45, "file", Some(1)),
                (50, "console", Some(1)),
                (60, "console", Some(2)),
            ]
        );
        let j = json::parse(t.to_json(&net, 1_000).as_bytes()).expect("valid JSON");
        let Some(Value::Array(inputs)) = j.get("inputs") else { panic!() };
        assert_eq!(inputs[1].get("effects"), Some(&Value::Number("4".into())));
        assert_eq!(inputs[1].get("label").and_then(Value::as_str), Some("Enter: wget x"));
        assert_eq!(inputs[0].get("weak"), Some(&Value::Bool(true)));
        let Some(Value::Array(effects)) = j.get("effects") else { panic!() };
        assert_eq!(effects[4].get("ref"), Some(&Value::Number("0".into())));
        assert_eq!(effects[4].get("cause"), Some(&Value::Number("1".into())));
        assert_eq!(effects[0].get("cause"), Some(&Value::Null));
        let v = t.version;
        t.clear();
        assert!(t.version > v && t.inputs().is_empty() && t.effects().is_empty());
    }

    /// Out-of-order inputs are put in their place; beyond the limits the
    /// oldest are dropped.
    #[test]
    fn ordine_e_limiti() {
        let mut t = Timeline::new();
        t.push_input(input(500, "b", false));
        t.push_input(input(100, "a", false));
        assert_eq!(t.inputs()[0].label, "a");
        for k in 0..MAX_INPUTS as u64 {
            t.push_input(input(1_000 + k, "x", false));
        }
        assert_eq!(t.inputs().len(), MAX_INPUTS);
        assert_eq!(t.dropped, 2);
        assert_eq!(t.inputs()[0].step, 1_000);
    }

    #[test]
    fn righe_della_console() {
        let mut l = LineEditor::default();
        assert!(l.feed(b"wgex\x7ft").is_empty());
        assert_eq!(l.current(), "wget");
        assert_eq!(l.feed(b" -q\x1b[D\x1b[C http://a\r"), ["wget -q http://a"]);
        assert_eq!(l.feed(b"ls\x15pwd\none\rtwo"), ["pwd", "one"]);
        assert_eq!(l.current(), "two");
        assert!(l.feed(b"\x03").is_empty());
        assert_eq!(l.current(), "");
        assert_eq!(l.feed("città\r".as_bytes()), ["città"]);
    }

    #[test]
    fn testo_stampabile_e_tasti() {
        assert_eq!(printable(b"a\x1b[6nb\r\nc\x7f\x01\t"), "ab⏎c⌫^A⇥");
        assert_eq!(key_name(30), "A");
        assert_eq!(key_name(2), "1");
        assert_eq!(key_name(11), "0");
        assert_eq!(key_name(28), "Enter");
        assert_eq!(key_name(50), "M");
        assert_eq!(key_name(60), "F2");
        assert_eq!(key_name(0x110), "left click");
        assert_eq!(key_name(999), "key 999");
        assert!(is_enter(28) && is_enter(96) && !is_enter(30));
        for k in InputKind::ALL {
            assert_eq!(InputKind::from_code(k.code()), k);
        }
        assert_eq!(InputKind::from_code(99), InputKind::Other);
        for k in EffectKind::ALL {
            assert_eq!(EffectKind::from_code(k.code()), Some(k));
        }
    }

    /// The network effects of a session's analysis: the DNS query and
    /// the two requests, with the index in the inspector.
    #[test]
    fn effetti_dall_analisi_di_rete() {
        let a = NetworkAnalysis::from_frames(&session());
        let e = network_effects(&a);
        let labels: Vec<&str> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "GET http://api.example:8080/v1/items?id=7&q=a+b → 200",
                "POST http://api.example:8080/v1/form → 204",
                "DNS A api.example → 198.18.0.1",
            ]
        );
        assert_eq!((e[0].at_us, e[0].detail, e[0].bytes), (1_000, Some(0), 13));
        assert_eq!((e[1].at_us, e[1].detail), (4_000, Some(1)));
        assert_eq!(e[2].at_us, 1_000);
    }
}
