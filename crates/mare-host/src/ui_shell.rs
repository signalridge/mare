//! Per-session external UI attachment, widget/status/overlay/editor shell snapshot, takeover
//! tracking, and external-editor lock.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use mare_protocol::{
    UiEditorAttachment, UiEditorConfigParams, UiEditorStateParams, UiOverlayParams, UiShellSnapshot, UiStatusParams,
    UiViewport, UiWidgetParams, UiWorkingSnapshot,
};

use crate::ConnId;

/// One UI frontend attachment attached to this session's `Host`.
#[derive(Clone, Debug)]
pub struct UiAttachment {
    pub conn_id: ConnId,
    pub responder: bool,
    pub takeover: BTreeSet<String>,
    pub viewport: Option<UiViewport>,
}

#[derive(Default)]
struct Inner {
    attachments: HashMap<ConnId, UiAttachment>,
    viewport_seq: u64,
    viewport_order: HashMap<ConnId, u64>,
    widgets: BTreeMap<String, UiWidgetParams>,
    statuses: BTreeMap<String, UiStatusParams>,
    working: Option<UiWorkingSnapshot>,
    editor_config: Option<UiEditorConfigParams>,
    editor_hint: Option<String>,
    overlays: BTreeMap<String, UiOverlayParams>,
    flags: BTreeMap<String, bool>,
    editor_text: String,
    editor_cursor: usize,
    editor_shell_mode: bool,
    editor_image_markers: Vec<String>,
    editor_attachments: Vec<UiEditorAttachment>,
    editor_revision: u64,
    external_editor_owner: Option<ConnId>,
    callback_editor_active: bool,
    mirror_installed: bool,
}

impl Inner {
    fn editor_state(&self, session_id: Option<&str>) -> UiEditorStateParams {
        UiEditorStateParams {
            session_id: session_id.map(str::to_string),
            text: self.editor_text.clone(),
            cursor: self.editor_cursor,
            shell_mode: self.editor_shell_mode,
            image_markers: self.editor_image_markers.clone(),
            attachments: self.editor_attachments.clone(),
            external_editor_running: self.external_editor_owner.is_some() || self.callback_editor_active,
            revision: self.editor_revision,
        }
    }

    fn snapshot(&self, session_id: Option<&str>) -> UiShellSnapshot {
        UiShellSnapshot {
            widgets: self.widgets.values().cloned().collect(),
            statuses: self.statuses.values().cloned().collect(),
            working: self.working.clone(),
            editor_config: self.editor_config.clone(),
            editor_hint: self.editor_hint.clone(),
            editor_state: self.editor_state(session_id),
            overlays: self.overlays.values().cloned().collect(),
            flags: self.flags.clone(),
        }
    }
}

/// Per-host shell state cache and external UI attachment registry.
#[derive(Default)]
pub struct UiShellState {
    inner: RefCell<Inner>,
}

