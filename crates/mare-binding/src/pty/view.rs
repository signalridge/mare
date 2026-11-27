//! Bounded VT viewport and protocol metadata. No terminal controls reach Lua display text.
use std::{collections::VecDeque, num::NonZeroU16};

#[derive(Default)]
struct Events {
    generation: u64,
    exit_code: Option<i32>,
    replies: Vec<Vec<u8>>,
    history: VecDeque<String>,
    bytes: usize,
    max_bytes: usize,
    max_lines: usize,
    dropped: bool,
}
impl vt100::Callbacks for Events {
    fn on_scroll(&mut self, row: vt100::capture::RowContents<'_>, alternate: bool) {
        if alternate {
            return;
        }
        let mut formatted = String::new();
        let _ = row.write_formatted_basic(&mut formatted, &mut vt100::capture::BasicFormattedCaptureState::default());
        let text: String = vt100::capture::basic_formatted_to_plain(&formatted).collect();
        self.bytes += text.len() + 1;
        self.history.push_back(text);
        while self.bytes > self.max_bytes || self.history.len() > self.max_lines {
            if let Some(line) = self.history.pop_front() {
                self.bytes -= line.len() + 1;
                self.dropped = true;
            } else {
                break;
            }
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() == Some(&b"133".as_slice()) && params.get(1) == Some(&b"D".as_slice()) {
            self.generation = self.generation.saturating_add(1);
            self.exit_code = params.get(2).and_then(|v| std::str::from_utf8(v).ok()?.parse().ok());
        }
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Bound replies per transport chunk, including malicious query floods.
        if self.replies.len() >= 16 || i1.is_some() || i2.is_some() {
            return;
        }
        let value = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let reply = match (c, value) {
            ('n', 5) => Some(b"\x1b[0n".to_vec()),
            ('n', 6) => {
                let (r, c) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", r + 1, c + 1).into_bytes())
            }
            ('c', 0) => Some(b"\x1b[?1;2c".to_vec()),
            _ => None,
        };
        if let Some(reply) = reply {
            self.replies.push(reply);
        }
    }
}
// vt100 enables vte/std, whose OSC accumulator is otherwise unbounded. Hold complete OSC
// sequences here, with a fixed budget, before passing them to the emulator. Oversized or
// malformed strings are discarded through BEL/ST, like the shared display sanitizer.
#[derive(Default)]
struct OscBudget {
    escape: bool,
    osc: bool,
    osc_escape: bool,
    dropped: bool,
    bytes: Vec<u8>,
}
impl OscBudget {
    fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 1024);
        for &b in input {
            if self.osc {
                if b == 7 || (self.osc_escape && b == b'\\') {
                    if !self.dropped {
                        out.extend_from_slice(&self.bytes);
                        if self.osc_escape {
                            out.push(27);
                        }
                        out.push(b);
                    }
                    self.osc = false;
                    self.osc_escape = false;
                    self.dropped = false;
                    self.bytes.clear();
                } else if b == 27 {
                    self.osc_escape = true;
                } else {
                    if self.osc_escape {
                        self.dropped = true;
                        self.bytes.clear();
                        self.osc_escape = false;
                    }
                    if !self.dropped {
                        if self.bytes.len() < 1024 {
                            self.bytes.push(b);
                        } else {
                            self.dropped = true;
                            self.bytes.clear();
                        }
                    }
                }
            } else if self.escape {
                self.escape = false;
                if b == b']' {
                    self.osc = true;
                    self.bytes.extend_from_slice(b"\x1b]");
                } else if b == 27 {
                    self.escape = true;
                } else {
                    out.extend_from_slice(&[27, b]);
                }
            } else if b == 27 {
                self.escape = true;
            } else {
                out.push(b);
            }
        }
        out
    }
}
pub(super) struct View {
    parser: vt100::Parser<Events>,
    osc_budget: OscBudget,
}
impl View {
    pub fn new(rows: u16, cols: u16) -> mlua::Result<Self> {
        let rows = NonZeroU16::new(rows).ok_or_else(|| super::rt_err("PTY_SIZE: rows must be positive"))?;
        let cols = NonZeroU16::new(cols).ok_or_else(|| super::rt_err("PTY_SIZE: cols must be positive"))?;
        Ok(Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                0,
                Events { max_bytes: 4 * 1024 * 1024, max_lines: 10000, ..Events::default() },
            ),
            osc_budget: OscBudget::default(),
        })
    }
    pub fn limits(&mut self, max_bytes: usize, max_lines: usize) {
        let e = self.parser.callbacks_mut();
        e.max_bytes = max_bytes;
        e.max_lines = max_lines;
    }
    pub fn page(&self, lua: &mlua::Lua, offset: usize, count: usize, max_bytes: usize) -> mlua::Result<mlua::Table> {
        let screen = self.parser.screen().contents();
        let mut lines: Vec<&str> = if self.parser.screen().alternate_screen() {
            Vec::new()
        } else {
            self.parser.callbacks().history.iter().map(String::as_str).collect()
        };
        lines.extend(screen.lines());
        let total = lines.len();
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(count);
        let mut text = lines[start..end].join("\n");
        let mut truncated = self.parser.callbacks().dropped;
        if text.len() > max_bytes {
            let mut cut = text.len() - max_bytes;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
            truncated = true;
        }
        let out = lua.create_table()?;
        out.set("text", text.as_str())?;
        out.set("total_lines", total)?;
        out.set("line_begin", offset)?;
        out.set("line_end", offset.saturating_add(if text.is_empty() { 0 } else { text.lines().count() }))?;
        out.set("truncated", truncated)?;
        Ok(out)
    }
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(NonZeroU16::new(rows).unwrap(), NonZeroU16::new(cols).unwrap());
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.parser.process(&self.osc_budget.feed(bytes));
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }
    pub fn snapshot(&self, lua: &mlua::Lua, max_bytes: usize, include_screen: bool) -> mlua::Result<mlua::Table> {
        let out = lua.create_table()?;
        if include_screen {
            let mut screen = self.parser.screen().contents();
            let truncated = screen.len() > max_bytes;
            if truncated {
                let mut cut = screen.len() - max_bytes;
                while !screen.is_char_boundary(cut) {
                    cut += 1;
                }
                screen.drain(..cut);
            }
            out.set("screen", screen)?;
            out.set("view_truncated", truncated)?;
        }
        let screen = self.parser.screen().contents();
        let tail = screen.lines().last().unwrap_or("");
        let mut start = tail.len().saturating_sub(128);
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        out.set("tail", &tail[start..])?;
        out.set("alternate_screen", self.parser.screen().alternate_screen())?;
        out.set("prompt_generation", self.parser.callbacks().generation)?;
        out.set("command_exit_code", self.parser.callbacks().exit_code)?;
        Ok(out)
    }

    pub(super) fn prompt_generation(&self) -> u64 {
        self.parser.callbacks().generation
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_protocol_cr_alternate_and_queries() {
        let lua = mlua::Lua::new();
        let mut v = View::new(3, 20).unwrap();
        v.feed(b"old\rnew\x1b]133;");
        v.feed(b"D;7\x07");
        let s = v.snapshot(&lua, 256 * 1024, true).unwrap();
        assert_eq!(s.get::<String>("screen").unwrap(), "new");
        assert_eq!(s.get::<u64>("prompt_generation").unwrap(), 1);
        assert_eq!(s.get::<i32>("command_exit_code").unwrap(), 7);
        assert_eq!(v.feed(b"\x1b[6n"), vec![b"\x1b[1;4R".to_vec()]);
        v.feed(b"\x1b[?1049hsecret\x1b[?1049l");
        assert_eq!(v.snapshot(&lua, 256 * 1024, true).unwrap().get::<String>("screen").unwrap(), "new");
    }
    #[test]
    fn new_rejects_zero_dimensions_without_panicking() {
        assert!(View::new(0, 80).is_err());
        assert!(View::new(24, 0).is_err());
    }
    #[test]
    fn history_and_utf8_pages_are_bounded_and_queries_do_not_grow() {
        let lua = mlua::Lua::new();
        let mut v = View::new(2, 20).unwrap();
        v.limits(16, 2);
        v.feed("one\r\ntwo\r\nthree\r\nfour\r\n世界".as_bytes());
        let page = v.page(&lua, 0, 500, 4).unwrap();
        assert!(page.get::<String>("text").unwrap().len() <= 4);
        assert!(page.get::<bool>("truncated").unwrap());
        let empty = v.page(&lua, 100, 500, 100).unwrap();
        assert_eq!(empty.get::<String>("text").unwrap(), "");
        assert_eq!(empty.get::<usize>("line_begin").unwrap(), 100);
        assert_eq!(empty.get::<usize>("line_end").unwrap(), 100);
        let mut flood = b"\x1b]133;D;".to_vec();
        flood.extend(vec![b'x'; 100000]);
        for chunk in flood.chunks(17) {
            v.feed(chunk);
            assert!(v.osc_budget.bytes.len() <= 1024);
        }
        v.feed(b"\x07safe");
        assert_eq!(v.parser.callbacks().generation, 0);
        assert!(v.parser.screen().contents().ends_with("safe"));
        assert_eq!(v.feed(&b"\x1b[6n".repeat(1000)).len(), 16);
    }
}
