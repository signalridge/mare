//! Core UI region registry, layout configuration validation, and slot/align checks.

use std::collections::{BTreeMap, BTreeSet};

/// Stacked regions rendered in the bottom live area (configurable order/options).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StackRegion {
    Live,
    Working,
    AboveEditor,
    Completion,
    Editor,
    BelowEditor,
    Status,
}

/// Overlay regions composited above the stacked regions (fixed z-order, not configurable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayRegion {
    WhichKey,
    Notices,
    Modal,
}

/// Default stacked region order (top to bottom).
pub const DEFAULT_STACK_ORDER: [StackRegion; 7] = [
    StackRegion::Live,
    StackRegion::Working,
    StackRegion::AboveEditor,
    StackRegion::Completion,
    StackRegion::Editor,
    StackRegion::BelowEditor,
    StackRegion::Status,
];

/// Fixed overlay z-order (bottom to top: `modal` on top, then `notices`, then `which-key`).
pub const OVERLAY_Z_ORDER: [OverlayRegion; 3] = [OverlayRegion::WhichKey, OverlayRegion::Notices, OverlayRegion::Modal];

/// Valid widget slot names.
pub const WIDGET_SLOTS: &[&str] = &["above-editor", "below-editor"];

/// Valid status segment align names.
pub const STATUS_ALIGNS: &[&str] = &["left", "right"];

/// Maximum allowed byte length for a region, slot, or align name.
pub const MAX_REGION_NAME_LEN: usize = 64;

/// Metadata for a stacked region in the core registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionSpec {
    pub region: StackRegion,
    pub name: &'static str,
    pub order_configurable: bool,
    pub hideable: bool,
    pub has_max_height: bool,
    pub required: bool,
}

/// Core table of all stacked regions and their constraints.
pub const STACK_REGIONS: &[RegionSpec] = &[
    RegionSpec {
        region: StackRegion::Live,
        name: "live",
        order_configurable: true,
        hideable: false,
        has_max_height: false,
        required: false,
    },
    RegionSpec {
        region: StackRegion::Working,
        name: "working",
        order_configurable: true,
        hideable: true,
        has_max_height: false,
        required: false,
    },
    RegionSpec {
        region: StackRegion::AboveEditor,
        name: "above-editor",
        order_configurable: true,
        hideable: true,
        has_max_height: true,
        required: false,
    },
    RegionSpec {
        region: StackRegion::Completion,
        name: "completion",
        order_configurable: false,
        hideable: false,
        has_max_height: true,
        required: false,
    },
    RegionSpec {
        region: StackRegion::Editor,
        name: "editor",
        order_configurable: true,
        hideable: false,
        has_max_height: true,
        required: true,
    },
    RegionSpec {
        region: StackRegion::BelowEditor,
        name: "below-editor",
        order_configurable: true,
        hideable: true,
        has_max_height: true,
        required: false,
    },
    RegionSpec {
        region: StackRegion::Status,
        name: "status",
        order_configurable: true,
        hideable: false,
        has_max_height: true,
        required: true,
    },
];

impl StackRegion {
    pub fn as_str(self) -> &'static str {
        self.spec().name
    }

    pub fn spec(self) -> &'static RegionSpec {
        STACK_REGIONS.iter().find(|s| s.region == self).expect("all StackRegion variants are registered")
    }

    pub fn parse(name: &str) -> Option<Self> {
        STACK_REGIONS.iter().find(|s| s.name == name).map(|s| s.region)
    }
}

impl OverlayRegion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WhichKey => "which-key",
            Self::Notices => "notices",
            Self::Modal => "modal",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "which-key" => Some(Self::WhichKey),
            "notices" => Some(Self::Notices),
            "modal" => Some(Self::Modal),
            _ => None,
        }
    }
}

/// Per-region configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionConfig {
    pub max_height: Option<usize>,
    pub hidden: bool,
}

/// Unvalidated per-region configuration parsed from Lua/config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawRegionConfig {
    pub max_height: Option<usize>,
    pub hidden: Option<bool>,
}

/// Validated layout configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutConfig {
    pub order: Vec<StackRegion>,
    pub regions: BTreeMap<StackRegion, RegionConfig>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self { order: DEFAULT_STACK_ORDER.to_vec(), regions: BTreeMap::new() }
    }
}

impl LayoutConfig {
    pub fn region(&self, r: StackRegion) -> RegionConfig {
        self.regions.get(&r).copied().unwrap_or_default()
    }

    pub fn is_hidden(&self, r: StackRegion) -> bool {
        self.region(r).hidden
    }

