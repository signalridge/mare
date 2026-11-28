//! Key event <-> canonical string conversion ("ctrl+t", "shift+enter", ...).

use crate::tty::{KeyCode, KeyEvent, Modifiers};

/// Return the physical ASCII base-layout character when a non-ASCII character key is pressed
/// with a shortcut modifier (Ctrl, Alt, or Super). ASCII keys on layouts like Dvorak or Colemak
/// keep their primary character.
pub fn base_layout_fallback(ev: &KeyEvent) -> Option<char> {
    if !ev.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER) {
        return None;
    }
    let base = ev.base_layout_key.filter(|b| b.is_ascii_graphic())?;
    match ev.code {
        KeyCode::Char(c) if !c.is_ascii_graphic() => Some(base),
        _ => None,
    }
}

/// The reserved interrupt key, independent of the configured keymap.
pub fn is_ctrl_c(ev: &KeyEvent) -> bool {
    let mut mods = ev.modifiers;
    if let KeyCode::Char(c) = ev.code
        && c.is_uppercase()
    {
        mods |= Modifiers::SHIFT;
    }
    if mods != Modifiers::CONTROL {
        return false;
    }
    ev.code == KeyCode::Char('c') || base_layout_fallback(ev).is_some_and(|b| b.eq_ignore_ascii_case(&'c'))
}

/// Printable character without Control or Alt modifiers (`Shift` and `Super` are preserved).
pub fn printable_char(ev: &KeyEvent) -> Option<char> {
    match ev.code {
        KeyCode::Char(c) if !ev.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT) => Some(c),
        _ => None,
    }
}

/// Fold fullwidth ASCII characters (`！`..=`～`) to ASCII so single-key modal shortcuts work
/// when an IME is left in fullwidth alphanumeric mode.
pub fn fold_fullwidth_ascii(c: char) -> char {
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

/// Fold any fullwidth ASCII `KeyCode::Char` on `key` to its ASCII equivalent.
pub fn fold_fullwidth_key(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = key.code {
        key.code = KeyCode::Char(fold_fullwidth_ascii(c));
    }
    key
}

/// Event used when the reserved key is invoked from the key menu.
pub fn ctrl_c_event() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL)
}

fn code_name(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Escape => "esc".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Function(n) => format!("f{n}"),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_lowercase().to_string(),
        _ => return None,
    })
}

fn key_to_string_with_code(ev: &KeyEvent, code: KeyCode) -> String {
    let mut mods = ev.modifiers;
    if code == KeyCode::BackTab {
        mods |= Modifiers::SHIFT;
    }
    // Uppercase chars imply shift; normalize "A" -> "shift+a".
    if let KeyCode::Char(c) = ev.code
        && c.is_uppercase()
    {
        mods |= Modifiers::SHIFT;
    }
    let mut out = String::new();
    if mods.contains(Modifiers::CONTROL) {
        out.push_str("ctrl+");
    }
    if mods.contains(Modifiers::ALT) {
        out.push_str("alt+");
    }
    if mods.contains(Modifiers::SHIFT) {
        // shift on a non-letter printable char is already reflected in the char itself
        let printable_symbol = matches!(code, KeyCode::Char(c) if !c.is_alphabetic() && c != ' ');
        if !printable_symbol {
            out.push_str("shift+");
        }
    }
    if mods.contains(Modifiers::SUPER) {
        out.push_str("super+");
    }
    out.push_str(&code_name(code).unwrap_or_else(|| "unknown".into()));
    out
}

/// Canonical string for a key event: modifiers in order ctrl, alt, shift, super, then key.
/// Falls back to `base_layout_key` for non-ASCII shortcut combinations (e.g. Cyrillic Ctrl+с).
pub fn key_to_string(ev: &KeyEvent) -> String {
    let code = base_layout_fallback(ev).map(KeyCode::Char).unwrap_or(ev.code);
    key_to_string_with_code(ev, code)
}

