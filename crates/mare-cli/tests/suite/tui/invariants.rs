//! Global screen invariants checked automatically at the end of every TUI `Term` test
//! (and explicitly inside the command × session-state matrix).
//!
//! Reference patterns:
//! - Codex `codex-rs/tui/src/test_backend.rs` (`VT100Backend`) and
//!   `codex-rs/tui/tests/suite/vt100_history.rs` (virtual-terminal cell/wrap assertions)
//! - pi `packages/tui/test/virtual-terminal.ts` (headless terminal screen capture)

use super::*;

#[derive(Clone, Debug)]
pub(super) struct CellInfo {
    pub(super) text: String,
    pub(super) wide_cont: bool,
}

#[derive(Clone, Debug)]
pub(in crate::suite) struct ScreenObservation {
    pub(super) screen: String,
    pub(super) history: String,
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) wrapped_rows: Vec<u16>,
    pub(super) grid: Vec<Vec<CellInfo>>,
    pub(super) saw_welcome_banner: bool,
    pub(super) clear_count: usize,
    pub(super) in_alt_screen: bool,
}

pub(in crate::suite) fn has_welcome_banner(text: &str) -> bool {
    text.contains("/model · /help · @ file · ! bash")
        || text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("mare v") && (t.contains(" · ") || t.contains("no model"))
        })
}

fn extract_grid_and_wraps(vt_screen: &vt100::Screen) -> (Vec<Vec<CellInfo>>, Vec<u16>) {
    let (rows, cols) = (vt_screen.size().0.get(), vt_screen.size().1.get());
    let mut wrapped_rows = Vec::new();
    let mut grid = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        if vt_screen.row_wrapped(r) {
            wrapped_rows.push(r);
        }
        let mut row = Vec::with_capacity(cols as usize);
        for c in 0..cols {
            let (text, wide_cont) = vt_screen
                .cell(r, c)
                .map(|cell| (cell.contents().to_string(), cell.is_wide_continuation()))
                .unwrap_or_default();
            row.push(CellInfo { text, wide_cont });
        }
        grid.push(row);
    }
    (grid, wrapped_rows)
}

pub(in crate::suite) fn observe_term(t: &mut Term, saw_welcome_banner: bool) -> ScreenObservation {
    let history = t.history();
    let vt_screen = t.parser.screen();
    let (rows, cols) = (vt_screen.size().0.get(), vt_screen.size().1.get());
    let screen = vt_screen.contents();
    let in_alt_screen = vt_screen.alternate_screen() || t.vt.transcript_open();
    let (grid, wrapped_rows) = extract_grid_and_wraps(vt_screen);
    ScreenObservation {
        screen,
        history,
        cols,
        rows,
        wrapped_rows,
        grid,
        saw_welcome_banner,
        clear_count: t.vt.clear_count(),
        in_alt_screen,
    }
}

pub(in crate::suite) fn check_global_invariants(obs: &ScreenObservation) -> Result<(), String> {
    let mut violations = Vec::new();
    if let Err(e) = check_no_duplicate_answer_blocks(&obs.history) {
        violations.push(e);
    }
    if let Err(e) = check_no_raw_error_codes(&obs.history) {
        violations.push(e);
    }
    if let Err(e) = check_welcome_banner_after_clear(obs) {
        violations.push(e);
    }
    if let Err(e) = check_boxes_and_truncation(obs) {
        violations.push(e);
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Global screen invariant violation(s):\n- {}\n--- screen ({}x{}) ---\n{}",
            violations.join("\n- "),
            obs.cols,
            obs.rows,
            obs.screen
        ))
    }
}

fn is_chrome_or_boundary_line(trimmed: &str) -> bool {
    trimmed.is_empty()
        || trimmed.starts_with('╭')
        || trimmed.starts_with('╰')
        || trimmed.starts_with('┌')
        || trimmed.starts_with('└')
        || trimmed.starts_with('│')
        || trimmed.starts_with("─")
        || trimmed.starts_with("━")
        || trimmed.starts_with("▄")
        || trimmed.starts_with("▀")
        || trimmed.starts_with("⎯")
        || trimmed.starts_with("↳ ")
        || trimmed.starts_with("SIDE · ")
        || trimmed.starts_with("Parent: ")
        || trimmed.starts_with("Transcript")
}

fn is_turn_separator_line(trimmed: &str) -> bool {
    trimmed.starts_with("❯ ")
        || trimmed.starts_with("! ")
        || trimmed.starts_with("You: ")
        || trimmed.starts_with("worked ")
        || trimmed.starts_with("interrupted after ")
        || trimmed.starts_with("⎯ ")
}