    pub fn max_height(&self, r: StackRegion) -> Option<usize> {
        self.region(r).max_height
    }

    /// Validate a user-provided layout order and per-region overrides.
    pub fn validate(
        order: Option<Vec<String>>,
        raw_regions: BTreeMap<String, RawRegionConfig>,
    ) -> Result<Self, String> {
        let resolved_order = match order {
            None => DEFAULT_STACK_ORDER.to_vec(),
            Some(names) => {
                let mut seen = BTreeSet::new();
                let mut parsed = Vec::with_capacity(names.len());
                for name in names {
                    let region = parse_stack_region_error(&name)?;
                    if !seen.insert(region) {
                        return Err(format!("duplicate region `{name}` in ui.layout"));
                    }
                    parsed.push(region);
                }
                let editor_pos = parsed
                    .iter()
                    .position(|&r| r == StackRegion::Editor)
                    .ok_or_else(|| "ui.layout must include `editor`".to_string())?;
                if !parsed.contains(&StackRegion::Status) {
                    return Err("ui.layout must include `status`".to_string());
                }
                if let Some(comp_pos) = parsed.iter().position(|&r| r == StackRegion::Completion) {
                    if comp_pos.abs_diff(editor_pos) != 1 {
                        return Err("`completion` must be adjacent to `editor` in ui.layout".to_string());
                    }
                } else {
                    parsed.insert(editor_pos, StackRegion::Completion);
                }
                if !parsed.contains(&StackRegion::Live) {
                    parsed.insert(0, StackRegion::Live);
                }
                parsed
            }
        };

        let mut regions = BTreeMap::new();
        for (name, raw) in raw_regions {
            let region = parse_stack_region_error(&name)?;
            let spec = region.spec();
            if raw.hidden == Some(true) && !spec.hideable {
                return Err(format!("region `{name}` cannot be hidden"));
            }
            if let Some(mh) = raw.max_height {
                if !spec.has_max_height {
                    return Err(format!("region `{name}` does not support max_height"));
                }
                let min_h = if region == StackRegion::Completion { 3 } else { 1 };
                if mh < min_h {
                    return Err(format!("max_height for region `{name}` must be at least {min_h}"));
                }
            }
            regions.insert(region, RegionConfig { max_height: raw.max_height, hidden: raw.hidden.unwrap_or(false) });
        }

        Ok(Self { order: resolved_order, regions })
    }
}

fn parse_stack_region_error(name: &str) -> Result<StackRegion, String> {
    if name.len() > MAX_REGION_NAME_LEN {
        return Err(format!("unknown layout region (name too long: {} bytes, max {MAX_REGION_NAME_LEN})", name.len()));
    }
    if let Some(r) = StackRegion::parse(name) {
        return Ok(r);
    }
    if OverlayRegion::parse(name).is_some() {
        return Err(format!("`{name}` is an overlay region and cannot be configured in ui.layout"));
    }
    let candidates: Vec<&str> = STACK_REGIONS.iter().map(|s| s.name).collect();
    Err(format!("unknown layout region `{name}`{}", closest_hint(name, &candidates)))
}

/// Validate a widget `slot` name, returning an error with a closest-match hint when invalid.
pub fn validate_widget_slot(slot: &str) -> Result<&'static str, String> {
    if slot.len() > MAX_REGION_NAME_LEN {
        return Err(format!("unknown widget slot (name too long: {} bytes, max {MAX_REGION_NAME_LEN})", slot.len()));
    }
    WIDGET_SLOTS
        .iter()
        .copied()
        .find(|&s| s == slot)
        .ok_or_else(|| format!("unknown widget slot `{slot}`{}", closest_hint(slot, WIDGET_SLOTS)))
}

/// Validate a status `align` name, returning an error with a closest-match hint when invalid.
pub fn validate_status_align(align: &str) -> Result<&'static str, String> {
    if align.len() > MAX_REGION_NAME_LEN {
        return Err(format!("unknown status align (name too long: {} bytes, max {MAX_REGION_NAME_LEN})", align.len()));
    }
    STATUS_ALIGNS
        .iter()
        .copied()
        .find(|&s| s == align)
        .ok_or_else(|| format!("unknown status align `{align}`{}", closest_hint(align, STATUS_ALIGNS)))
}

fn closest_hint(input: &str, candidates: &[&str]) -> String {
    match closest_match(input, candidates) {
        Some(best) => format!(" — did you mean `{best}`?"),
        None => format!(" (expected one of: {})", candidates.join(", ")),
    }
}

