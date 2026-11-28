//! Raw terminal input helpers: keys typed while the startup background query (OSC 11) owned
//! stdin, and the terminal's replies to that query. Pure functions (tested here).

use crate::tty::{KeyCode, KeyEvent, KeyEventKind, Modifiers};

/// Keys typed while the startup probe ran are in the same buffer as the terminal's replies:
/// drop every reply — OSC (`ESC ] … BEL|ST`), DCS (`ESC P … ST`, XTGETTCAP), the CSI replies
/// `ESC [ ? … c` (DA1), `ESC [ ? … $ y` (DECRPM), `ESC [ ? … u` (kitty keyboard flags) and the
/// cursor-position report `ESC [ row ; col R` — and keep the rest for replay.
pub fn strip_replies(buf: &[u8]) -> Vec<u8> {
    let st_end = |i: usize| -> usize {
        buf[i..]
            .iter()
            .position(|&b| b == 0x07)
            .map(|p| i + p + 1)
            .into_iter()
            .chain(buf[i..].windows(2).position(|w| w == b"\x1b\\").map(|p| i + p + 2))
            .min()
            .unwrap_or(buf.len())
    };
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\x1b]") || buf[i..].starts_with(b"\x1bP") || buf[i..].starts_with(b"\x1b_") {
            i = st_end(i);
        } else if buf[i..].starts_with(b"\x1b[") {
            // a CSI: parameters up to the final byte
            let fin = buf[i + 2..].iter().position(|b| (0x40..=0x7E).contains(b)).map(|p| i + 2 + p);
            match fin {
                Some(f) => {
                    let params = &buf[i + 2..f];
                    let reply = (params.first() == Some(&b'?') && matches!(buf[f], b'c' | b'y' | b'u'))
                        || (buf[f] == b't' && params.starts_with(b"6;"))
                        || (buf[f] == b'R'
                            && params.contains(&b';')
                            && params.iter().all(|b| b.is_ascii_digit() || *b == b';'));
                    if !reply {
                        out.extend_from_slice(&buf[i..=f]);
                    }
                    i = f + 1;
                }
                None => {
                    out.extend_from_slice(&buf[i..]);
                    i = buf.len();
                }
            }
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    out
}

/// What the startup probe learned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// DECRPM for mode 2026 (synchronized output): Some(supported)
    pub sync: Option<bool>,
    /// DECRPM for mode 2027 (grapheme-cluster widths)
    pub grapheme: Option<bool>,
    /// mode 2027 is on now (DECRPM 1 = set, 3 = permanently set): the terminal measures
    /// grapheme clusters (the width rule, `termcaps::width_method`)
    pub grapheme_set: Option<bool>,
    /// XTGETTCAP `RGB` / `Tc` answered as valid
    pub truecolor: Option<bool>,
    /// kitty keyboard protocol flags (answer to `CSI ? u`)
    pub kitty: Option<u32>,
    /// cursor row (1-based) from the cursor-position report
    pub cursor_row: Option<usize>,
    /// XTVERSION reply (`CSI > q` → `DCS > | name ST`): the terminal's name and version
    pub xtversion: Option<String>,
    /// DA1 attributes (`CSI ? 62; 4; … c`): 4 = sixel
    pub da1: Vec<u32>,
    /// cell size in pixels (`CSI 16 t` → `CSI 6 ; h ; w t`), width × height
    pub cell_px: Option<(u32, u32)>,
}

/// Terminals known to answer DECRQM / XTGETTCAP cleanly (others may echo unknown queries).
pub fn query_whitelisted(var: &dyn Fn(&str) -> Option<String>) -> bool {
    let term = var("TERM").unwrap_or_default();
    let prog = var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    var("KITTY_WINDOW_ID").is_some()
        || var("WT_SESSION").is_some()
        || ["kitty", "ghostty", "foot", "alacritty", "contour", "rio", "wezterm"].iter().any(|t| term.contains(t))
        || ["ghostty", "wezterm", "iterm.app", "vscode", "rio", "tabby", "contour"].contains(&prog.as_str())
}

/// OSC 11 dynamic background color query (ST-terminated).
pub const OSC11_QUERY: &[u8] = b"\x1b]11;?\x1b\\";

/// The probe sequence. Everything is fenced by DA1 (`ESC [ c`), which every terminal answers.
pub fn probe_query(osc11: bool, whitelisted: bool) -> Vec<u8> {
    let mut q = Vec::new();
    if osc11 {
        q.extend_from_slice(OSC11_QUERY);
    }
    if whitelisted {
        // DECRQM 2026 / 2027; XTGETTCAP RGB (hex 524742) and Tc (hex 5463)
        q.extend_from_slice(b"\x1b[?2026$p\x1b[?2027$p\x1bP+q524742\x1b\\\x1bP+q5463\x1b\\");
    }
    // XTVERSION and the cell's pixel size, kitty keyboard
    // flags query, cursor position report, DA1 fence (its attributes say whether sixel works)
    q.extend_from_slice(b"\x1b[>q\x1b[16t\x1b[?u\x1b[6n\x1b[c");
    q
}

/// The DA1 reply (`ESC [ ? … c`) fences the probe: every terminal answers it, after the rest.
pub fn probe_complete(buf: &[u8]) -> bool {
    buf.windows(3)
        .rposition(|w| w == b"\x1b[?")
        .is_some_and(|i| buf[i..].iter().skip(3).find(|b| (0x40..=0x7E).contains(*b)) == Some(&b'c'))
}

