# Pre-Release Exploratory Run: Reference-Parity Checklist

This checklist is used by a second-model reviewer (or human reviewer) to inspect the screen captures produced by `scripts/prerelease_explore.py` against a release-candidate `mare` binary.

This is a **manual pre-release step** (not a CI gate). Every defect or UX regression discovered during review becomes a GitHub issue before the release ships, and the run summary is recorded in the release's tracking notes.

## How to Run

1. Build or download the release-candidate `mare` binary.
2. Run the exploratory driver against a low-cost real model (for example `opencode-go/glm-5.3-flash` with `OPENCODE_API_KEY` or `MARE_LIVE_OPENCODE_KEY` set in the environment):

   ```sh
   python3 scripts/prerelease_explore.py \
     --mare target/release/mare \
     --out /tmp/mare-rc-explore
   ```

   For an offline smoke check of the driver itself (against a `faux`-enabled debug binary):

   ```sh
   python3 scripts/prerelease_explore.py \
     --mare target/debug/mare \
     --faux \
     --out /tmp/mare-rc-explore-faux
   ```

3. Hand the generated `review-prompt.md` and `captures/*.txt` files to a second model together with this checklist.
4. File GitHub issues for every finding, and record the run date, candidate SHA, model, and issue links in the release's tracking notes.

---

## Global Invariants (Check on Every Capture)

Compare every capture against the baseline polish of **Claude Code**, **Codex**, and **Kimi Code**:

- [ ] **Single intact prompt box:** Only one active input box (`╭…╮` / `│ ❯ … │` / `╰…╯`) is visible; no vertically stacked ghost copies after overlays or resizes.
- [ ] **Closed borders & clamping:** Every modal, picker, and toast box has aligned left/right `│` borders with zero text overflowing past the right edge or wrapping outside the frame.
- [ ] **No raw internals:** Zero Lua `stack traceback:` frames, zero raw `namespace.error_code` strings (e.g. `session.no_history`), and zero unrendered ANSI escape fragments.
- [ ] **No duplicate answer blocks:** No assistant response appears twice consecutively in main or side conversations.
- [ ] **Non-occluding notices:** Notification toasts never cover open approval dialogs or modal pickers.

---

## Step-by-Step Reference-Parity Checks

### 1. Startup & Onboarding (`01_startup`, `02_onboarding`)
- **Claude Code / Codex / Kimi Code parity:** Clear first-run welcome showing version, working directory, active model (or clear setup guidance), and essential key hints without visual clutter.
- [ ] Version, cwd, model, and key hints (`/model`, `/help`, `@`, `!`) render cleanly inside terminal width.
- [ ] First-run onboarding dialog (when enabled) has clear navigation hints and dismisses cleanly to a usable prompt box.

### 2. Empty-Session States & Common Slash Commands (`03_slash_commands`, `04_empty_side`)
- **Claude Code / Codex / Kimi Code parity:** Informational commands (`/help`, `/status`, `/model`, `/usage`, `/context`) behave gracefully on an empty session with zero errors; `/btw` / `/side` works even before the first main turn.
- [ ] `/help` overlay opens, paginates cleanly, and closes on `Esc` leaving the prompt intact.
- [ ] `/status`, `/usage`, and `/model` display meaningful empty-session states (no `NaN`, no raw error codes, no crashes).
- [ ] `/side` / `/btw` on a brand-new empty session answers cleanly (no `"source session has no history"` failure) and `Esc` returns cleanly to the main session.

### 3. Normal Turn & Keybindings (`05_first_prompt`, `06_ctrl_o`, `07_ctrl_t`, `08_shift_tab`)
- **Claude Code / Codex / Kimi Code parity:**
  - `Ctrl+O` toggles compact ↔ expanded tool output / reasoning in place, returning cleanly to compact on the second press.
  - `Ctrl+T` opens a full transcript view and restores the main screen cleanly on `Ctrl+T` / `Esc` / `q` with no ghost prompt boxes.
  - `Shift+Tab` cycles permission modes cleanly in trusted directories and opens the preset picker in untrusted directories instead of trapping the user in `READ`.
  - `Esc` cleanly dismisses overlays/pickers or interrupts an active turn without garbling the prompt.
- [ ] First prompt streams, finishes, and commits cleanly; large tool diffs/outputs collapse in compact view with `Ctrl+O to expand`.
- [ ] Pressing `Ctrl+O` once expands committed tool output; pressing `Ctrl+O` a second time collapses back to compact view.
- [ ] Pressing `Ctrl+T` opens the transcript overlay; pressing `Esc` returns to main with exactly one prompt box.
- [ ] Pressing `Shift+Tab` reflects the permission mode / picker clearly in the status bar and restores cleanly on `Esc`.

### 4. Side Conversation After a Turn (`09_side_after_turn`)
- **Claude Code / Codex / Kimi Code parity:** Side/btw Q&A shows clear visual separation from the main thread, renders the answer once, and discards cleanly on return.
- [ ] Side banner (`SIDE · Esc / /side back…`) and parent status are visible.
- [ ] Side reply appears **once** (no streaming + committed duplicate).
- [ ] `Esc` returns to the main transcript without altering main history.

### 5. Permission Prompt & Approval UX (`10_permission_prompt`)
- **Claude Code / Codex / Kimi Code parity:** Approval modals explain *what* tool/action is requested, *where* (full target path/command wrapped cleanly), and *which* keys approve once / session / deny.
- [ ] Approval modal renders cleanly with readable options (`Allow once`, `Allow for session`, `Deny`) and keyboard shortcuts.
- [ ] Approving or denying completes without stale overlay artifacts.

### 6. Subagent Flow (`11_subagent`)
- **Claude Code / Codex / Kimi Code parity:** Subagent launch, waiting indicator, and any escalated subagent approval clearly identify the subagent once, wrap long paths, state grant scope/expiration, and clarify that denying affects only that subagent.
- [ ] Subagent progress / completion renders clearly without colliding with the prompt box.
- [ ] If a subagent asks for approval, the requester name is not duplicated (`<name> · <name>`), target paths are not truncated, and waiting status is accurate.

### 7. Session Lifecycle: `/new` and `/resume` (`12_new_session`, `13_resume_session`, `14_quit`)
- **Claude Code / Codex / Kimi Code parity:** `/new` gives immediate visual confirmation of a fresh session (restoring the welcome banner); `/resume` lists prior sessions clearly and restores history cleanly; quitting restores terminal state.
- [ ] `/new` clears prior turns and brings back the welcome banner (version, cwd, model, hints).
- [ ] `/resume` picker opens, lists the prior session, and resuming restores its history cleanly.
- [ ] `Ctrl+D` exits cleanly with exit code 0 and leaves the terminal in cooked mode.

---

## Reviewer Report Format

When reporting findings from a run, include:

1. **Metadata:** Candidate version/SHA, platform, terminal size, model used, date.
2. **Findings table:** Step capture filename, severity (`blocker` / `major` / `minor`), observed behavior, expected behavior (citing Claude Code / Codex / Kimi Code parity), and created GitHub issue number.
3. **Verdict:** `READY` (zero blockers) or `HOLD` (list blocking issues to resolve before tagging).