fn is_non_answer_notice_line(trimmed: &str) -> bool {
    trimmed.starts_with("● not sent:")
}

/// Invariant 1: no block of answer text appears twice in a row.
pub(super) fn check_no_duplicate_answer_blocks(text: &str) -> Result<(), String> {
    let mut prev: Option<String> = None;
    let mut current: Vec<&str> = Vec::new();
    let flush = |current: &mut Vec<&str>, prev: &mut Option<String>| -> Result<(), String> {
        if current.is_empty() {
            return Ok(());
        }
        let norm = current.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        current.clear();
        if norm.chars().count() < 8 {
            *prev = None;
            return Ok(());
        }
        if prev.as_deref() == Some(norm.as_str()) {
            return Err(format!("duplicate consecutive answer block: {norm:?}"));
        }
        *prev = Some(norm);
        Ok(())
    };

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            flush(&mut current, &mut prev)?;
            continue;
        }
        if is_turn_separator_line(trimmed) {
            flush(&mut current, &mut prev)?;
            prev = None;
            continue;
        }
        if is_chrome_or_boundary_line(trimmed) || is_non_answer_notice_line(trimmed) {
            flush(&mut current, &mut prev)?;
            continue;
        }
        // Skip status bar lines at the bottom of the screen.
        if trimmed.contains("Shift+Tab") || trimmed.contains("Faux Echo") {
            flush(&mut current, &mut prev)?;
            continue;
        }
        current.push(trimmed);
    }
    flush(&mut current, &mut prev)?;
    Ok(())
}

const ERROR_NAMESPACES: &[&str] = &[
    "session",
    "agent",
    "subagent",
    "compact",
    "commands",
    "terminal",
    "proc",
    "query",
    "tools",
    "llm",
    "mcp",
    "permissions",
    "approvals",
    "conversations",
    "context",
    "changes",
    "plugins",
    "ui",
    "store",
    "kernel",
    "rpc",
    "fs",
    "auth",
    "provider",
    "err_probe",
];

const ERROR_SUFFIXES: &[&str] = &[
    "not_found",
    "no_history",
    "already_exists",
    "too_large",
    "io_failed",
    "no_pending_work",
    "not_compacted",
    "not_running",
    "command_unavailable",
    "unknown_type",
    "unknown_subagent",
    "unknown_process",
    "unknown_terminal",
    "steer_rejected",
    "stale_cursor",
    "provider_changed",
    "stop_unconfirmed",
    "terminal_limit_reached",
    "duplicate_name",
    "rejected",
    "locked",
    "failed",
    "unavailable",
];

fn looks_like_raw_error_code(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
    let Some((ns, code)) = trimmed.split_once('.') else {
        return false;
    };
    if ns.is_empty() || code.is_empty() || code.contains('.') {
        return false;
    }
    if !ns.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    if !code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    if ERROR_SUFFIXES.contains(&code) {
        return true;
    }
    ERROR_NAMESPACES.contains(&ns) && code.contains('_')
}

/// Invariant 2: no raw error codes (`session.no_history`, `*.not_found`, …) are visible.
pub(super) fn check_no_raw_error_codes(text: &str) -> Result<(), String> {
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("❯ ") || trimmed.starts_with("│ ❯ ") {
            continue;
        }
        for tok in trimmed.split_whitespace() {
            if looks_like_raw_error_code(tok) {
                return Err(format!("raw error code visible on screen: {tok:?} in line {trimmed:?}"));
            }
        }
    }
    Ok(())
}

pub(super) fn has_session_replay_header(text: &str) -> bool {
    text.contains("⎯ Resumed") || text.contains("⎯ Moved to branch ⎯")
}