/// Parse the probe replies (any order, mixed with typed keys).
pub fn parse_probe(buf: &[u8]) -> Probe {
    let s = String::from_utf8_lossy(buf);
    let mut p = Probe::default();
    // CSI replies
    let mut rest: &str = &s;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let Some(f) = rest.find(|c: char| ('\x40'..='\x7e').contains(&c)) else { break };
        let (params, fin) = (&rest[..f], rest.as_bytes()[f]);
        match fin {
            b'y' if params.starts_with('?') && params.ends_with('$') => {
                let mut it = params[1..params.len() - 1].split(';');
                let (mode, ps) = (it.next().unwrap_or(""), it.next().and_then(|v| v.parse::<u8>().ok()));
                let ok = ps.map(|v| (1..=3).contains(&v));
                match mode {
                    "2026" => p.sync = ok,
                    "2027" => {
                        p.grapheme = ok;
                        p.grapheme_set = ps.map(|v| v == 1 || v == 3);
                    }
                    _ => {}
                }
            }
            b'u' if params.starts_with('?') => p.kitty = params[1..].parse().ok(),
            b'c' if params.starts_with('?') => {
                p.da1 = params[1..].split(';').filter_map(|v| v.parse().ok()).collect();
            }
            b't' if params.starts_with("6;") => {
                let mut it = params[2..].split(';').filter_map(|v| v.parse::<u32>().ok());
                if let (Some(h), Some(w)) = (it.next(), it.next()) {
                    p.cell_px = Some((w, h));
                }
            }
            b'R' => {
                if let Some((r, _)) = params.split_once(';') {
                    p.cursor_row = r.parse().ok();
                }
            }
            _ => {}
        }
        rest = &rest[f + 1..];
    }
    // XTVERSION: DCS > | text ST
    if let Some(i) = s.find("\x1bP>|") {
        let rest = &s[i + 4..];
        if let Some(j) = rest.find("\x1b\\") {
            p.xtversion = Some(rest[..j].chars().filter(|c| !c.is_control()).collect());
        }
    }
    // XTGETTCAP: DCS 1 + r <hex name> = <hex value> ST (valid) / DCS 0 + r … ST (invalid)
    for name in ["524742", "5463"] {
        if s.contains(&format!("\x1bP1+r{name}")) {
            p.truecolor = Some(true);
        } else if p.truecolor.is_none() && s.contains(&format!("\x1bP0+r{name}")) {
            p.truecolor = Some(false);
        }
    }
    p
}

/// Capability update extracted from an in-band `termina::Event` escape reply (late replies
/// arriving after the startup probe budget or replies to an in-band re-probe).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EscapeProbe {
    pub rgb: Option<[u8; 3]>,
    pub probe: Probe,
    pub saw_da1: bool,
}

/// Extract terminal capability / background / cursor-position data from a `tty::Event` escape reply.
pub fn parse_escape_event(ev: &crate::tty::Event) -> Option<EscapeProbe> {
    let crate::tty::Event::Termina(raw) = ev else {
        return None;
    };
    parse_termina_escape(raw)
}

/// Extract terminal capability / background / cursor-position data from a parsed `termina::Event`.
pub fn parse_termina_escape(ev: &termina::Event) -> Option<EscapeProbe> {
    use termina::escape::{csi, osc};
    let mut out = EscapeProbe::default();
    let mut any = false;
    match ev {
        termina::Event::Csi(csi::Csi::Keyboard(csi::Keyboard::ReportFlags(flags))) => {
            out.probe.kitty = Some(flags.bits() as u32);
            any = true;
        }
        termina::Event::Csi(csi::Csi::Cursor(csi::Cursor::ActivePositionReport { line, .. })) => {
            out.probe.cursor_row = Some(line.get() as usize);
            any = true;
        }
        termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportDecPrivateMode { mode, setting })) => {
            let code = match *mode {
                csi::DecPrivateMode::Code(c) => c as u16,
                csi::DecPrivateMode::Unspecified(n) => n,
            };
            let ok = matches!(
                setting,
                csi::DecModeSetting::Set | csi::DecModeSetting::Reset | csi::DecModeSetting::PermanentlySet
            );
            let set = matches!(setting, csi::DecModeSetting::Set | csi::DecModeSetting::PermanentlySet);
            match code {
                2026 => {
                    out.probe.sync = Some(ok);
                    any = true;
                }
                2027 => {
                    out.probe.grapheme = Some(ok);
                    out.probe.grapheme_set = Some(set);
                    any = true;
                }
                _ => {}
            }
        }
        termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(theme))) => {
            out.rgb = Some(match theme {
                csi::ThemeMode::Dark => [0, 0, 0],
                csi::ThemeMode::Light => [255, 255, 255],
            });
            any = true;
        }
        termina::Event::Csi(csi::Csi::Window(win)) => {
            if let csi::Window::ReportCellSizePixelsResponse { width: Some(w), height: Some(h) } = win.as_ref()
                && *w > 0
                && *h > 0
            {
                out.probe.cell_px = Some((*w as u32, *h as u32));
                any = true;
            }
        }
        termina::Event::Csi(csi::Csi::Device(csi::Device::DeviceAttributes(()))) => {
            out.saw_da1 = true;
            any = true;
        }
        termina::Event::Osc(osc::Osc::ChangeDynamicColors(osc::DynamicColorNumber::TextBackgroundColor, colors)) => {
            if let Some(osc::ColorOrQuery::Color(c)) = colors.first() {
                out.rgb = Some([c.red, c.green, c.blue]);
                any = true;
            }
        }
        _ => {}
    }
    any.then_some(out)
}

/// Decode raw terminal bytes buffered during the startup probe using `termina::Parser` (single
/// input decoder: preserves bracketed paste `Event::Paste`, `FocusIn`/`FocusOut`, mouse, and keys).
pub fn decode_events(bytes: &[u8]) -> Vec<crate::tty::Event> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut parser = termina::Parser::default();
    parser.parse(bytes, false);
    let mut out = Vec::new();
    while let Some(ev) = parser.pop() {
        let ev = crate::tty::Event::from(ev);
        if !ev.is_escape() {
            out.push(ev);
        }
    }
    out
}