/// Optimal string alignment distance (Levenshtein + adjacent transpositions) in O(min(m, n)) space.
fn osa_distance(a: &str, b: &str) -> usize {
    let mut a: Vec<char> = a.chars().collect();
    let mut b: Vec<char> = b.chars().collect();
    if a.len() < b.len() {
        std::mem::swap(&mut a, &mut b);
    }
    let (n, m) = (a.len(), b.len());
    let mut prev_prev = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut val = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                val = val.min(prev_prev[j - 2] + 1);
            }
            curr[j] = val;
        }
        std::mem::swap(&mut prev_prev, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// The candidate nearest to `input` within a small edit distance, for a "did you mean" hint (also
/// the kernel's unknown plugin-definition fields).
pub fn closest_match<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if input.len() > MAX_REGION_NAME_LEN {
        return None;
    }
    let input_chars = input.chars().count();
    let max_dist = (input_chars / 2).max(2);
    let mut best: Option<(&'a str, usize)> = None;
    for &c in candidates {
        let cand_chars = c.chars().count();
        if input_chars.abs_diff(cand_chars) > max_dist {
            continue;
        }
        let d = osa_distance(input, c);
        if d <= max_dist && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    best.map(|(c, _)| c)
}

/// Rendered lines for a single stacked region, used for height-insufficient clipping.
pub struct RegionLines {
    pub kind: StackRegion,
    pub lines: Vec<crate::style::Line>,
    pub cursor: Option<(usize, usize)>,
}

/// Shrink stacked regions when total height exceeds `max_h` in layout order:
/// `live` -> slots (`above-editor`, `below-editor`) -> `working` -> `completion`
/// -> `status` non-`keep` segments -> `editor` (preserving at least the cursor row).
pub fn clip_regions(
    regions: &mut [RegionLines],
    max_h: usize,
    live_stream_end: &mut usize,
    render_kept_status: impl Fn() -> crate::style::Line,
) {
    let total = |rs: &[RegionLines]| rs.iter().map(|r| r.lines.len()).sum::<usize>();
    let mut excess = total(regions).saturating_sub(max_h);
    if excess == 0 {
        return;
    }

    // 1. `live` shrinks from the top (keep newest live output at the bottom).
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::Live) {
        let cut = excess.min(r.lines.len());
        if cut > 0 {
            r.lines.drain(..cut);
            *live_stream_end = live_stream_end.saturating_sub(cut);
            excess -= cut;
        }
    }
    if excess == 0 {
        return;
    }

    // 2. Widget slots shrink next (`above-editor` from top, `below-editor` from bottom).
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::AboveEditor) {
        let cut = excess.min(r.lines.len());
        if cut > 0 {
            r.lines.drain(..cut);
            excess -= cut;
        }
    }
    if excess == 0 {
        return;
    }
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::BelowEditor) {
        let cut = excess.min(r.lines.len());
        if cut > 0 {
            r.lines.truncate(r.lines.len() - cut);
            excess -= cut;
        }
    }
    if excess == 0 {
        return;
    }

    // 3. `working` shrinks next.
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::Working) {
        let cut = excess.min(r.lines.len());
        if cut > 0 {
            r.lines.truncate(r.lines.len() - cut);
            excess -= cut;
        }
    }
    if excess == 0 {
        return;
    }

    // 4. `completion` shrinks before `status` / `editor`.
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::Completion) {
        let cut = excess.min(r.lines.len());
        if cut > 0 {
            r.lines.truncate(r.lines.len() - cut);
            excess -= cut;
        }
    }
    if excess == 0 {
        return;
    }

    // 5. `status` drops non-`keep` segments.
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::Status) {
        let kept = render_kept_status();
        r.lines = if kept.spans.is_empty() { vec![] } else { vec![kept] };
    }
    excess = total(regions).saturating_sub(max_h);
    if excess == 0 {
        return;
    }

    // 6. `editor` shrinks while keeping at least the cursor row.
    for r in regions.iter_mut().filter(|r| r.kind == StackRegion::Editor) {
        if r.lines.len() <= 1 {
            continue;
        }
        let keep = r.lines.len().saturating_sub(excess).max(1);
        let (cr, cc) = r.cursor.unwrap_or((0, 0));
        let start = cr.saturating_sub(keep - 1).min(r.lines.len() - keep);
        r.lines = r.lines.drain(start..start + keep).collect();
        r.cursor = Some((cr.saturating_sub(start), cc));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_reordered_and_auto_places_completion() {
        let cfg = LayoutConfig::validate(Some(vec!["status".into(), "live".into(), "editor".into()]), BTreeMap::new())
            .unwrap();
        assert_eq!(
            cfg.order,
            vec![StackRegion::Status, StackRegion::Live, StackRegion::Completion, StackRegion::Editor]
        );
    }

    #[test]
    fn validate_rejects_missing_duplicate_non_adjacent_and_unknown() {
        assert!(LayoutConfig::validate(Some(vec!["live".into(), "status".into()]), BTreeMap::new()).is_err());
        assert!(LayoutConfig::validate(Some(vec!["live".into(), "editor".into()]), BTreeMap::new()).is_err());
        assert!(
            LayoutConfig::validate(
                Some(vec!["live".into(), "editor".into(), "editor".into(), "status".into()]),
                BTreeMap::new()
            )
            .is_err()
        );
        assert!(
            LayoutConfig::validate(
                Some(vec!["completion".into(), "live".into(), "editor".into(), "status".into()]),
                BTreeMap::new()
            )
            .is_err()
        );
        let typo = LayoutConfig::validate(Some(vec!["live".into(), "editer".into(), "status".into()]), BTreeMap::new())
            .unwrap_err();
        assert!(typo.contains("editor"), "{typo}");
        let overlay = LayoutConfig::validate(
            Some(vec!["live".into(), "modal".into(), "editor".into(), "status".into()]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(overlay.contains("overlay"), "{overlay}");
    }

    #[test]
    fn validate_checks_region_options() {
        let mut regions = BTreeMap::new();
        regions.insert("editor".into(), RawRegionConfig { max_height: None, hidden: Some(true) });
        assert!(LayoutConfig::validate(None, regions).is_err());

        let mut regions = BTreeMap::new();
        regions.insert("live".into(), RawRegionConfig { max_height: Some(3), hidden: None });
        assert!(LayoutConfig::validate(None, regions).is_err());

        let mut regions = BTreeMap::new();
        regions.insert("above-editor".into(), RawRegionConfig { max_height: Some(0), hidden: None });
        assert!(LayoutConfig::validate(None, regions).is_err());

        let mut regions = BTreeMap::new();
        regions.insert("above-editor".into(), RawRegionConfig { max_height: Some(4), hidden: Some(true) });
        let cfg = LayoutConfig::validate(None, regions).unwrap();
        assert!(cfg.is_hidden(StackRegion::AboveEditor));
        assert_eq!(cfg.max_height(StackRegion::AboveEditor), Some(4));
    }

    #[test]
    fn slot_and_align_validation_suggests_closest_match() {
        assert_eq!(validate_widget_slot("above-editor").unwrap(), "above-editor");
        assert_eq!(validate_widget_slot("below-editor").unwrap(), "below-editor");
        let e = validate_widget_slot("above_editor").unwrap_err();
        assert!(e.contains("above-editor"), "{e}");

        assert_eq!(validate_status_align("left").unwrap(), "left");
        assert_eq!(validate_status_align("right").unwrap(), "right");
        let e = validate_status_align("rihgt").unwrap_err();
        assert!(e.contains("right"), "{e}");
    }

    #[test]
    fn validate_reinserts_live_and_enforces_completion_min_height() {
        let cfg = LayoutConfig::validate(Some(vec!["status".into(), "editor".into()]), BTreeMap::new()).unwrap();
        assert_eq!(
            cfg.order,
            vec![StackRegion::Live, StackRegion::Status, StackRegion::Completion, StackRegion::Editor]
        );

        let mut regions = BTreeMap::new();
        regions.insert("completion".into(), RawRegionConfig { max_height: Some(2), hidden: None });
        let err = LayoutConfig::validate(None, regions).unwrap_err();
        assert!(err.contains("at least 3"), "{err}");

        let mut regions = BTreeMap::new();
        regions.insert("completion".into(), RawRegionConfig { max_height: Some(3), hidden: None });
        assert!(LayoutConfig::validate(None, regions).is_ok());
    }

    #[test]
    fn huge_region_slot_and_align_names_are_rejected_without_amplification() {
        let huge = "x".repeat(1024 * 1024);
        let err = LayoutConfig::validate(Some(vec![huge.clone(), "editor".into(), "status".into()]), BTreeMap::new())
            .unwrap_err();
        assert!(err.contains("too long"), "{err}");

        let err_slot = validate_widget_slot(&huge).unwrap_err();
        assert!(err_slot.contains("too long"), "{err_slot}");

        let err_align = validate_status_align(&huge).unwrap_err();
        assert!(err_align.contains("too long"), "{err_align}");
    }
}