/// Invariant 3: after a clear on a session that had the welcome banner, the welcome banner is redrawn
/// (unless the clear was for resuming/switching to an existing session/branch replay).
pub(super) fn check_welcome_banner_after_clear(obs: &ScreenObservation) -> Result<(), String> {
    if obs.saw_welcome_banner
        && obs.clear_count > 0
        && !obs.in_alt_screen
        && !has_welcome_banner(&obs.history)
        && !has_session_replay_header(&obs.history)
    {
        return Err("welcome banner was cleared and not redrawn".to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct ScreenBox {
    top: usize,
    bottom: usize,
    left: usize,
    right: usize,
}

impl ScreenBox {
    fn contains(&self, r: usize, c: usize) -> bool {
        r >= self.top && r <= self.bottom && c >= self.left && c <= self.right
    }
}

fn cell_str(row: &[CellInfo], c: usize) -> &str {
    row.get(c).map(|cell| cell.text.as_str()).unwrap_or("")
}

fn is_blank_cell(cell: &CellInfo) -> bool {
    cell.wide_cont || cell.text.is_empty() || cell.text == " "
}

fn is_vertical_border(s: &str) -> bool {
    matches!(s, "│" | "┃")
}

fn find_matching_bottom(
    grid: &[Vec<CellInfo>],
    r_top: usize,
    c: usize,
    tl: &str,
    bl: &str,
    br: Option<usize>,
    br_ch: &str,
) -> Option<usize> {
    for (r_bot, row) in grid.iter().enumerate().skip(r_top + 1) {
        let cell = cell_str(row, c);
        if cell == bl && br.is_none_or(|xr| cell_str(row, xr) == br_ch) {
            return Some(r_bot);
        }
        // Stop if another top corner starts on the same left column or the row is completely blank.
        if cell == tl || row.iter().all(is_blank_cell) {
            return None;
        }
    }
    None
}

fn detect_boxes(grid: &[Vec<CellInfo>]) -> Vec<ScreenBox> {
    let rows = grid.len();
    let mut boxes = Vec::new();
    let mut matched_tops = Vec::new();
    for r_top in 0..rows {
        let row = &grid[r_top];
        let cols = row.len();
        let mut c = 0;
        while c < cols {
            let ch = cell_str(row, c);
            let (tr, bl, br) = match ch {
                "╭" => ("╮", "╰", "╯"),
                "┌" => ("┐", "└", "┘"),
                _ => {
                    c += 1;
                    continue;
                }
            };
            let Some(x_right) = ((c + 1)..cols).find(|&x| cell_str(row, x) == tr) else {
                c += 1;
                continue;
            };
            if let Some(r_bot) = find_matching_bottom(grid, r_top, c, ch, bl, Some(x_right), br) {
                boxes.push(ScreenBox { top: r_top, bottom: r_bot, left: c, right: x_right });
                matched_tops.push((r_top, c));
                c = x_right + 1;
            } else {
                c += 1;
            }
        }
    }
    // Second pass: recover boxes whose top-right or bottom-right corner (or bottom-left corner) was
    // overlaid by a higher-z-order box (e.g., a toast drawn over an editor or a modal over a toast).
    for r_top in 0..rows {
        let row = &grid[r_top];
        let cols = row.len();
        for c in 0..cols {
            let ch = cell_str(row, c);
            let (tr, bl, br) = match ch {
                "╭" => ("╮", "╰", "╯"),
                "┌" => ("┐", "└", "┘"),
                _ => continue,
            };
            if matched_tops.contains(&(r_top, c)) {
                continue;
            }
            if let Some(r_bot) = find_matching_bottom(grid, r_top, c, ch, bl, None, br) {
                let x_right = ((c + 1)..cols)
                    .rev()
                    .find(|&x| cell_str(row, x) == tr || cell_str(&grid[r_bot], x) == br)
                    .or_else(|| {
                        boxes
                            .iter()
                            .any(|b| b.top <= r_bot && b.bottom >= r_top && b.right + 1 >= cols)
                            .then_some(cols - 1)
                    });
                if let Some(xr) = x_right {
                    boxes.push(ScreenBox { top: r_top, bottom: r_bot, left: c, right: xr });
                }
            } else if let Some(xr) = ((c + 1)..cols).rev().find(|&x| cell_str(row, x) == tr) {
                // Check if `(r_top, xr)` connects down to `(r_bot, xr) == br` while `(r_bot, c)` is
                // overlaid by an already-detected box.
                let r_bot = ((r_top + 1)..rows).find(|&rb| {
                    if cell_str(&grid[rb], xr) == br {
                        return boxes.iter().any(|b| b.contains(rb, c));
                    }
                    false
                });
                if let Some(rb) = r_bot
                    && !((r_top + 1)..rb).any(|r| grid[r].iter().all(is_blank_cell) || cell_str(&grid[r], c) == ch)
                {
                    boxes.push(ScreenBox { top: r_top, bottom: rb, left: c, right: xr });
                }
            }
        }
    }
    // Drop stale scrollback fragments where interior rows lack a vertical border or overlapping box.
    let snapshot = boxes.clone();
    boxes.retain(|b| {
        ((b.top + 1)..b.bottom).all(|r| {
            let row = &grid[r];
            let covered_left = snapshot.iter().any(|o| (o.top, o.left) != (b.top, b.left) && o.contains(r, b.left));
            let covered_right = snapshot.iter().any(|o| (o.top, o.left) != (b.top, b.left) && o.contains(r, b.right));
            (covered_left || is_vertical_border(cell_str(row, b.left)))
                && (covered_right || is_vertical_border(cell_str(row, b.right)))
        })
    });
    boxes
}

fn is_tool_tree_gutter(grid: &[Vec<CellInfo>], r: usize, c: usize) -> bool {
    let row = &grid[r];
    if c > 6 || cell_str(row, c) != "│" || c + 1 >= row.len() || cell_str(row, c + 1) != " " {
        return false;
    }
    if !row[..c].iter().all(is_blank_cell) {
        return false;
    }
    for up in (0..r).rev() {
        let prev = &grid[up];
        let first_non_blank = prev.iter().position(|cell| !is_blank_cell(cell));
        let Some(first) = first_non_blank else {
            return false;
        };
        let first_ch = prev[first].text.as_str();
        if first < c && first_ch == "●" {
            return true;
        }
        if first == c && first_ch == "│" {
            continue;
        }
        return false;
    }
    false
}

fn is_occluded_outer_border(grid: &[Vec<CellInfo>], boxes: &[ScreenBox], r: usize, c: usize) -> bool {
    if !boxes.iter().any(|b| r >= b.top && r <= b.bottom) {
        return false;
    }
    let row = &grid[r];
    let other_cells_clean =
        (0..row.len()).all(|col| col == c || is_blank_cell(&row[col]) || boxes.iter().any(|b| b.contains(r, col)));
    if !other_cells_clean {
        return false;
    }
    for up in (0..r).rev() {
        let ch = cell_str(&grid[up], c);
        if matches!(ch, "╮" | "┐") {
            let tl = if ch == "╮" { "╭" } else { "┌" };
            return (0..c).any(|left| cell_str(&grid[up], left) == tl);
        }
        if !is_vertical_border(ch) {
            return false;
        }
    }
    false
}

/// Invariant 4: no line is truncated or wrapped past the terminal/box border.
pub(super) fn check_boxes_and_truncation(obs: &ScreenObservation) -> Result<(), String> {
    if obs.cols < 24 {
        return Ok(());
    }
    if !obs.wrapped_rows.is_empty() {
        return Err(format!("line wrapped past terminal width ({} cols) at row(s) {:?}", obs.cols, obs.wrapped_rows));
    }

    let boxes = detect_boxes(&obs.grid);
    let cols = obs.cols as usize;
    let status_row = obs.grid.iter().rposition(|row| row.iter().any(|cell| !is_blank_cell(cell)));

    // 1. Every box's interior rows must have intact left/right borders and nothing overflowing right.
    for (idx, b) in boxes.iter().enumerate() {
        for r in (b.top + 1)..b.bottom {
            let row = &obs.grid[r];
            let covered_left = boxes.iter().enumerate().any(|(j, o)| j != idx && o.contains(r, b.left));
            let covered_right = boxes.iter().enumerate().any(|(j, o)| j != idx && o.contains(r, b.right));
            if !covered_left && !is_vertical_border(cell_str(row, b.left)) {
                return Err(format!(
                    "box left border broken at row {r}, col {}: got {:?} in line {:?}",
                    b.left,
                    cell_str(row, b.left),
                    obs.screen.lines().nth(r).unwrap_or_default()
                ));
            }
            if !covered_right && !is_vertical_border(cell_str(row, b.right)) {
                return Err(format!(
                    "box right border broken at row {r}, col {}: got {:?} in line {:?}",
                    b.right,
                    cell_str(row, b.right),
                    obs.screen.lines().nth(r).unwrap_or_default()
                ));
            }
            for (c, cell) in row.iter().enumerate().take(cols).skip(b.right + 1) {
                let in_other_box = boxes.iter().enumerate().any(|(j, o)| j != idx && o.contains(r, c));
                if !in_other_box && !is_blank_cell(cell) {
                    return Err(format!(
                        "text ran past box right border at row {r}, col {c} (box right={}): {:?}",
                        b.right,
                        obs.screen.lines().nth(r).unwrap_or_default()
                    ));
                }
            }
        }
    }

    // 2. Check for orphan/broken box rows (e.g. unclosed `│` from a wrapped toast).
    for (r, row) in obs.grid.iter().enumerate() {
        // Status bar (the last non-empty row) uses `│` as a key-hint separator.
        if status_row == Some(r) {
            continue;
        }
        // A 1-row degraded borderless-top/bottom editor (`editor.max_height < 3`) or a vertically
        // clipped inner frame has matching `│` on left and right with nothing past the right `│`.
        let first_v = row.iter().position(|cell| is_vertical_border(&cell.text));
        let last_v = row.iter().rposition(|cell| is_vertical_border(&cell.text));
        if let (Some(lv), Some(rv)) = (first_v, last_v)
            && lv < rv
            && row[(rv + 1)..].iter().all(is_blank_cell)
        {
            continue;
        }
        for (c, cell) in row.iter().enumerate() {
            if is_vertical_border(&cell.text) {
                let on_box_border = boxes.iter().any(|b| r > b.top && r < b.bottom && (c == b.left || c == b.right));
                let inside_box = boxes.iter().any(|b| b.contains(r, c));
                if !on_box_border
                    && !inside_box
                    && !is_tool_tree_gutter(&obs.grid, r, c)
                    && !is_occluded_outer_border(&obs.grid, &boxes, r, c)
                {
                    return Err(format!(
                        "unclosed or overflowing box border '│' at row {r}, col {c}: {:?}",
                        obs.screen.lines().nth(r).unwrap_or_default()
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Check for vertically duplicated input prompt boxes (used by matrix `Ctrl+T` check for #13).
pub(in crate::suite) fn check_no_duplicated_prompt_box(obs: &ScreenObservation) -> Result<(), String> {
    if !obs.in_alt_screen {
        let prompt_rows = obs
            .screen
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("│ ❯ ") && t.trim_end().ends_with('│')
            })
            .count();
        if prompt_rows > 1 {
            return Err(format!("input prompt box duplicated vertically ({prompt_rows} prompt rows on screen)"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_from_text(screen: &str, cols: u16, rows: u16) -> ScreenObservation {
        let mut parser = vt100::Parser::new(rows.max(1).try_into().unwrap(), cols.max(1).try_into().unwrap(), 100);
        parser.process(screen.replace('\n', "\r\n").as_bytes());
        let sc = parser.screen();
        let (grid, wrapped_rows) = extract_grid_and_wraps(sc);
        ScreenObservation {
            screen: sc.contents(),
            history: screen.to_string(),
            cols,
            rows,
            wrapped_rows,
            grid,
            saw_welcome_banner: false,
            clear_count: 0,
            in_alt_screen: false,
        }
    }

    #[test]
    fn catches_duplicate_consecutive_answer_block() {
        let dup = "❯ who are you\n\nI am an assistant running in mare.\n\nI am an assistant running in mare.\n";
        assert!(check_no_duplicate_answer_blocks(dup).is_err());
        let distinct_turns = "❯ first\n\nSame reply text.\n\n❯ second\n\nSame reply text.\n";
        assert!(check_no_duplicate_answer_blocks(distinct_turns).is_ok());
    }

    #[test]
    fn catches_raw_error_codes_without_flagging_filenames() {
        assert!(check_no_raw_error_codes("Warning: session.no_history").is_err());
        assert!(check_no_raw_error_codes("Warning: session.no_history.").is_err());
        assert!(check_no_raw_error_codes("Error: custom_plugin.not_found").is_err());
        assert!(check_no_raw_error_codes("Read notes.txt and init.lua cleanly").is_ok());
    }

    #[test]
    fn catches_cleared_screen_without_welcome_banner() {
        let mut obs = obs_from_text("╭────╮\n│ ❯  │\n╰────╯", 80, 10);
        obs.saw_welcome_banner = true;
        obs.clear_count = 1;
        assert!(check_welcome_banner_after_clear(&obs).is_err());
        let named = obs_from_text("⎯ Resumed: demo-session ⎯\n╭────╮\n│ ❯  │\n╰────╯", 80, 10);
        assert!(has_session_replay_header(&named.history));
    }

    #[test]
    fn catches_toast_border_overflow_and_duplicated_prompt_box() {
        let overflow = "\
╭─ Warning ─────────────────────────╮
Retrying (1) in 0s: stream ended    │
                                    │ response.completed
╰───────────────────────────────────╯
";
        let obs = obs_from_text(overflow, 80, 10);
        assert!(check_boxes_and_truncation(&obs).is_err());

        let dup_prompt = "\
╭──────────────────────────────────────╮
│ ❯ Ask mare anything…                 │
╭──────────────────────────────────────╮
│ ❯ Ask mare anything…                 │
╰──────────────────────────────────────╯
";
        let obs = obs_from_text(dup_prompt, 60, 10);
        assert!(check_no_duplicated_prompt_box(&obs).is_err());
    }
}