/// Raw terminal input bytes → key events, for keys typed while the startup background query
/// owned stdin: text, Enter/Tab/Backspace, Ctrl+letter, Alt+key (ESC prefix), and the common
/// CSI / SS3 sequences (arrows, Home/End, Insert/Delete, PgUp/PgDn, with modifiers).
pub fn decode_keys(bytes: &[u8]) -> Vec<KeyEvent> {
    let s = String::from_utf8_lossy(bytes);
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let plain = |c: char| -> Option<KeyEvent> {
        Some(match c {
            '\r' | '\n' => KeyEvent::new(KeyCode::Enter, Modifiers::NONE),
            '\t' => KeyEvent::new(KeyCode::Tab, Modifiers::NONE),
            '\x7f' | '\x08' => KeyEvent::new(KeyCode::Backspace, Modifiers::NONE),
            c @ '\x01'..='\x1a' => KeyEvent::new(KeyCode::Char((b'a' + (c as u8) - 1) as char), Modifiers::CONTROL),
            c if c.is_control() => return None,
            c => KeyEvent::new(KeyCode::Char(c), Modifiers::NONE),
        })
    };
    // xterm / kitty modifier parameter: 1 + (shift 1 | alt 2 | ctrl 4 | super 8)
    let mods = |m: u32| {
        let m = m.saturating_sub(1);
        let mut k = Modifiers::NONE;
        if m & 1 != 0 {
            k |= Modifiers::SHIFT;
        }
        if m & 2 != 0 {
            k |= Modifiers::ALT;
        }
        if m & 4 != 0 {
            k |= Modifiers::CONTROL;
        }
        if m & 8 != 0 {
            k |= Modifiers::SUPER;
        }
        k
    };
    let csi_u_code = |cp: u32| -> Option<KeyCode> {
        Some(match cp {
            27 => KeyCode::Escape,
            13 => KeyCode::Enter,
            9 => KeyCode::Tab,
            127 | 8 => KeyCode::Backspace,
            c if c >= 0x20 && c != 0x7f && !(0xE000..=0xF8FF).contains(&c) => KeyCode::Char(char::from_u32(c)?),
            _ => return None,
        })
    };
    while i < cs.len() {
        let c = cs[i];
        if c != '\x1b' {
            if let Some(k) = plain(c) {
                out.push(k);
            }
            i += 1;
            continue;
        }
        match cs.get(i + 1) {
            // CSI: ESC [ (0x30..=0x3F)* (0x20..=0x2F)* (0x40..=0x7E)
            Some('[') => {
                let mut j = i + 2;
                while j < cs.len() && ('\x30'..='\x3f').contains(&cs[j]) {
                    j += 1;
                }
                while j < cs.len() && ('\x20'..='\x2f').contains(&cs[j]) {
                    j += 1;
                }
                let Some(&fin) = cs.get(j) else { break };
                if !('\x40'..='\x7e').contains(&fin) {
                    i = j;
                    continue;
                }
                let raw: String = cs[i + 2..j].iter().collect();
                // Private/intermediate sequences (mouse SGR `<`, kitty reply `?`, etc.) are discarded.
                if raw.bytes().any(|b| !b.is_ascii_digit() && b != b';' && b != b':') {
                    i = j + 1;
                    continue;
                }
                if fin == 'u' {
                    let mut fields = raw.split(';');
                    let code_field = fields.next().unwrap_or("");
                    let mod_field = fields.next();
                    let mut cp_parts = code_field.split(':');
                    let cp = cp_parts.next().and_then(|s| s.parse::<u32>().ok());
                    let shifted = cp_parts.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<u32>().ok());
                    let base = cp_parts.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<u32>().ok());

                    let (m_val, ev_type) = match mod_field {
                        Some(mf) => {
                            let mut mp = mf.split(':');
                            let mv =
                                mp.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
                            let et = mp.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<u32>().ok());
                            (mv, et)
                        }
                        None => (1, None),
                    };
                    let m = mods(m_val);
                    let use_shifted = m.contains(Modifiers::SHIFT)
                        && !m.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
                    let effective_cp = if use_shifted { shifted.or(cp) } else { cp };
                    if let Some(code) = effective_cp.and_then(csi_u_code) {
                        let base_key = base.and_then(char::from_u32).filter(|b| !b.is_control());
                        let mut ev = KeyEvent::new(code, m).with_base_layout_key(base_key);
                        ev.kind = match ev_type {
                            Some(2) => KeyEventKind::Repeat,
                            Some(3) => KeyEventKind::Release,
                            _ => KeyEventKind::Press,
                        };
                        out.push(ev);
                    }
                    i = j + 1;
                    continue;
                }
                let params: Vec<u32> = raw
                    .split(';')
                    .filter_map(|p| p.split(':').next().filter(|s| !s.is_empty()).and_then(|s| s.parse().ok()))
                    .collect();
                let m = mods(params.get(1).copied().unwrap_or(1));
                if fin == '~' && params.first() == Some(&27) && params.len() >= 3 {
                    if let Some(code) = csi_u_code(params[2]) {
                        out.push(KeyEvent::new(code, m));
                    }
                    i = j + 1;
                    continue;
                }
                let f1_to_4_prefix = params.is_empty() || params.first() == Some(&1);
                let code = match fin {
                    'A' => Some(KeyCode::Up),
                    'B' => Some(KeyCode::Down),
                    'C' => Some(KeyCode::Right),
                    'D' => Some(KeyCode::Left),
                    'H' => Some(KeyCode::Home),
                    'F' => Some(KeyCode::End),
                    'Z' => Some(KeyCode::BackTab),
                    'P' if f1_to_4_prefix => Some(KeyCode::Function(1)),
                    'Q' if f1_to_4_prefix => Some(KeyCode::Function(2)),
                    'R' if f1_to_4_prefix => Some(KeyCode::Function(3)),
                    'S' if f1_to_4_prefix => Some(KeyCode::Function(4)),
                    '~' => match params.first().copied().unwrap_or(0) {
                        1 | 7 => Some(KeyCode::Home),
                        2 => Some(KeyCode::Insert),
                        3 => Some(KeyCode::Delete),
                        4 | 8 => Some(KeyCode::End),
                        5 => Some(KeyCode::PageUp),
                        6 => Some(KeyCode::PageDown),
                        11 => Some(KeyCode::Function(1)),
                        12 => Some(KeyCode::Function(2)),
                        13 => Some(KeyCode::Function(3)),
                        14 => Some(KeyCode::Function(4)),
                        15 => Some(KeyCode::Function(5)),
                        17 => Some(KeyCode::Function(6)),
                        18 => Some(KeyCode::Function(7)),
                        19 => Some(KeyCode::Function(8)),
                        20 => Some(KeyCode::Function(9)),
                        21 => Some(KeyCode::Function(10)),
                        23 => Some(KeyCode::Function(11)),
                        24 => Some(KeyCode::Function(12)),
                        _ => None,
                    },
                    _ => None, // other sequences (focus, mouse, …) are dropped
                };
                if let Some(code) = code {
                    out.push(KeyEvent::new(code, m));
                }
                i = j + 1;
            }
            // SS3: ESC O [params] final (arrows, Home/End, F1-F4)
            Some('O') => {
                let mut j = i + 2;
                while j < cs.len() && (cs[j].is_ascii_digit() || cs[j] == ';') {
                    j += 1;
                }
                let Some(&fin) = cs.get(j) else { break };
                let params: Vec<u32> =
                    cs[i + 2..j].iter().collect::<String>().split(';').filter_map(|p| p.parse().ok()).collect();
                let m = mods(params.get(1).copied().unwrap_or(1));
                let code = match fin {
                    'A' => Some(KeyCode::Up),
                    'B' => Some(KeyCode::Down),
                    'C' => Some(KeyCode::Right),
                    'D' => Some(KeyCode::Left),
                    'H' => Some(KeyCode::Home),
                    'F' => Some(KeyCode::End),
                    'P' => Some(KeyCode::Function(1)),
                    'Q' => Some(KeyCode::Function(2)),
                    'R' => Some(KeyCode::Function(3)),
                    'S' => Some(KeyCode::Function(4)),
                    _ => None,
                };
                if let Some(code) = code {
                    out.push(KeyEvent::new(code, m));
                }
                i = j + 1;
            }
            // Alt+key
            Some(&n) if n != '\x1b' => {
                if let Some(mut k) = plain(n) {
                    k.modifiers |= Modifiers::ALT;
                    out.push(k);
                }
                i += 2;
            }
            _ => {
                out.push(KeyEvent::new(KeyCode::Escape, Modifiers::NONE));
                i += 1;
            }
        }
    }
    out
}