impl UiShellState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&self, att: UiAttachment, session_id: Option<&str>) -> (Vec<String>, UiShellSnapshot) {
        let mut guard = self.inner.borrow_mut();
        let takeover: Vec<String> = att.takeover.iter().cloned().collect();
        let conn_id = att.conn_id;
        let has_viewport = att.viewport.is_some();
        guard.attachments.insert(conn_id, att);
        if has_viewport {
            guard.viewport_seq = guard.viewport_seq.wrapping_add(1);
            let seq = guard.viewport_seq;
            guard.viewport_order.insert(conn_id, seq);
        } else {
            guard.viewport_order.remove(&conn_id);
        }
        (takeover, guard.snapshot(session_id))
    }

    /// Remove `conn_id`'s attachment; returns `(removed_attachment, editor_state_if_unlocked)`.
    pub fn detach(
        &self,
        conn_id: ConnId,
        session_id: Option<&str>,
    ) -> (Option<UiAttachment>, Option<UiEditorStateParams>) {
        let mut guard = self.inner.borrow_mut();
        let removed = guard.attachments.remove(&conn_id);
        guard.viewport_order.remove(&conn_id);
        let editor_changed = if guard.external_editor_owner == Some(conn_id) {
            guard.external_editor_owner = None;
            guard.editor_revision = guard.editor_revision.wrapping_add(1);
            Some(guard.editor_state(session_id))
        } else {
            None
        };
        (removed, editor_changed)
    }

    pub fn is_attached(&self, conn_id: ConnId) -> bool {
        self.inner.borrow().attachments.contains_key(&conn_id)
    }

    pub fn is_responder(&self, conn_id: ConnId) -> bool {
        self.inner.borrow().attachments.get(&conn_id).is_some_and(|a| a.responder)
    }

    pub fn clear_responder(&self, conn_id: ConnId) {
        if let Some(att) = self.inner.borrow_mut().attachments.get_mut(&conn_id) {
            att.responder = false;
        }
    }

    pub fn try_mark_mirror_installed(&self) -> bool {
        let mut guard = self.inner.borrow_mut();
        if guard.mirror_installed {
            false
        } else {
            guard.mirror_installed = true;
            true
        }
    }

    pub fn is_external_editor_running(&self) -> bool {
        let guard = self.inner.borrow();
        guard.external_editor_owner.is_some() || guard.callback_editor_active
    }

    pub fn peek_editor(&self) -> (String, Vec<UiEditorAttachment>) {
        let guard = self.inner.borrow();
        (guard.editor_text.clone(), guard.editor_attachments.clone())
    }

    pub fn peek_editor_with_revision(&self) -> (String, Vec<UiEditorAttachment>, u64) {
        let guard = self.inner.borrow();
        (guard.editor_text.clone(), guard.editor_attachments.clone(), guard.editor_revision)
    }

    pub fn set_viewport(&self, conn_id: ConnId, viewport: UiViewport) {
        let mut guard = self.inner.borrow_mut();
        if let Some(att) = guard.attachments.get_mut(&conn_id) {
            att.viewport = Some(viewport);
            guard.viewport_seq = guard.viewport_seq.wrapping_add(1);
            let seq = guard.viewport_seq;
            guard.viewport_order.insert(conn_id, seq);
        }
    }

    /// Return the most recently updated viewport among currently attached UI clients.
    pub fn active_viewport(&self) -> Option<UiViewport> {
        let guard = self.inner.borrow();
        guard
            .attachments
            .values()
            .filter_map(|att| {
                let vp = att.viewport.as_ref()?;
                let order = guard.viewport_order.get(&att.conn_id).copied().unwrap_or(0);
                Some(((order, att.conn_id.0), vp))
            })
            .max_by_key(|(key, _)| *key)
            .map(|(_, vp)| vp.clone())
    }

    /// Clear session-scoped visual shell state when the underlying host switches
    /// in-place to a different session (`/new`, `/resume`), preserving client
    /// attachments, viewport tracking, and editor configuration.
    pub fn reset_for_session_switch(&self) {
        let mut guard = self.inner.borrow_mut();
        guard.widgets.clear();
        guard.statuses.clear();
        guard.working = None;
        guard.overlays.clear();
        guard.flags.clear();
        guard.editor_hint = None;
    }

    pub fn attached_conns(&self) -> Vec<ConnId> {
        self.inner.borrow().attachments.keys().copied().collect()
    }

    /// Return connection ids that explicitly took over `component` (supporting `widget:*` wildcard).
    pub fn takeover_holders(&self, component: &str) -> Vec<ConnId> {
        let guard = self.inner.borrow();
        let is_widget = component.starts_with("widget:");
        guard
            .attachments
            .values()
            .filter(|att| att.takeover.contains(component) || (is_widget && att.takeover.contains("widget:*")))
            .map(|att| att.conn_id)
            .collect()
    }

    pub fn has_takeover(&self, conn_id: ConnId, component: &str) -> bool {
        let guard = self.inner.borrow();
        let is_widget = component.starts_with("widget:");
        guard
            .attachments
            .get(&conn_id)
            .is_some_and(|att| att.takeover.contains(component) || (is_widget && att.takeover.contains("widget:*")))
    }

    pub fn set_widget(&self, params: &UiWidgetParams) {
        let mut guard = self.inner.borrow_mut();
        if params.node.is_none() {
            guard.widgets.remove(&params.widget_id);
        } else {
            guard.widgets.insert(params.widget_id.clone(), params.clone());
        }
    }

    pub fn set_status(&self, params: &UiStatusParams) {
        let mut guard = self.inner.borrow_mut();
        if params.node.is_none() {
            guard.statuses.remove(&params.status_id);
        } else {
            guard.statuses.insert(params.status_id.clone(), params.clone());
        }
    }

    pub fn set_working(
        &self,
        label: Option<String>,
        detail: Option<String>,
        background: bool,
    ) -> Option<UiWorkingSnapshot> {
        let mut guard = self.inner.borrow_mut();
        if label.is_none() && detail.is_none() {
            guard.working = None;
        } else {
            guard.working = Some(UiWorkingSnapshot { label, detail, background });
        }
        guard.working.clone()
    }

    pub fn is_working(&self) -> bool {
        self.inner.borrow().working.is_some()
    }

    pub fn set_editor_config(&self, params: &UiEditorConfigParams) {
        self.inner.borrow_mut().editor_config = Some(params.clone());
    }

    pub fn set_editor_hint(&self, text: &str) {
        let mut guard = self.inner.borrow_mut();
        if text.is_empty() {
            guard.editor_hint = None;
        } else {
            guard.editor_hint = Some(text.to_string());
        }
    }

    pub fn set_overlay(&self, params: &UiOverlayParams) {
        let mut guard = self.inner.borrow_mut();
        if !params.open {
            guard.overlays.remove(&params.title);
        } else {
            guard.overlays.insert(params.title.clone(), params.clone());
        }
    }

    pub fn set_flag(&self, name: &str, value: bool) {
        self.inner.borrow_mut().flags.insert(name.to_string(), value);
    }

    /// Apply Lua `mare.ui.set_editor(text, {keep_draft, prepend, image_markers, ...})` onto the
    /// tracked draft state, returning the updated `UiEditorStateParams`.
    pub fn apply_set_editor(
        &self,
        session_id: Option<&str>,
        text: &str,
        keep_draft: bool,
        prepend: bool,
        image_markers: Vec<String>,
        attachments: Vec<UiEditorAttachment>,
    ) -> (bool, UiEditorStateParams) {
        let mut guard = self.inner.borrow_mut();
        if keep_draft && !guard.editor_text.is_empty() {
            return (false, guard.editor_state(session_id));
        }
        if prepend && !guard.editor_text.is_empty() {
            if text.is_empty() {
                return (false, guard.editor_state(session_id));
            }
            guard.editor_text = format!("{text}\n\n{}", guard.editor_text);
        } else {
            guard.editor_text = text.to_string();
        }
        guard.editor_cursor = guard.editor_text.chars().count();
        if !image_markers.is_empty() || !prepend {
            guard.editor_image_markers = image_markers;
        }
        if !attachments.is_empty() || !prepend {
            guard.editor_attachments = attachments;
        }
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        (true, guard.editor_state(session_id))
    }

    pub fn update_editor_input(
        &self,
        session_id: Option<&str>,
        text: String,
        cursor: Option<usize>,
        shell_mode: Option<bool>,
        image_markers: Option<Vec<String>>,
        attachments: Option<Vec<UiEditorAttachment>>,
    ) -> Result<UiEditorStateParams, &'static str> {
        let mut guard = self.inner.borrow_mut();
        if guard.external_editor_owner.is_some() {
            return Err("editor buffer is locked while external editor is active");
        }
        let char_len = text.chars().count();
        guard.editor_text = text;
        guard.editor_cursor = cursor.unwrap_or(char_len).min(char_len);
        if let Some(sh) = shell_mode {
            guard.editor_shell_mode = sh;
        }
        if let Some(markers) = image_markers {
            guard.editor_image_markers = markers;
        }
        if let Some(atts) = attachments {
            guard.editor_attachments = atts;
        }
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Ok(guard.editor_state(session_id))
    }

    pub fn take_editor_submit(
        &self,
        session_id: Option<&str>,
        override_text: Option<String>,
    ) -> (String, Vec<UiEditorAttachment>, UiEditorStateParams) {
        let mut guard = self.inner.borrow_mut();
        let submitted = override_text.unwrap_or_else(|| guard.editor_text.clone());
        let attachments = std::mem::take(&mut guard.editor_attachments);
        guard.editor_text.clear();
        guard.editor_cursor = 0;
        guard.editor_image_markers.clear();
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        let state = guard.editor_state(session_id);
        (submitted, attachments, state)
    }

    pub fn editor_is_empty(&self) -> bool {
        self.inner.borrow().editor_text.is_empty()
    }

    pub fn editor_revision(&self) -> u64 {
        self.inner.borrow().editor_revision
    }

    /// Snapshot and clear the shared draft if `editor_revision == expected_revision`, returning
    /// the saved draft fields alongside the cleared `UiEditorStateParams`.
    pub fn take_editor_draft_if_revision(
        &self,
        session_id: Option<&str>,
        expected_revision: u64,
    ) -> Option<(String, Vec<String>, Vec<UiEditorAttachment>, UiEditorStateParams)> {
        let mut guard = self.inner.borrow_mut();
        if guard.editor_revision != expected_revision {
            return None;
        }
        let text = std::mem::take(&mut guard.editor_text);
        let markers = std::mem::take(&mut guard.editor_image_markers);
        let attachments = std::mem::take(&mut guard.editor_attachments);
        guard.editor_cursor = 0;
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        let state = guard.editor_state(session_id);
        Some((text, markers, attachments, state))
    }

    /// Restore a previously taken draft only if `editor_revision` has not changed since clearing,
    /// avoiding clobbering input written by a submit handler or another client.
    pub fn restore_editor_if_revision(
        &self,
        session_id: Option<&str>,
        expected_revision: u64,
        text: String,
        image_markers: Vec<String>,
        attachments: Vec<UiEditorAttachment>,
    ) -> Option<UiEditorStateParams> {
        let mut guard = self.inner.borrow_mut();
        if guard.editor_revision != expected_revision {
            return None;
        }
        guard.editor_cursor = text.chars().count();
        guard.editor_text = text;
        guard.editor_image_markers = image_markers;
        guard.editor_attachments = attachments;
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Some(guard.editor_state(session_id))
    }

    pub fn set_callback_editor_active(
        &self,
        session_id: Option<&str>,
        active: bool,
    ) -> Result<UiEditorStateParams, &'static str> {
        let mut guard = self.inner.borrow_mut();
        if active && guard.external_editor_owner.is_some() {
            return Err("external editor is already active");
        }
        guard.callback_editor_active = active;
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Ok(guard.editor_state(session_id))
    }

    pub fn start_external_editor(
        &self,
        session_id: Option<&str>,
        conn_id: ConnId,
    ) -> Result<UiEditorStateParams, &'static str> {
        let mut guard = self.inner.borrow_mut();
        if guard.callback_editor_active {
            return Err("callback editor dialog is currently active");
        }
        if let Some(owner) = guard.external_editor_owner
            && owner != conn_id
        {
            return Err("external editor is already active on another connection");
        }
        guard.external_editor_owner = Some(conn_id);
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Ok(guard.editor_state(session_id))
    }

    pub fn commit_external_editor(
        &self,
        session_id: Option<&str>,
        conn_id: ConnId,
        text: Option<String>,
    ) -> Result<UiEditorStateParams, &'static str> {
        let mut guard = self.inner.borrow_mut();
        match guard.external_editor_owner {
            Some(owner) if owner == conn_id => {}
            Some(_) => return Err("external editor is owned by another connection"),
            None => return Err("external editor is not active"),
        }
        guard.external_editor_owner = None;
        if let Some(t) = text {
            guard.editor_cursor = t.chars().count();
            guard.editor_text = t;
        }
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Ok(guard.editor_state(session_id))
    }

    pub fn cancel_external_editor(
        &self,
        session_id: Option<&str>,
        conn_id: ConnId,
    ) -> Result<UiEditorStateParams, &'static str> {
        let mut guard = self.inner.borrow_mut();
        match guard.external_editor_owner {
            Some(owner) if owner == conn_id => {}
            Some(_) => return Err("external editor is owned by another connection"),
            None => return Err("external editor is not active"),
        }
        guard.external_editor_owner = None;
        guard.editor_revision = guard.editor_revision.wrapping_add(1);
        Ok(guard.editor_state(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_viewport_tracks_recency_and_restores_remaining_client_on_detach() {
        let shell = UiShellState::new();
        assert!(shell.active_viewport().is_none());

        shell.attach(
            UiAttachment {
                conn_id: ConnId(1),
                responder: true,
                takeover: BTreeSet::new(),
                viewport: Some(UiViewport { width: Some(120), height: Some(40), ..Default::default() }),
            },
            Some("s1"),
        );
        shell.attach(
            UiAttachment {
                conn_id: ConnId(2),
                responder: false,
                takeover: BTreeSet::new(),
                viewport: Some(UiViewport { width: Some(45), height: Some(20), ..Default::default() }),
            },
            Some("s1"),
        );
        let active = shell.active_viewport().expect("active viewport");
        assert_eq!((active.width, active.height), (Some(45), Some(20)));

        // Updating conn 1 makes it the most recent viewport.
        shell.set_viewport(ConnId(1), UiViewport { width: Some(100), height: Some(35), ..Default::default() });
        let active = shell.active_viewport().expect("active viewport after update");
        assert_eq!((active.width, active.height), (Some(100), Some(35)));

        // Detaching conn 1 falls back to conn 2's stored viewport.
        let _ = shell.detach(ConnId(1), Some("s1"));
        let active = shell.active_viewport().expect("fallback viewport");
        assert_eq!((active.width, active.height), (Some(45), Some(20)));

        // Detaching the final client yields None.
        let _ = shell.detach(ConnId(2), Some("s1"));
        assert!(shell.active_viewport().is_none());
    }

    #[test]
    fn reset_for_session_switch_clears_visual_state_and_preserves_attachments() {
        let shell = UiShellState::new();
        shell.attach(
            UiAttachment {
                conn_id: ConnId(1),
                responder: true,
                takeover: BTreeSet::new(),
                viewport: Some(UiViewport { width: Some(120), height: Some(40), ..Default::default() }),
            },
            Some("s1"),
        );
        shell.set_widget(&UiWidgetParams {
            session_id: Some("s1".into()),
            widget_id: "w1".into(),
            slot: "above-editor".into(),
            order: 1,
            node: Some(serde_json::json!({"type":"text","text":"w"})),
        });
        shell.set_status(&UiStatusParams {
            session_id: Some("s1".into()),
            status_id: "st1".into(),
            align: "right".into(),
            order: 1,
            keep: false,
            node: Some(serde_json::json!({"type":"text","text":"s"})),
        });
        shell.set_working(Some("running".into()), Some("detail".into()), false);
        shell.set_overlay(&UiOverlayParams {
            session_id: Some("s1".into()),
            title: "Help".into(),
            node: Some(serde_json::json!({"type":"text","text":"o"})),
            open: true,
            context: None,
        });
        shell.set_flag("f1", true);
        shell.set_editor_hint("hint");

        shell.reset_for_session_switch();

        assert!(shell.is_attached(ConnId(1)));
        assert_eq!(shell.active_viewport().and_then(|v| v.width), Some(120));
        let (_, snap) = shell.attach(
            UiAttachment { conn_id: ConnId(2), responder: false, takeover: BTreeSet::new(), viewport: None },
            Some("s2"),
        );
        assert!(snap.widgets.is_empty());
        assert!(snap.statuses.is_empty());
        assert!(snap.working.is_none());
        assert!(snap.overlays.is_empty());
        assert!(snap.flags.is_empty());
        assert!(snap.editor_hint.is_none());
    }
}