/// Parse a binding string into key code + modifiers. Case-insensitive.
/// Accepts `ctrl+x`, `c-x`, `C-x`, `M-x` (alt), `S-x`, aliases (escape, return, cr, del, ...).
pub fn parse_key(s: &str) -> Option<(KeyCode, Modifiers)> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let mut mods = Modifiers::NONE;
    // Split on '+' or '-' but keep a trailing literal '+'/'-' key.
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if (c == '+' || c == '-') && !cur.is_empty() && i + 1 < chars.len() {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    let key = parts.pop()?;
    for m in parts {
        match m.as_str() {
            "ctrl" | "control" | "c" | "ctl" => mods |= Modifiers::CONTROL,
            "alt" | "meta" | "m" | "opt" | "option" | "a" => mods |= Modifiers::ALT,
            "shift" | "s" => mods |= Modifiers::SHIFT,
            "super" | "cmd" | "command" | "win" | "d" => mods |= Modifiers::SUPER,
            _ => return None,
        }
    }
    let code = match key.as_str() {
        "esc" | "escape" => KeyCode::Escape,
        "enter" | "return" | "cr" | "ret" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => {
            mods |= Modifiers::SHIFT;
            KeyCode::Tab
        }
        "backspace" | "bs" | "bksp" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "space" | "spc" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "gt" => KeyCode::Char('>'),
        k if k.len() > 1 && k.starts_with('f') && k[1..].parse::<u8>().is_ok() => {
            KeyCode::Function(k[1..].parse().ok()?)
        }
        k => {
            let mut it = k.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };
    Some((code, mods))
}

/// Does `ev` match the binding string?
pub fn matches(ev: &KeyEvent, binding: &str) -> bool {
    let Some((code, mods)) = parse_key(binding) else {
        return false;
    };
    let matches_str = |s: &str| parse_key(s).is_some_and(|(ecode, emods)| code == ecode && mods == emods);
    matches_str(&key_to_string_with_code(ev, ev.code))
        || base_layout_fallback(ev).is_some_and(|b| matches_str(&key_to_string_with_code(ev, KeyCode::Char(b))))
}

/// "<C-t>" / "C-t" / "Ctrl+T" → canonical "ctrl+t".
pub fn normalize_key(k: &str) -> String {
    let t = k.trim();
    let inner = if t.len() >= 2 && t.starts_with('<') && t.ends_with('>') { &t[1..t.len() - 1] } else { t };
    match parse_key(inner) {
        Some((code, mods)) => key_to_string(&KeyEvent::new(code, mods)),
        None => inner.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: Modifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn to_string() {
        assert_eq!(key_to_string(&k(KeyCode::Char('t'), Modifiers::CONTROL)), "ctrl+t");
        assert_eq!(key_to_string(&k(KeyCode::Enter, Modifiers::SHIFT)), "shift+enter");
        assert_eq!(key_to_string(&k(KeyCode::BackTab, Modifiers::SHIFT)), "shift+tab");
        assert_eq!(key_to_string(&k(KeyCode::Char('P'), Modifiers::CONTROL | Modifiers::SHIFT)), "ctrl+shift+p");
        assert_eq!(key_to_string(&k(KeyCode::Char(' '), Modifiers::NONE)), "space");
        assert_eq!(key_to_string(&k(KeyCode::Function(1), Modifiers::NONE)), "f1");
        assert_eq!(key_to_string(&k(KeyCode::Escape, Modifiers::NONE)), "esc");
    }

    #[test]
    fn parse_roundtrip() {
        assert_eq!(parse_key("Ctrl+T"), Some((KeyCode::Char('t'), Modifiers::CONTROL)));
        assert_eq!(parse_key("C-x"), Some((KeyCode::Char('x'), Modifiers::CONTROL)));
        assert_eq!(parse_key("M-b"), Some((KeyCode::Char('b'), Modifiers::ALT)));
        assert_eq!(parse_key("escape"), Some((KeyCode::Escape, Modifiers::NONE)));
        assert_eq!(parse_key("return"), Some((KeyCode::Enter, Modifiers::NONE)));
        assert_eq!(parse_key("ctrl++"), Some((KeyCode::Char('+'), Modifiers::CONTROL)));
        assert_eq!(parse_key("f12"), Some((KeyCode::Function(12), Modifiers::NONE)));
        assert_eq!(parse_key("hyper+x"), None);
    }

    #[test]
    fn matching() {
        assert!(matches(&k(KeyCode::Char('t'), Modifiers::CONTROL), "ctrl+t"));
        assert!(matches(&k(KeyCode::BackTab, Modifiers::SHIFT), "shift+tab"));
        assert!(matches(&k(KeyCode::Char('P'), Modifiers::CONTROL | Modifiers::SHIFT), "ctrl+shift+p"));
        assert!(!matches(&k(KeyCode::Char('t'), Modifiers::NONE), "ctrl+t"));
        assert!(matches(&k(KeyCode::Escape, Modifiers::NONE), "Escape"));
    }

    #[test]
    fn printable_char_keeps_turkish_capital_i_and_rejects_ctrl_alt() {
        assert_eq!(printable_char(&k(KeyCode::Char('İ'), Modifiers::NONE)), Some('İ'));
        assert_eq!(printable_char(&k(KeyCode::Char('İ'), Modifiers::SHIFT)), Some('İ'));
        assert_eq!(printable_char(&k(KeyCode::Char('a'), Modifiers::SUPER)), Some('a'));
        assert_eq!(printable_char(&k(KeyCode::Char('a'), Modifiers::CONTROL)), None);
        assert_eq!(printable_char(&k(KeyCode::Char('a'), Modifiers::ALT)), None);
    }

    #[test]
    fn base_layout_fallback_handles_non_latin_and_preserves_dvorak_ascii() {
        let cyrillic_ctrl_c = KeyEvent::new(KeyCode::Char('с'), Modifiers::CONTROL).with_base_layout_key(Some('c'));
        assert!(is_ctrl_c(&cyrillic_ctrl_c));
        assert_eq!(key_to_string(&cyrillic_ctrl_c), "ctrl+c");
        assert!(matches(&cyrillic_ctrl_c, "ctrl+c"));

        // ASCII layout keys (e.g. Dvorak 'f' physically at QWERTY 'u') keep primary 'f'.
        let dvorak_ctrl_f = KeyEvent::new(KeyCode::Char('f'), Modifiers::CONTROL).with_base_layout_key(Some('u'));
        assert_eq!(key_to_string(&dvorak_ctrl_f), "ctrl+f");
        assert!(matches(&dvorak_ctrl_f, "ctrl+f"));
        assert!(!matches(&dvorak_ctrl_f, "ctrl+u"));
    }

    #[test]
    fn normalize_key_preserves_angle_brackets() {
        assert_eq!(normalize_key("<C->>"), "ctrl+>");
        assert_eq!(normalize_key("<lt>"), "<");
        assert_eq!(normalize_key("<gt>"), ">");
        assert_eq!(normalize_key("<C-lt>"), "ctrl+<");
    }

    #[test]
    fn repro_222_modified_copy_shortcuts_do_not_trigger_ctrl_c() {
        // Plain Ctrl+C (ASCII and non-Latin base-layout fallback) must still match.
        assert!(is_ctrl_c(&k(KeyCode::Char('c'), Modifiers::CONTROL)));
        assert!(is_ctrl_c(&KeyEvent::new(KeyCode::Char('с'), Modifiers::CONTROL).with_base_layout_key(Some('c'))));

        // Forwarded Shift/Super copy chords must NOT activate the reserved Ctrl+C action.
        let ctrl_shift_c = k(KeyCode::Char('c'), Modifiers::CONTROL | Modifiers::SHIFT);
        let ctrl_super_c = k(KeyCode::Char('c'), Modifiers::CONTROL | Modifiers::SUPER);
        let cyrillic_ctrl_shift_c =
            KeyEvent::new(KeyCode::Char('С'), Modifiers::CONTROL | Modifiers::SHIFT).with_base_layout_key(Some('c'));

        assert!(!is_ctrl_c(&ctrl_shift_c), "Ctrl+Shift+C must not trigger reserved Ctrl+C");
        assert!(!is_ctrl_c(&ctrl_super_c), "Ctrl+Super+C must not trigger reserved Ctrl+C");
        assert!(!is_ctrl_c(&cyrillic_ctrl_shift_c), "Cyrillic Ctrl+Shift+C must not trigger reserved Ctrl+C");

        // Also check decoded Kitty CSI-u sequence for Ctrl+Shift+c (`99;6u`) and Ctrl+Super+c (`99;13u`).
        #[cfg(unix)]
        {
            let mut dec = crate::live_input::LiveDecoder::default();
            let mut out = Vec::new();
            dec.feed(b"\x1b[99;6u\x1b[99;13u\x1b[1089::99;6u", false, &mut out);
            for ev in out {
                if let crate::tty::Event::Key(ke) = ev {
                    assert!(!is_ctrl_c(&ke), "decoded modified chord {ke:?} must not trigger reserved Ctrl+C");
                }
            }
        }
    }
}