/// Output of [`EscJoin::push_with_replies`]: settled key events plus any reassembled
/// in-band terminal replies (`OSC`, `DCS`, `APC`, or `CSI` probe/CPR replies) that must
/// route to the terminal reply parser instead of key handling.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EscJoinOutput {
    pub keys: Vec<KeyEvent>,
    pub reply_bytes: Vec<Vec<u8>>,
}

/// A lone ESC waits briefly before it counts as the Esc key (MIT:
/// 10 ms escape timeout, longer once a sequence has started). termina decodes each read on
/// its own, so an escape sequence split across two reads arrives as Esc followed by plain
/// characters (`[`, `A`): this joins them back into the key they were, and buffers split
/// in-band replies (`OSC`/`DCS`/`APC`/`CSI`) so their bytes never leak as typed keys or `Ctrl+G`.
#[derive(Default)]
pub struct EscJoin {
    /// when the pending Esc (or latest sequence byte) arrived, and the characters after ESC so far
    pending: Option<(std::time::Instant, String)>,
    /// true once an active string reply (`OSC`/`DCS`/`APC`) exceeds `MAX_SEQ_LEN`; remaining
    /// body bytes are discarded until ST/BEL or timeout instead of leaking as editor keys.
    overflowed: bool,
}

impl EscJoin {
    /// How long a lone ESC waits for the rest of a sequence.
    pub const ESC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10);
    /// How long a started sequence (`ESC [` / `ESC ]` / `ESC P` / `ESC _`…) waits for its end.
    pub const SEQ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);
    /// Maximum bytes buffered for one split sequence before dropping a malformed/unterminated reply.
    pub const MAX_SEQ_LEN: usize = 1024;

    /// Feed one key event; returns the key events that are now settled, in order.
    pub fn push(&mut self, ev: KeyEvent, now: std::time::Instant) -> Vec<KeyEvent> {
        self.push_with_replies(ev, now).keys
    }

    /// Feed one key event; returns settled key events and any reassembled in-band terminal replies.
    pub fn push_with_replies(&mut self, ev: KeyEvent, now: std::time::Instant) -> EscJoinOutput {
        use crate::tty::KeyEventKind;
        let lone_esc = ev.code == KeyCode::Escape && ev.modifiers.is_empty() && ev.kind == KeyEventKind::Press;
        let Some((at, mut chars)) = self.pending.take() else {
            if lone_esc {
                self.pending = Some((now, String::new()));
                return EscJoinOutput::default();
            }
            return EscJoinOutput { keys: vec![ev], reply_bytes: Vec::new() };
        };
        let timeout = if chars.is_empty() { Self::ESC_TIMEOUT } else { Self::SEQ_TIMEOUT };
        let within_timeout = now <= at + timeout;

        // Buffered string reply (OSC `]`, DCS `P`, APC `_`) waiting for ST (`ESC \`) or BEL (`\x07`).
        if let Some(intro @ (']' | 'P' | '_')) = chars.chars().next() {
            if !within_timeout {
                self.overflowed = false;
                let mut out = EscJoinOutput { keys: Self::flush_chars(chars), reply_bytes: Vec::new() };
                let next = self.push_with_replies(ev, now);
                out.keys.extend(next.keys);
                out.reply_bytes.extend(next.reply_bytes);
                return out;
            }
            let press = ev.kind == KeyEventKind::Press;
            let is_bel = intro == ']'
                && press
                && ev.modifiers == Modifiers::CONTROL
                && matches!(ev.code, KeyCode::Char('g' | 'G'));
            let is_alt_st = press && ev.modifiers == Modifiers::ALT && ev.code == KeyCode::Char('\\');
            if chars.ends_with('\x1b') {
                if press && (ev.modifiers - Modifiers::SHIFT).is_empty() && ev.code == KeyCode::Char('\\') {
                    if self.overflowed {
                        self.overflowed = false;
                        return EscJoinOutput::default();
                    }
                    chars.push('\\');
                    return Self::finish_sequence(chars);
                }
                // Trailing ESC was not followed by `\`: drop truncated reply and treat ESC as fresh.
                self.overflowed = false;
                self.pending = Some((now, String::new()));
                return self.push_with_replies(ev, now);
            }
            if is_bel {
                if self.overflowed {
                    self.overflowed = false;
                    return EscJoinOutput::default();
                }
                chars.push('\x07');
                return Self::finish_sequence(chars);
            }
            if is_alt_st {
                if self.overflowed {
                    self.overflowed = false;
                    return EscJoinOutput::default();
                }
                chars.push_str("\x1b\\");
                return Self::finish_sequence(chars);
            }
            if lone_esc {
                if self.overflowed || chars.len() >= Self::MAX_SEQ_LEN {
                    self.overflowed = true;
                    chars.truncate(Self::MAX_SEQ_LEN.saturating_sub(1));
                }
                chars.push('\x1b');
                self.pending = Some((now, chars));
                return EscJoinOutput::default();
            }
            if let KeyCode::Char(c) = ev.code
                && press
                && (ev.modifiers - Modifiers::SHIFT).is_empty()
                && c.is_ascii()
                && !c.is_control()
            {
                if !self.overflowed && chars.len() + 1 < Self::MAX_SEQ_LEN {
                    chars.push(c);
                } else {
                    self.overflowed = true;
                }
                // Overlong reply keeps consuming until ST/BEL or timeout rather than leaking as editor text.
                self.pending = Some((now, chars));
                return EscJoinOutput::default();
            }
            self.overflowed = false;
            let mut out = EscJoinOutput { keys: Self::flush_chars(chars), reply_bytes: Vec::new() };
            let next = self.push_with_replies(ev, now);
            out.keys.extend(next.keys);
            out.reply_bytes.extend(next.reply_bytes);
            return out;
        }

        let joinable = match ev.code {
            KeyCode::Char(c) => {
                ev.kind == KeyEventKind::Press
                    && (ev.modifiers - Modifiers::SHIFT).is_empty()
                    && c.is_ascii()
                    && within_timeout
            }
            _ => false,
        };
        if !joinable {
            let mut out = EscJoinOutput { keys: Self::settle_all(chars), reply_bytes: Vec::new() };
            let next = self.push_with_replies(ev, now);
            out.keys.extend(next.keys);
            out.reply_bytes.extend(next.reply_bytes);
            return out;
        }
        if let KeyCode::Char(c) = ev.code {
            chars.push(c);
        }
        match Self::complete(&chars) {
            Some(true) => Self::finish_sequence(chars),
            Some(false) => EscJoinOutput { keys: Self::settle_all(chars), reply_bytes: Vec::new() },
            None => {
                if chars.len() < Self::MAX_SEQ_LEN {
                    self.pending = Some((now, chars));
                }
                EscJoinOutput::default()
            }
        }
    }

    fn finish_sequence(chars: String) -> EscJoinOutput {
        let raw = format!("\x1b{chars}");
        let stripped = strip_replies(raw.as_bytes());
        if stripped.is_empty() {
            return EscJoinOutput { keys: Vec::new(), reply_bytes: vec![raw.into_bytes()] };
        }
        EscJoinOutput { keys: decode_keys(&stripped), reply_bytes: Vec::new() }
    }

    /// Whether `ESC chars` is a complete sequence (Some(true)), can't be one (Some(false)),
    /// or may still be one (None).
    fn complete(chars: &str) -> Option<bool> {
        let mut it = chars.chars();
        match it.next()? {
            '[' => {
                let mut seen_intermediate = false;
                for c in it {
                    if !seen_intermediate && ('\x30'..='\x3f').contains(&c) {
                        continue;
                    }
                    if ('\x20'..='\x2f').contains(&c) {
                        seen_intermediate = true;
                        continue;
                    }
                    return Some(('\x40'..='\x7e').contains(&c));
                }
                None
            }
            'O' => {
                for c in it {
                    if c.is_ascii_digit() || c == ';' {
                        continue;
                    }
                    return Some(('\x40'..='\x7e').contains(&c));
                }
                None
            }
            // Known terminal string-reply introducers (`OSC`, `DCS`, `APC`): wait for ST or BEL.
            ']' | 'P' | '_' => None,
            // ESC + a printable key = Alt+key (legacy encoding)
            c if !c.is_control() => Some(true),
            _ => Some(false),
        }
    }

    /// Flush a pending sequence on timeout or non-joinable event: a lone string-reply introducer
    /// (`ESC ]`/`ESC P`/`ESC _`) with no body still settles as `Alt+<char>`, whereas a partial
    /// reply body that never terminated is dropped instead of leaking into the editor.
    fn flush_chars(chars: String) -> Vec<KeyEvent> {
        if matches!(chars.as_str(), "]" | "P" | "_") {
            return decode_keys(format!("\x1b{chars}").as_bytes());
        }
        if matches!(chars.chars().next(), Some(']' | 'P' | '_')) {
            return Vec::new();
        }
        Self::settle_all(chars)
    }

    /// Give up joining: the Esc key, then the characters as typed.
    fn settle_all(chars: String) -> Vec<KeyEvent> {
        let mut out = vec![KeyEvent::new(KeyCode::Escape, Modifiers::NONE)];
        out.extend(chars.chars().map(|c| KeyEvent::new(KeyCode::Char(c), Modifiers::NONE)));
        out
    }

    /// When the pending Esc must be settled even if nothing else arrives.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.pending
            .as_ref()
            .map(|(at, chars)| *at + if chars.is_empty() { Self::ESC_TIMEOUT } else { Self::SEQ_TIMEOUT })
    }

    /// Settle the pending Esc now (its deadline passed, or a non-key event arrived).
    pub fn flush(&mut self) -> Vec<KeyEvent> {
        self.overflowed = false;
        self.pending.take().map(|(_, chars)| Self::flush_chars(chars)).unwrap_or_default()
    }
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (1–4 hex digits per channel) → background RGB (0–255).
pub fn parse_osc11(reply: &str) -> Option<[u8; 3]> {
    let rgb = reply.split("rgb:").nth(1)?;
    let mut parts = rgb.split('/').map(|p| {
        let hex: String = p.chars().take_while(char::is_ascii_hexdigit).collect();
        let max = (1u32 << (4 * hex.len().clamp(1, 4))) - 1;
        u32::from_str_radix(&hex, 16).ok().map(|v| (v as f32 / max as f32 * 255.0).round() as u8)
    });
    Some([parts.next()??, parts.next()??, parts.next()??])
}

/// Dark background? (relative luminance < 0.5)
pub fn is_dark([r, g, b]: [u8; 3]) -> bool {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0 < 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tty::{KeyCode, Modifiers};

    #[test]
    fn decode_typed_keys() {
        // "hi", Left, Ctrl+Right, Alt+b, Delete, Ctrl+a, Enter
        let ks = decode_keys(b"hi\x1b[D\x1b[1;5C\x1bb\x1b[3~\x01\r");
        let got: Vec<(KeyCode, Modifiers)> = ks.iter().map(|k| (k.code, k.modifiers)).collect();
        assert_eq!(
            got,
            vec![
                (KeyCode::Char('h'), Modifiers::NONE),
                (KeyCode::Char('i'), Modifiers::NONE),
                (KeyCode::Left, Modifiers::NONE),
                (KeyCode::Right, Modifiers::CONTROL),
                (KeyCode::Char('b'), Modifiers::ALT),
                (KeyCode::Delete, Modifiers::NONE),
                (KeyCode::Char('a'), Modifiers::CONTROL),
                (KeyCode::Enter, Modifiers::NONE),
            ]
        );
    }

    #[test]
    fn probe_replies_are_parsed_and_stripped_from_typed_keys() {
        let buf = b"h\x1b[?2026;2$y\x1b[?2027;0$y\x1bP1+r524742=382F382F38\x1b\\i\x1b[?1u\x1b[12;1R\x1b[D\x1bP>|foot(1.16.2)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c";
        let p = parse_probe(buf);
        assert_eq!(
            p,
            Probe {
                sync: Some(true),
                grapheme: Some(false),
                grapheme_set: Some(false),
                truecolor: Some(true),
                kitty: Some(1),
                cursor_row: Some(12),
                xtversion: Some("foot(1.16.2)".into()),
                da1: vec![62, 4, 22],
                cell_px: Some((10, 20)),
            }
        );
        // typed `h`, `i` and the Left arrow survive; every reply is gone
        assert_eq!(strip_replies(buf), b"hi\x1b[D");
        assert_eq!(parse_probe(b"\x1b[3;7R\x1b[?62c").kitty, None, "no kitty answer: legacy keys");
        // mode 2027: 1 / 3 = on (clusters measured), 2 = known but off
        let on = parse_probe(b"\x1b[?2027;1$y\x1b[?62c");
        assert_eq!((on.grapheme, on.grapheme_set), (Some(true), Some(true)));
        let off = parse_probe(b"\x1b[?2027;2$y\x1b[?62c");
        assert_eq!((off.grapheme, off.grapheme_set), (Some(true), Some(false)));
    }

    #[test]
    fn decode_events_preserves_bracketed_paste_and_focus_without_splitting_lines() {
        let events = decode_events(b"a\x1b[200~line1\rline2\x1b[201~\x1b[O\x1b[I");
        assert!(
            matches!(
                events.as_slice(),
                [
                    crate::tty::Event::Key(k),
                    crate::tty::Event::Paste(p),
                    crate::tty::Event::FocusOut,
                    crate::tty::Event::FocusIn
                ] if k.code == KeyCode::Char('a') && p == "line1\rline2"
            ),
            "unexpected decoded events: {events:?}"
        );
    }

    #[test]
    fn parse_escape_event_extracts_late_probe_replies() {
        let mut parser = termina::Parser::default();
        parser
            .parse(b"\x1b[?1u\x1b[7;1R\x1b[?2026;1$y\x1b[?2027;1$y\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;4c", false);
        let mut got = EscapeProbe::default();
        while let Some(raw) = parser.pop() {
            let ev = crate::tty::Event::from(raw);
            if let Some(ep) = parse_escape_event(&ev) {
                if ep.rgb.is_some() {
                    got.rgb = ep.rgb;
                }
                if ep.probe.kitty.is_some() {
                    got.probe.kitty = ep.probe.kitty;
                }
                if ep.probe.cursor_row.is_some() {
                    got.probe.cursor_row = ep.probe.cursor_row;
                }
                if ep.probe.sync.is_some() {
                    got.probe.sync = ep.probe.sync;
                }
                if ep.probe.grapheme.is_some() {
                    got.probe.grapheme = ep.probe.grapheme;
                }
                if ep.probe.grapheme_set.is_some() {
                    got.probe.grapheme_set = ep.probe.grapheme_set;
                }
                got.saw_da1 |= ep.saw_da1;
            }
        }
        assert_eq!(got.probe.kitty, Some(1));
        assert_eq!(got.probe.cursor_row, Some(7));
        assert_eq!(got.probe.sync, Some(true));
        assert_eq!((got.probe.grapheme, got.probe.grapheme_set), (Some(true), Some(true)));
        assert_eq!(got.rgb, Some([255, 255, 255]));
        assert!(got.saw_da1);

        let cell_ev = crate::tty::Event::from(termina::Event::Csi(termina::escape::csi::Csi::Window(Box::new(
            termina::escape::csi::Window::ReportCellSizePixelsResponse { width: Some(10), height: Some(20) },
        ))));
        assert_eq!(parse_escape_event(&cell_ev).and_then(|e| e.probe.cell_px), Some((10, 20)));
    }

    #[test]
    fn the_probe_is_complete_once_the_da1_fence_arrived() {
        assert!(!probe_complete(b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?1u"));
        assert!(!probe_complete(b"\x1b[?2026;2$y"), "a DECRPM is not the fence");
        assert!(probe_complete(b"\x1b[?2026;2$y\x1b[12;1R\x1b[?62;22c"));
    }

    #[test]
    fn typed_bytes_survive_the_query() {
        let buf = b"go\x1b]11;rgb:0000/0000/0000\x1b\\\r\x1b[?62;22c";
        assert_eq!(strip_replies(buf), b"go\r");
    }

    #[test]
    fn split_escape_sequences_are_joined() {
        use std::time::{Duration, Instant};
        let k = |c| KeyEvent::new(c, Modifiers::NONE);
        let t0 = Instant::now();
        let mut j = EscJoin::default();
        // ESC | [ | A across three reads: one Up key
        assert!(j.push(k(KeyCode::Escape), t0).is_empty());
        assert_eq!(j.deadline(), Some(t0 + EscJoin::ESC_TIMEOUT));
        assert!(j.push(k(KeyCode::Char('[')), t0 + Duration::from_millis(2)).is_empty());
        assert_eq!(j.push(k(KeyCode::Char('A')), t0 + Duration::from_millis(3)), vec![k(KeyCode::Up)]);
        assert_eq!(j.deadline(), None);
        // modified arrows: ESC [ 1 ; 5 C
        j.push(k(KeyCode::Escape), t0);
        let mut out = Vec::new();
        for c in "[1;5C".chars() {
            out.extend(j.push(k(KeyCode::Char(c)), t0 + Duration::from_millis(1)));
        }
        assert_eq!(out, vec![KeyEvent::new(KeyCode::Right, Modifiers::CONTROL)]);
        // ESC then a letter within the window: Alt+letter (legacy encoding)
        j.push(k(KeyCode::Escape), t0);
        assert_eq!(j.push(k(KeyCode::Char('b')), t0), vec![KeyEvent::new(KeyCode::Char('b'), Modifiers::ALT)]);
        // a lone Esc settles at its deadline
        j.push(k(KeyCode::Escape), t0);
        assert_eq!(j.flush(), vec![k(KeyCode::Escape)]);
        // Esc followed by a letter typed later: two keys
        j.push(k(KeyCode::Escape), t0);
        assert_eq!(
            j.push(k(KeyCode::Char('x')), t0 + Duration::from_millis(80)),
            vec![k(KeyCode::Escape), k(KeyCode::Char('x'))]
        );
        // Esc followed by another key kind: Esc first, then that key
        j.push(k(KeyCode::Escape), t0);
        assert_eq!(j.push(k(KeyCode::Enter), t0), vec![k(KeyCode::Escape), k(KeyCode::Enter)]);
        // Esc Esc: the first settles, the second waits
        j.push(k(KeyCode::Escape), t0);
        assert_eq!(j.push(k(KeyCode::Escape), t0), vec![k(KeyCode::Escape)]);
        assert_eq!(j.flush(), vec![k(KeyCode::Escape)]);
        // other keys pass straight through
        assert_eq!(j.push(k(KeyCode::Char('q')), t0), vec![k(KeyCode::Char('q'))]);
    }

    #[test]
    fn osc11_luminance() {
        assert_eq!(parse_osc11("\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\\x1b[?62;c"), Some([28, 28, 28]));
        assert!(is_dark([28, 28, 28]));
        assert_eq!(parse_osc11("\x1b]11;rgb:ffff/ffff/f0f0\x07"), Some([255, 255, 240]));
        assert!(!is_dark([255, 255, 240]));
        assert_eq!(parse_osc11("\x1b]11;rgb:fa/fa/fa\x07"), Some([250, 250, 250]));
        assert_eq!(parse_osc11("\x1b[?1;2c"), None, "DA1 only: terminal has no OSC 11");
    }

    #[test]
    fn decodes_csi_u_fkeys_and_discards_unrecognized_csi() {
        // SGR mouse report must be dropped, not injected as text.
        assert!(decode_keys(b"\x1b[<0;12;3M").is_empty());
        // Kitty CSI-u with Cyrillic 'с' (1089) + base_layout_key 'c' (99) + Ctrl (5).
        let ks = decode_keys(b"\x1b[1089::99;5u");
        assert_eq!(ks.len(), 1);
        assert_eq!(ks[0].code, KeyCode::Char('с'));
        assert_eq!(ks[0].modifiers, Modifiers::CONTROL);
        assert_eq!(ks[0].base_layout_key, Some('c'));
        assert!(crate::keys::is_ctrl_c(&ks[0]));

        // F1-F12 across SS3 and CSI ~ forms.
        let fkeys: Vec<KeyCode> = decode_keys(b"\x1bOP\x1b[15~\x1b[24;5~").into_iter().map(|e| e.code).collect();
        assert_eq!(fkeys, vec![KeyCode::Function(1), KeyCode::Function(5), KeyCode::Function(12)]);
    }

    #[test]
    fn lone_esc_respects_esc_timeout_before_seq_timeout() {
        use std::time::{Duration, Instant};
        let k = |c| KeyEvent::new(c, Modifiers::NONE);
        let t0 = Instant::now();
        let mut j = EscJoin::default();
        assert!(j.push(k(KeyCode::Escape), t0).is_empty());
        // 20 ms > ESC_TIMEOUT (10 ms) but < SEQ_TIMEOUT (50 ms): settles as Esc + 'x', not Alt+x.
        assert_eq!(
            j.push(k(KeyCode::Char('x')), t0 + Duration::from_millis(20)),
            vec![k(KeyCode::Escape), k(KeyCode::Char('x'))]
        );
    }

    #[test]
    fn esc_join_reassembles_split_osc_dcs_apc_and_cpr_replies_without_leaking_keys() {
        use std::time::{Duration, Instant};
        let k = |c| KeyEvent::new(c, Modifiers::NONE);
        let t0 = Instant::now();
        let mut j = EscJoin::default();
        let run_seq = |j: &mut EscJoin, seq: Vec<(KeyEvent, u64)>| -> (Vec<KeyEvent>, Vec<Vec<u8>>) {
            let mut keys = Vec::new();
            let mut replies = Vec::new();
            for (ev, dt_ms) in seq {
                let out = j.push_with_replies(ev, t0 + Duration::from_millis(dt_ms));
                keys.extend(out.keys);
                replies.extend(out.reply_bytes);
            }
            (keys, replies)
        };

        // 1) Split OSC 11 ending with BEL (termina decodes BEL as Ctrl+G): must become a reply,
        //    never Alt+], never typed text, and never Ctrl+G.
        let mut seq = vec![(k(KeyCode::Escape), 0)];
        for (idx, c) in "]11;rgb:1c1c/1c1c/1c1c".chars().enumerate() {
            seq.push((k(KeyCode::Char(c)), (idx as u64) + 1));
        }
        seq.push((KeyEvent::new(KeyCode::Char('g'), Modifiers::CONTROL), 25));
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "split OSC 11 leaked keys: {keys:?}");
        assert_eq!(replies, vec![b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07".to_vec()]);
        assert_eq!(parse_osc11(&String::from_utf8_lossy(&replies[0])), Some([28, 28, 28]));

        // 2) Split OSC 11 ending with ST (`ESC` + `\`) and DCS ending with `Alt+\`.
        let mut seq = vec![(k(KeyCode::Escape), 100)];
        for c in "]11;rgb:ffff/ffff/ffff".chars() {
            seq.push((k(KeyCode::Char(c)), 101));
        }
        seq.push((k(KeyCode::Escape), 102));
        seq.push((k(KeyCode::Char('\\')), 103));
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "split OSC 11 (ST) leaked keys: {keys:?}");
        assert_eq!(replies, vec![b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".to_vec()]);

        let mut seq = vec![(k(KeyCode::Escape), 200)];
        for c in "P>|foot(1.16.2)".chars() {
            seq.push((k(KeyCode::Char(c)), 201));
        }
        seq.push((KeyEvent::new(KeyCode::Char('\\'), Modifiers::ALT), 202));
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "split DCS leaked keys: {keys:?}");
        assert_eq!(replies, vec![b"\x1bP>|foot(1.16.2)\x1b\\".to_vec()]);
        assert_eq!(parse_probe(&replies[0]).xtversion.as_deref(), Some("foot(1.16.2)"));

        // 3) Split APC (e.g. Kitty graphics reply).
        let mut seq = vec![(k(KeyCode::Escape), 300)];
        for c in "_Gi=1;OK".chars() {
            seq.push((k(KeyCode::Char(c)), 301));
        }
        seq.push((k(KeyCode::Escape), 302));
        seq.push((k(KeyCode::Char('\\')), 303));
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "split APC leaked keys: {keys:?}");
        assert_eq!(replies, vec![b"\x1b_Gi=1;OK\x1b\\".to_vec()]);

        // 4) Split CPR on row 1 (`ESC [ 1 ; 5 R`): must route to replies, NEVER decode as F3.
        let mut seq = vec![(k(KeyCode::Escape), 400)];
        for c in "[1;5R".chars() {
            seq.push((k(KeyCode::Char(c)), 401));
        }
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "split CPR row 1 must not decode as F3: {keys:?}");
        assert_eq!(replies, vec![b"\x1b[1;5R".to_vec()]);
        assert_eq!(parse_probe(&replies[0]).cursor_row, Some(1));

        // 5) Overlong unterminated OSC reply is bounded and dropped without leaking into editor.
        let mut seq = vec![(k(KeyCode::Escape), 500), (k(KeyCode::Char(']')), 501)];
        for _ in 0..(EscJoin::MAX_SEQ_LEN + 10) {
            seq.push((k(KeyCode::Char('a')), 502));
        }
        let (keys, replies) = run_seq(&mut j, seq);
        assert!(keys.is_empty(), "overlong OSC reply leaked keys: {keys:?}");
        assert!(replies.is_empty());

        // 6) Genuine lone `ESC` followed immediately by `]` with no reply body still settles as Alt+].
        let mut j2 = EscJoin::default();
        assert!(j2.push_with_replies(k(KeyCode::Escape), t0).keys.is_empty());
        assert!(j2.push_with_replies(k(KeyCode::Char(']')), t0 + Duration::from_millis(1)).keys.is_empty());
        assert_eq!(j2.flush(), vec![KeyEvent::new(KeyCode::Char(']'), Modifiers::ALT)]);
    }
}
