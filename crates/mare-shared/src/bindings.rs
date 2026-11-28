//! Key bindings by context: the core's copy of `mare.keymap`'s stacks. Lua owns
//! the stacks (`preset/keymap.lua`); every change sends one key's whole stack for one context
//! ([`crate::ui::UiCmd::SetBindings`]), so this is a cache the key handler reads without Lua.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

static KEYMAP_GEN: AtomicU64 = AtomicU64::new(1);

fn next_keymap_gen() -> u64 {
    KEYMAP_GEN.fetch_add(1, Ordering::Relaxed)
}

use crate::keymap_contexts::is_valid_context_segment;
pub use crate::keymap_contexts::{CORE_ACTIONS, CORE_CONTEXTS, ContextDef, CoreAction, find_core_action};

/// The predicates a binding's `when` may name: a closed set the core reads from its own state,
/// one bit each. Plugins can't add Lua predicates: that would put Lua back on
/// every key; an action that declines covers the rare complex case.
pub const EDITOR_EMPTY: u8 = 1;
pub const COMPLETION_OPEN: u8 = 2;
pub const PASTE_BURST: u8 = 4;
pub const WORKING: u8 = 8;
const PREDICATES: [(&str, u8); 4] = [
    ("editor_empty", EDITOR_EMPTY),
    ("completion_open", COMPLETION_OPEN),
    ("paste_burst", PASTE_BURST),
    ("working", WORKING),
];

/// A binding's condition: every predicate of `all` holds and none of `none` does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct When {
    pub all: u8,
    pub none: u8,
}

impl When {
    /// `when = "editor_empty"` or `{ "editor_empty", "!completion_open" }`: all must hold, `!`
    /// negates, there is no "or" (bind twice instead).
    pub fn parse<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut w = Self::default();
        for name in names {
            let (neg, bare) = name.strip_prefix('!').map_or((false, name), |b| (true, b));
            let Some(&(_, bit)) = PREDICATES.iter().find(|(n, _)| *n == bare) else {
                let known: Vec<&str> = PREDICATES.iter().map(|(n, _)| *n).collect();
                return Err(format!("unknown predicate \"{bare}\" in `when` — known: {}", known.join(", ")));
            };
            if neg {
                w.none |= bit;
            } else {
                w.all |= bit;
            }
        }
        Ok(w)
    }

    pub fn holds(self, state: u8) -> bool {
        state & self.all == self.all && state & self.none == 0
    }

    /// no condition: the binding always takes the key (and hides those below it)
    pub fn always(self) -> bool {
        self.all == 0 && self.none == 0
    }

    /// Whether two `When` conditions can hold simultaneously (not mutually exclusive: neither
    /// requires a predicate `p` while the other requires `!p`).
    pub fn overlaps(self, other: Self) -> bool {
        (self.all & other.none) == 0 && (self.none & other.all) == 0
    }

    /// 16-bit mask of all combinations of the 4 predicate bits (`0..16`) for which `self.holds(s)`.
    pub fn state_mask(self) -> u16 {
        let mut mask = 0u16;
        for s in 0..16u8 {
            if self.holds(s) {
                mask |= 1u16 << s;
            }
        }
        mask
    }

    /// Human-readable predicate labels for `?` help display.
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, bit) in PREDICATES {
            if (self.all & bit) != 0 {
                out.push(name);
            }
        }
        for (idx, (_, bit)) in PREDICATES.iter().enumerate() {
            if (self.none & bit) != 0 {
                const NEG: [&str; 4] = ["!editor_empty", "!completion_open", "!paste_burst", "!working"];
                out.push(NEG[idx]);
            }
        }
        out
    }
}

/// Explicit priority within a keymap source layer (higher wins).
#[derive(Clone, Copy, Debug, Default)]
pub struct Priority(pub f64);

impl Priority {
    pub fn new(v: f64) -> Result<Self, String> {
        if !v.is_finite() {
            return Err("keymap priority must be a finite number".into());
        }
        Ok(Self(if v == 0.0 { 0.0 } else { v }))
    }
}

impl PartialEq for Priority {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}

impl Eq for Priority {}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// One binding of a key in a context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// a core action ("transcript"), a named Lua action, or a Lua function id ("fn:3")
    pub action: String,
    /// an explicit unbinding that masks every entry below it
    pub unbound: bool,
    pub when: When,
    /// a Lua action or function: it can decline (return `false`), so the key goes on to the
    /// binding below it. Core actions and unregistered names take the key
    pub declines: bool,
    /// source rank: default 1 < plugin 2 < user 3 < project 4 (higher outranks in `hint`)
    pub rank: u8,
    /// explicit priority within a source layer (higher outranks within the same rank)
    pub priority: Priority,
    /// registration order across all keys (smaller is earlier within the same rank and priority)
    pub seq: u64,
}

#[derive(Clone, Debug, Default)]
struct LayerMap {
    /// key → stack, bottom first
    binds: HashMap<String, Vec<Entry>>,
    /// every proper prefix of a key sequence ("ctrl+x" of "ctrl+x g"), precomputed so an unbound
    /// key costs one lookup instead of a scan
    prefixes: HashSet<String>,
    /// prefixes of sequence entries carrying `unbound` masks so child contexts can suppress
    /// parent sequences during prefix checks
    mask_prefixes: HashSet<String>,
}

impl LayerMap {
    fn reindex(&mut self) {
        self.prefixes.clear();
        self.mask_prefixes.clear();
        for (key, stack) in &self.binds {
            let has_active = !stack.last().is_some_and(|entry| entry.unbound);
            let has_mask = stack.iter().any(|entry| entry.unbound);
            if !has_active && !has_mask {
                continue;
            }
            let mut at = 0;
            while let Some(i) = key[at..].find(' ') {
                let pref = &key[..at + i];
                if has_active {
                    self.prefixes.insert(pref.to_string());
                }
                if has_mask {
                    self.mask_prefixes.insert(pref.to_string());
                }
                at += i + 1;
            }
        }
    }
}

/// Whether a canonical key string represents an unmodified printable character (without Ctrl/Alt/Super).
pub fn is_printable_key_name(key: &str) -> bool {
    if key.contains(' ') {
        return false;
    }
    let mut rest = key;
    while let Some(r) = rest.strip_prefix("shift+") {
        rest = r;
    }
    rest == "space" || rest.chars().count() == 1
}

fn unknown_context_err(context: &str) -> String {
    format!(
        "unknown context \"{context}\" — use \"editor\", \"transcript\", \"text\", \"global\", or a declared subcontext"
    )
}

#[derive(Clone, Debug)]
pub struct Keymap {
    contexts: HashMap<String, ContextDef>,
    maps: HashMap<String, LayerMap>,
    /// Activated plugin subcontexts, most recently activated first.
    active_subcontexts: Vec<String>,
    /// Monotonic counter bumped on every mutation so TUI sync can short-circuit in O(1).
    generation: u64,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::new()
    }
}

impl Keymap {
    /// Keymap pre-populated with all core contexts (bindable and reserved).
    pub fn new() -> Self {
        let mut contexts = HashMap::new();
        let mut maps = HashMap::new();
        for &(name, parents, bindable) in CORE_CONTEXTS {
            contexts.insert(
                name.to_string(),
                ContextDef {
                    parents: parents.iter().map(|&p| p.to_string()).collect(),
                    core: true,
                    bindable,
                    swallow_printable: false,
                },
            );
            if bindable {
                maps.insert(name.to_string(), LayerMap::default());
            }
        }
        Self { contexts, maps, active_subcontexts: Vec::new(), generation: 0 }
    }

    pub fn context_def(&self, name: &str) -> Option<&ContextDef> {
        self.contexts.get(name)
    }

    /// Check that `context` exists and can be bound in this wave.
    pub fn check_bindable(&self, context: &str) -> Result<(), String> {
        match self.contexts.get(context) {
            Some(def) if def.bindable => Ok(()),
            Some(_) => Err(format!("context \"{context}\" can't be bound yet (its keys belong to the dialog)")),
            None => Err(unknown_context_err(context)),
        }
    }

    /// Context lookup chain starting at `context`: `context`, then its parents recursively.
    pub fn chain_for<'a>(&'a self, context: &'a str) -> Result<Vec<&'a str>, String> {
        let Some(key) = self.contexts.get_key_value(context).map(|(k, _)| k.as_str()) else {
            return Err(unknown_context_err(context));
        };
        let mut out = Vec::new();
        self.push_chain(key, &mut out);
        Ok(out)
    }

    fn push_chain<'a>(&'a self, ctx: &'a str, out: &mut Vec<&'a str>) {
        if !out.contains(&ctx) {
            out.push(ctx);
        }
        if let Some(def) = self.contexts.get(ctx) {
            for p in &def.parents {
                self.push_chain(p.as_str(), out);
            }
        }
    }

    /// Sequences are allowed in `editor`, `transcript`, `select`, and their subcontexts.
    pub fn allows_sequences(&self, context: &str) -> bool {
        self.chain_for(context)
            .is_ok_and(|chain| chain.iter().any(|c| matches!(*c, "editor" | "transcript" | "select")))
    }

    /// Validate a binding target (context bindability, sequence allowance, core action support).
    pub fn check_binding_target(&self, context: &str, key: &str, action: Option<&str>) -> Result<(), String> {
        self.check_bindable(context)?;
        if key.contains(' ') && !self.allows_sequences(context) {
            return Err(format!(
                "key sequences work in \"editor\", \"transcript\", or \"select\" contexts, not \"{context}\""
            ));
        }
        if let Some(act) = action
            && let Some(core) = find_core_action(act)
        {
            let chain = self.chain_for(context)?;
            if !core.contexts.iter().any(|allowed| chain.contains(allowed)) {
                return Err(format!(
                    "action \"{act}\" is not available in context \"{context}\" (it works in: {})",
                    core.contexts.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Declare a plugin subcontext `name` under bindable `parent` (`parent.<seg>`).
    pub fn register_context(&mut self, name: String, parent: String) -> Result<(), String> {
        self.register_context_with(name, parent, false, false)
    }

    /// Declare a plugin subcontext `name` under bindable `parent` (`parent.<seg>`) with options.
    pub fn register_context_with(
        &mut self,
        name: String,
        parent: String,
        swallow_printable: bool,
        active: bool,
    ) -> Result<(), String> {
        self.check_bindable(&parent)?;
        let Some(seg) = name.strip_prefix(&parent).and_then(|rest| rest.strip_prefix('.')) else {
            return Err(format!("context \"{name}\" must start with \"{parent}.\""));
        };
        if !is_valid_context_segment(seg) {
            return Err(format!("invalid context segment \"{seg}\" in \"{name}\" (expected [a-z][a-z0-9_-]*)"));
        }
        if self.contexts.contains_key(&name) {
            return Err(format!("context \"{name}\" already exists"));
        }
        self.contexts
            .insert(name.clone(), ContextDef { parents: vec![parent], core: false, bindable: true, swallow_printable });
        self.maps.insert(name.clone(), LayerMap::default());
        if active {
            self.active_subcontexts.retain(|c| c != &name);
            self.active_subcontexts.insert(0, name);
        }
        self.generation = next_keymap_gen();
        Ok(())
    }

    /// Toggle whether a declared plugin subcontext is active in focus lookup chains.
    pub fn set_context_active(&mut self, name: &str, active: bool) -> Result<(), String> {
        let Some(def) = self.contexts.get(name) else {
            return Err(unknown_context_err(name));
        };
        if def.core {
            return Err(format!("core context \"{name}\" activation is managed by the TUI"));
        }
        self.active_subcontexts.retain(|c| c != name);
        if active {
            self.active_subcontexts.insert(0, name.to_string());
        }
        self.generation = next_keymap_gen();
        Ok(())
    }

    /// Whether a plugin subcontext is currently active.
    pub fn is_context_active(&self, name: &str) -> bool {
        self.active_subcontexts.iter().any(|c| c == name)
    }

    /// Active plugin subcontexts that inherit from `base`, most recently activated first.
    pub fn active_subcontexts_for(&self, base: &str) -> Vec<String> {
        self.active_subcontexts
            .iter()
            .filter(|name| self.chain_for(name).is_ok_and(|chain| chain.contains(&base)))
            .cloned()
            .collect()
    }

    /// Synchronize bindings, contexts, and active subcontexts from `other` (O(1) when unchanged).
    pub fn sync_from(&mut self, other: &Self) {
        if self.generation == other.generation {
            return;
        }
        *self = other.clone();
    }

    /// Whether any context in `chain` requests swallowing unbound printable keys.
    pub fn should_swallow_printable(&self, chain: &[&str]) -> bool {
        chain.iter().any(|&ctx| self.contexts.get(ctx).is_some_and(|d| d.swallow_printable))
    }

    /// Remove a non-core context and all its descendants, returning the removed names.
    pub fn remove_context(&mut self, name: &str) -> Vec<String> {
        if self.contexts.get(name).is_none_or(|d| d.core) {
            return Vec::new();
        }
        let mut doomed: Vec<String> = self
            .contexts
            .iter()
            .filter(|(_, def)| !def.core)
            .filter_map(|(c, _)| self.chain_for(c).ok().filter(|chain| chain.contains(&name)).map(|_| c.clone()))
            .collect();
        doomed.sort();
        for c in &doomed {
            self.contexts.remove(c);
            self.maps.remove(c);
        }
        self.active_subcontexts.retain(|c| !doomed.contains(c));
        if !doomed.is_empty() {
            self.generation = next_keymap_gen();
        }
        doomed
    }

    /// Apply a `UiCmd::SetContext` delta on the TUI's local copy.
    pub fn set_context(&mut self, name: String, parent: Option<String>, swallow_printable: bool, active: bool) {
        match parent {
            Some(p) => {
                self.contexts.entry(name.clone()).or_insert(ContextDef {
                    parents: vec![p],
                    core: false,
                    bindable: true,
                    swallow_printable,
                });
                if let Some(def) = self.contexts.get_mut(&name) {
                    def.swallow_printable = swallow_printable;
                }
                self.maps.entry(name.clone()).or_default();
                self.active_subcontexts.retain(|c| c != &name);
                if active {
                    self.active_subcontexts.insert(0, name);
                }
                self.generation = next_keymap_gen();
            }
            None => {
                self.remove_context(&name);
            }
        }
    }

    /// Sorted names of every currently bindable context.
    pub fn bindable_contexts(&self) -> Vec<&str> {
        let mut out: Vec<&str> =
            self.contexts.iter().filter(|(_, def)| def.bindable).map(|(k, _)| k.as_str()).collect();
        out.sort();
        out
    }

    /// Replace `key`'s stack in `context` (bottom first); an empty stack unbinds it.
    /// Returns `true` if the stored stack actually changed.
    pub fn set(&mut self, context: &str, key: String, entries: Vec<Entry>) -> bool {
        let l = self.maps.entry(context.to_string()).or_default();
        let seq = key.contains(' ');
        let changed = if entries.is_empty() {
            l.binds.remove(&key).is_some()
        } else if l.binds.get(&key) == Some(&entries) {
            false
        } else {
            l.binds.insert(key, entries);
            true
        };
        if changed {
            if seq {
                l.reindex();
            }
            self.generation = next_keymap_gen();
        }
        changed
    }

    /// `key`'s stack in `context`, bottom first (empty when no entries are registered).
    pub fn stack(&self, context: &str, key: &str) -> &[Entry] {
        self.maps.get(context).and_then(|l| l.binds.get(key)).map_or(&[], Vec::as_slice)
    }

    /// `key` isn't bound itself in the chain (nor blocked by a mask) and starts a longer binding.
    pub fn is_prefix(&self, chain: &[&str], key: &str) -> bool {
        self.is_prefix_for_state(chain, key, 0)
    }

    /// Like [`Self::is_prefix`], evaluated against the active predicate bits `state`.
    /// A single-key binding whose `when` does not hold in `state` does not block prefix mode,
    /// and a longer sequence whose `when` does not hold in `state` does not enter prefix mode.
    pub fn is_prefix_for_state(&self, chain: &[&str], key: &str, state: u8) -> bool {
        let prefix = format!("{key} ");
        let state_bit = 1u16 << (state & 0xF);
        let mut key_blocked = false;
        let mut seq_blocked: HashMap<&str, u16> = HashMap::new();
        for &ctx in chain {
            let Some(l) = self.maps.get(ctx) else { continue };
            if !key_blocked && let Some(stack) = l.binds.get(key) {
                for e in stack.iter().rev() {
                    if e.unbound {
                        key_blocked = true;
                        break;
                    }
                    if e.when.holds(state) {
                        return false;
                    }
                }
            }
            if l.prefixes.contains(key) || l.mask_prefixes.contains(key) {
                for (seq, stack) in &l.binds {
                    if seq.starts_with(&prefix) {
                        for e in stack.iter().rev() {
                            let cov = seq_blocked.get(seq.as_str()).copied().unwrap_or(0);
                            if e.unbound {
                                seq_blocked.insert(seq.as_str(), cov | e.when.state_mask());
                                break;
                            }
                            if (state_bit & !cov) != 0 && e.when.holds(state) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Whether any binding of `key` in `chain` carries a `when` condition.
    pub fn chain_needs_state(&self, chain: &[&str], key: &str) -> bool {
        chain.iter().any(|&ctx| self.stack(ctx, key).iter().any(|e| !e.when.always()))
    }

    /// Candidates `(context, action)` along `chain` for `key` in `state`, plus whether an
    /// `unbound` mask stopped the chain. Stops after `"text"` when `printable` is true.
    pub fn chain_resolve<'a>(
        &'a self,
        chain: &[&'a str],
        key: &str,
        state: u8,
        printable: bool,
    ) -> (Vec<(&'a str, &'a str)>, bool) {
        let mut out = Vec::new();
        for &ctx in chain {
            for e in self.stack(ctx, key).iter().rev() {
                if e.unbound {
                    return (out, true);
                }
                if e.when.holds(state) {
                    out.push((ctx, e.action.as_str()));
                }
            }
            if printable && ctx == "text" {
                break;
            }
        }
        (out, false)
    }

    /// Candidates `(context, action)` along `chain` for `key` in `state`.
    pub fn chain_candidates<'a>(
        &'a self,
        chain: &[&'a str],
        key: &str,
        state: u8,
        printable: bool,
    ) -> Vec<(&'a str, &'a str)> {
        self.chain_resolve(chain, key, state, printable).0
    }

    /// Every bound key of `context` with its topmost action, sorted by key.
    pub fn bindings(&self, context: &str) -> Vec<(String, String)> {
        let Some(l) = self.maps.get(context) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = l
            .binds
            .iter()
            .filter_map(|(k, st)| st.last().filter(|e| !e.unbound).map(|e| (k.clone(), e.action.clone())))
            .collect();
        out.sort();
        out
    }

    /// Effective `(key, action)` bindings across `chain` (first context wins; masks hide lower ones,
    /// and printable keys stop after `"text"`).
    pub fn bindings_in_chain(&self, chain: &[&str]) -> Vec<(String, String)> {
        let mut seen: HashMap<&str, Option<&str>> = HashMap::new();
        let mut past_text = false;
        for &ctx in chain {
            if let Some(l) = self.maps.get(ctx) {
                for (k, st) in &l.binds {
                    if seen.contains_key(k.as_str()) || (past_text && is_printable_key_name(k.as_str())) {
                        continue;
                    }
                    if let Some(top) = st.last() {
                        seen.insert(k.as_str(), if top.unbound { None } else { Some(top.action.as_str()) });
                    }
                }
            }
            if ctx == "text" {
                past_text = true;
            }
        }
        let mut out: Vec<(String, String)> =
            seen.into_iter().filter_map(|(k, a)| a.map(|act| (k.to_string(), act.to_string()))).collect();
        out.sort();
        out
    }

    /// Every reachable `(context, key, entry)` along `chain` using the 16-state `When` coverage:
    /// disjoint predicates, declining Lua actions (`declines = true`), and non-masked fallbacks
    /// stay visible, while masks and non-declining core actions block the states they cover.
    pub fn reachable_in_chain<'a>(&'a self, chain: &[&'a str]) -> Vec<(&'a str, &'a str, &'a Entry)> {
        let mut blocked: HashMap<&str, u16> = HashMap::new();
        let mut out: Vec<(&'a str, &'a str, &'a Entry)> = Vec::new();
        let mut past_text = false;
        for &ctx in chain {
            if let Some(l) = self.maps.get(ctx) {
                let mut keys: Vec<&String> = l.binds.keys().collect();
                keys.sort();
                for k in keys {
                    if past_text && is_printable_key_name(k.as_str()) {
                        continue;
                    }
                    let stack = &l.binds[k];
                    for e in stack.iter().rev() {
                        let sm = e.when.state_mask();
                        let cov = blocked.get(k.as_str()).copied().unwrap_or(0);
                        if e.unbound {
                            blocked.insert(k.as_str(), cov | sm);
                        } else if (sm & !cov) != 0 {
                            out.push((ctx, k.as_str(), e));
                            if !e.declines {
                                blocked.insert(k.as_str(), cov | sm);
                            }
                        }
                    }
                }
            }
            if ctx == "text" {
                past_text = true;
            }
        }
        out.sort_by(|(ca, ka, ea), (cb, kb, eb)| {
            ka.cmp(kb)
                .then(ca.cmp(cb))
                .then(Reverse(ea.rank).cmp(&Reverse(eb.rank)))
                .then(Reverse(ea.priority).cmp(&Reverse(eb.priority)))
                .then(eb.seq.cmp(&ea.seq))
        });
        out
    }

    /// Keys that would run `action` along `chain`.
    /// Honours masks, unconditional non-declinable shadowing, and the printable rule after `text`.
    /// Ordered by higher source rank first, then higher explicit `priority`, then earlier
    /// registration order (`seq` ascending).
    pub fn hint(&self, action: &str, chain: &[&str]) -> Vec<&str> {
        let mut blocked: HashSet<&str> = HashSet::new();
        let mut found: HashMap<&str, (u8, Priority, u64)> = HashMap::new();
        let mut past_text = false;
        for &ctx in chain {
            if let Some(l) = self.maps.get(ctx) {
                for (key, stack) in &l.binds {
                    let k = key.as_str();
                    if blocked.contains(k) || (past_text && is_printable_key_name(k)) {
                        continue;
                    }
                    for e in stack.iter().rev() {
                        if e.unbound {
                            blocked.insert(k);
                            break;
                        }
                        if e.action == action {
                            found.entry(k).or_insert((e.rank, e.priority, e.seq));
                            if e.when.always() && !e.declines {
                                blocked.insert(k);
                            }
                            break;
                        }
                        if e.when.always() && !e.declines {
                            blocked.insert(k);
                            break;
                        }
                    }
                }
            }
            if ctx == "text" {
                past_text = true;
            }
        }
        let mut items: Vec<(&str, (u8, Priority, u64))> = found.into_iter().collect();
        items.sort_by(|(ka, (ra, pa, sa)), (kb, (rb, pb, sb))| {
            Reverse(*ra).cmp(&Reverse(*rb)).then(Reverse(*pa).cmp(&Reverse(*pb))).then(sa.cmp(sb)).then(ka.cmp(kb))
        });
        items.into_iter().map(|(k, _)| k).collect()
    }

    /// Compute [`Self::hint`] for every reachable action along `chain` in a single pass.
    pub fn hints_for_chain(&self, chain: &[&str]) -> HashMap<&str, Vec<&str>> {
        let mut blocked: HashSet<&str> = HashSet::new();
        let mut by_action: HashMap<&str, HashMap<&str, (u8, Priority, u64)>> = HashMap::new();
        let mut past_text = false;
        for &ctx in chain {
            if let Some(l) = self.maps.get(ctx) {
                for (key, stack) in &l.binds {
                    let k = key.as_str();
                    if blocked.contains(k) || (past_text && is_printable_key_name(k)) {
                        continue;
                    }
                    for e in stack.iter().rev() {
                        if e.unbound {
                            blocked.insert(k);
                            break;
                        }
                        by_action.entry(e.action.as_str()).or_default().entry(k).or_insert((e.rank, e.priority, e.seq));
                        if e.when.always() && !e.declines {
                            blocked.insert(k);
                            break;
                        }
                    }
                }
            }
            if ctx == "text" {
                past_text = true;
            }
        }
        by_action
            .into_iter()
            .map(|(action, found)| {
                let mut items: Vec<(&str, (u8, Priority, u64))> = found.into_iter().collect();
                items.sort_by(|(ka, (ra, pa, sa)), (kb, (rb, pb, sb))| {
                    Reverse(*ra)
                        .cmp(&Reverse(*rb))
                        .then(Reverse(*pa).cmp(&Reverse(*pb)))
                        .then(sa.cmp(sb))
                        .then(ka.cmp(kb))
                });
                (action, items.into_iter().map(|(k, _)| k).collect())
            })
            .collect()
    }

    /// Essential core actions (`CoreAction::essential`) left without any key in their context.
    pub fn missing_essential(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        for a in CORE_ACTIONS.iter().filter(|a| a.essential) {
            for &ctx in a.contexts {
                if let Ok(chain) = self.chain_for(ctx)
                    && self.hint(a.name, &chain).is_empty()
                {
                    out.push((a.name, ctx));
                }
            }
        }
        out
    }

    /// Essential accept/cancel modal actions that currently have no triggering key
    /// (: unbinding or disabling `keymap-default` leaves them unbound and
    /// `/hotkeys` + `mare doctor` diagnose them).
    pub fn unbound_essential_actions(&self) -> Vec<(&'static str, &'static str)> {
        const ESSENTIAL: &[(&str, &str)] = &[
            ("select", "select.accept"),
            ("select", "select.back"),
            ("confirm", "confirm.accept"),
            ("confirm", "confirm.cancel"),
            ("help", "help.accept"),
            ("help", "help.cancel"),
        ];
        ESSENTIAL.iter().copied().filter(|&(ctx, action)| self.hint(action, &[ctx]).is_empty()).collect()
    }
}

/// Whether a normalized key string (`"ctrl+c"`) is reserved by the core and cannot be bound.
pub fn is_reserved(normalized_key: &str) -> bool {
    normalized_key == "ctrl+c"
}

/// A canonical key (`ctrl+t`, `alt+left`, `esc`) as a hint shows it — `Ctrl+T`, `Alt+←`, `Esc`:
/// modifiers and named keys capitalised, a single letter upper-cased, arrows as glyphs. The same
/// rules as the `/keys` list (`commands/keys.lua` `M.pretty`), so every label built from
/// `hint` reads alike.
pub fn key_label(key: &str) -> String {
    let parts: Vec<&str> = key.split('+').collect();
    let last = parts.len().saturating_sub(1);
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| match *p {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "super" => "Super".to_string(),
            "esc" => "Esc".to_string(),
            "enter" => "Enter".to_string(),
            "tab" => "Tab".to_string(),
            "space" => "Space".to_string(),
            "backspace" => "Backspace".to_string(),
            "pageup" => "PageUp".to_string(),
            "pagedown" => "PageDown".to_string(),
            "home" => "Home".to_string(),
            "end" => "End".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            p if i == last && p.chars().count() == 1 => p.to_uppercase(),
            p => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(action: &str, seq: u64) -> Entry {
        Entry {
            action: action.into(),
            unbound: false,
            when: When::default(),
            declines: false,
            rank: 1,
            priority: Priority::default(),
            seq,
        }
    }

    fn r(action: &str, rank: u8, seq: u64) -> Entry {
        Entry { rank, ..e(action, seq) }
    }

    fn w(action: &str, seq: u64, when: &[&str]) -> Entry {
        Entry {
            action: action.into(),
            unbound: false,
            when: When::parse(when.iter().copied()).unwrap(),
            declines: false,
            rank: 1,
            priority: Priority::default(),
            seq,
        }
    }

    #[test]
    fn sync_from_updates_existing_context_parents_before_queued_commands() {
        let mut local = Keymap::new();
        local.set_context("editor.custom".into(), Some("editor".into()), false, true);
        local.set("editor.custom", "x".into(), vec![e("stale", 1)]);
        let mut shared = Keymap::new();
        shared.set_context("editor.custom".into(), Some("input".into()), true, true);
        shared.set("editor.custom", "x".into(), vec![e("custom", 1)]);
        shared.set("editor.custom", "b".into(), vec![e("fresh", 2)]);

        assert_eq!(local.chain_for("editor.custom").unwrap(), ["editor.custom", "editor", "text", "global"]);
        local.sync_from(&shared);
        assert_eq!(local.chain_for("editor.custom").unwrap(), ["editor.custom", "input", "text"]);
        assert_eq!(local.active_subcontexts_for("input"), ["editor.custom"]);
        assert!(local.active_subcontexts_for("editor").is_empty());
        assert!(local.should_swallow_printable(&local.chain_for("editor.custom").unwrap()));
        assert_eq!(local.stack("editor.custom", "x")[0].action, "custom", "shared bindings replace stale copy");
        assert_eq!(local.stack("editor.custom", "b")[0].action, "fresh", "newly added bindings sync immediately");
    }

    #[test]
    fn a_key_in_two_contexts_is_two_bindings() {
        let mut km = Keymap::default();
        km.set("editor", "q".into(), vec![e("quit", 1)]);
        km.set("transcript", "q".into(), vec![e("view.close", 2)]);
        assert_eq!(km.chain_candidates(&["editor", "text", "global"], "q", 0, false), [("editor", "quit")]);
        assert_eq!(km.chain_candidates(&["transcript", "global"], "q", 0, false), [("transcript", "view.close")]);
        km.set("transcript", "q".into(), vec![]);
        assert!(km.stack("transcript", "q").is_empty());
        assert_eq!(km.bindings("editor"), [("q".to_string(), "quit".to_string())]);
    }

    #[test]
    fn prefixes_are_per_context_and_follow_unbinding() {
        let ed = ["editor", "text", "global"];
        let tr = ["transcript", "global"];
        for (ctx, chord, prefix, action, own_chain, other_chain) in [
            ("editor", "ctrl+x g", "ctrl+x", "git", &ed[..], &tr[..]),
            ("transcript", "g g", "g", "view.top", &tr[..], &ed[..]),
        ] {
            let mut km = Keymap::default();
            km.set(ctx, chord.into(), vec![e(action, 1)]);
            assert!(km.is_prefix(own_chain, prefix), "{ctx} {chord}");
            assert!(!km.is_prefix(other_chain, prefix), "{ctx} {chord}");
            km.set(ctx, chord.into(), vec![]);
            assert!(!km.is_prefix(own_chain, prefix), "{ctx} {chord} after unbind");
        }
    }

    #[test]
    fn an_unbound_entry_stops_candidates_hint_and_bindings() {
        let mut km = Keymap::default();
        let mask = Entry { unbound: true, ..e("", 2) };
        km.set("editor", "ctrl+t".into(), vec![e("transcript", 1), mask.clone()]);
        km.set("global", "ctrl+t".into(), vec![e("transcript", 4)]);
        let ed = ["editor", "text", "global"];
        assert!(km.chain_candidates(&ed, "ctrl+t", 0, false).is_empty());
        assert!(km.hint("transcript", &ed).is_empty());
        assert!(km.bindings("editor").is_empty());

        let above = Entry { declines: true, ..e("optional", 3) };
        km.set("editor", "ctrl+t".into(), vec![e("transcript", 1), mask, above]);
        assert_eq!(km.chain_candidates(&ed, "ctrl+t", 0, false), [("editor", "optional")]);
        assert!(km.hint("transcript", &ed).is_empty());
        assert_eq!(km.hint("optional", &ed), ["ctrl+t"]);
        assert_eq!(km.bindings("editor"), [("ctrl+t".into(), "optional".into())]);
    }

    #[test]
    fn a_masked_sequence_is_no_prefix() {
        let mut km = Keymap::default();
        let mask = Entry { unbound: true, ..e("", 2) };
        let ed = ["editor", "text", "global"];
        km.set("editor", "ctrl+x g".into(), vec![e("git", 1), mask]);
        assert!(!km.is_prefix(&ed, "ctrl+x"));
        km.set("editor", "ctrl+x h".into(), vec![e("help", 3)]);
        assert!(km.is_prefix(&ed, "ctrl+x"));
        km.set("editor", "ctrl+x h".into(), vec![]);
        assert!(!km.is_prefix(&ed, "ctrl+x"));
        km.set("editor", "ctrl+x g".into(), vec![e("git", 1)]);
        assert!(km.is_prefix(&ed, "ctrl+x"));
    }

    #[test]
    fn a_failing_condition_falls_through_to_the_binding_below() {
        let mut km = Keymap::default();
        let stack = vec![e("user", 1), w("keymap-help", 2, &["editor_empty", "!completion_open"])];
        km.set("editor", "?".into(), stack);
        let ed = ["editor", "text", "global"];
        let try_ = |state| km.chain_candidates(&ed, "?", state, false).into_iter().map(|(_, a)| a).collect::<Vec<_>>();
        assert_eq!(try_(EDITOR_EMPTY), ["keymap-help", "user"]);
        assert_eq!(try_(EDITOR_EMPTY | COMPLETION_OPEN), ["user"], "a negated predicate");
        assert_eq!(try_(0), ["user"], "a missing predicate");
    }

    #[test]
    fn key_labels_read_like_the_keys_list() {
        assert_eq!(key_label("ctrl+t"), "Ctrl+T");
        assert_eq!(key_label("alt+left"), "Alt+←");
        assert_eq!(key_label("esc"), "Esc");
        assert_eq!(key_label("q"), "Q");
    }

    #[test]
    fn hint_skips_shadowed_bindings_and_orders_rank_then_binding_order() {
        let mut km = Keymap::default();
        km.set("editor", "ctrl+b".into(), vec![e("background", 1)]);
        km.set("editor", "ctrl+j".into(), vec![e("background", 5)]);
        km.set("editor", "left".into(), vec![e("background", 3), w("agents", 4, &["editor_empty"])]);
        km.set("editor", "ctrl+k".into(), vec![e("background", 2), e("kill", 6)]);
        km.set("global", "f8".into(), vec![r("background", 2, 7)]);
        let ed = km.chain_for("editor").unwrap();
        let tr = km.chain_for("transcript").unwrap();
        assert_eq!(km.hint("background", &ed), ["f8", "ctrl+b", "left", "ctrl+j"]);
        assert_eq!(km.hint("background", &tr), ["f8"]);
    }

    #[test]
    fn printable_rule_and_bindings_in_chain_skip_printable_global_keys() {
        assert!(is_printable_key_name("q"));
        assert!(is_printable_key_name("shift+a"));
        assert!(!is_printable_key_name("super+x"));
        assert!(is_printable_key_name("space"));
        assert!(!is_printable_key_name("ctrl+x"));
        assert!(!is_printable_key_name("alt+x"));
        assert!(!is_printable_key_name("g g"));

        let mut km = Keymap::default();
        km.set("global", "q".into(), vec![e("g_q", 1)]);
        km.set("global", "super+x".into(), vec![e("g_sx", 2)]);
        km.set("global", "f5".into(), vec![e("redraw", 3)]);
        let ed = km.chain_for("editor").unwrap();
        assert_eq!(
            km.bindings_in_chain(&ed),
            [("f5".to_string(), "redraw".to_string()), ("super+x".to_string(), "g_sx".to_string())]
        );
        assert_eq!(km.hint("g_sx", &ed), ["super+x"]);

        let mask = Entry { unbound: true, ..e("", 4) };
        km.set("transcript.search", "j".into(), vec![mask]);
        let (cands, masked) = km.chain_resolve(&["transcript.search"], "j", 0, false);
        assert!(cands.is_empty() && masked);
    }

    #[test]
    fn global_falls_back_through_chain_and_printable_stops_after_text() {
        let mut km = Keymap::default();
        let ed = ["editor", "text", "global"];
        km.set("global", "f5".into(), vec![e("g_f5", 1)]);
        km.set("global", "q".into(), vec![e("g_q", 2)]);
        assert_eq!(km.chain_candidates(&ed, "f5", 0, false), [("global", "g_f5")]);
        // Printable char stops after `text`, so `global` is not consulted.
        assert!(km.chain_candidates(&ed, "q", 0, true).is_empty());
        // A binding in `text` shadows `global` and still runs on printable keys.
        km.set("text", "f5".into(), vec![e("t_f5", 3)]);
        assert_eq!(km.chain_candidates(&ed, "f5", 0, false), [("text", "t_f5"), ("global", "g_f5")]);
        km.set("text", "q".into(), vec![e("t_q", 4)]);
        assert_eq!(km.chain_candidates(&ed, "q", 0, true), [("text", "t_q")]);
    }

    #[test]
    fn a_declinable_lua_action_above_does_not_shadow_the_binding_below() {
        let mut km = Keymap::default();
        let lua = Entry { declines: true, ..e("agents.back", 2) };
        km.set("transcript", "esc".into(), vec![e("view.back", 1), lua]);
        let tr = ["transcript", "global"];
        assert_eq!(km.hint("view.back", &tr), ["esc"]);
        km.set("transcript", "esc".into(), vec![e("view.back", 1), e("view.close", 3)]);
        assert!(km.hint("view.back", &tr).is_empty());
    }

    #[test]
    fn when_and_contexts_reject_unknown_or_wrong_targets() {
        assert_eq!(When::parse(["working", "!paste_burst"]), Ok(When { all: WORKING, none: PASTE_BURST }));
        assert!(When::parse(["typing"]).unwrap_err().contains("editor_empty"), "the error lists the known ones");
        let mut km = Keymap::default();
        assert!(km.check_bindable("n").unwrap_err().contains("unknown context \"n\""));
        assert!(km.check_bindable("input").is_ok());
        assert!(km.check_binding_target("select", "g g", Some("select.top")).is_ok());
        assert!(km.check_binding_target("select.vim", "j", Some("select.down")).is_ok());
        assert!(km.check_binding_target("global", "g g", None).is_err());
        assert!(km.check_binding_target("transcript", "ctrl+x", Some("steer")).unwrap_err().contains("not available"));
        assert!(km.check_binding_target("global", "ctrl+x", Some("steer")).is_err());
        km.register_context("editor.vi".into(), "editor".into()).unwrap();
        assert!(km.check_binding_target("editor.vi", "x", Some("steer")).is_ok());
        km.register_context("editor.vi.op".into(), "editor.vi".into()).unwrap();
        km.set("editor.vi.op", "d".into(), vec![e("redraw", 1)]);
        let removed = km.remove_context("editor.vi");
        assert_eq!(removed, ["editor.vi", "editor.vi.op"]);
        assert!(km.check_bindable("editor.vi").is_err());
        assert!(km.check_bindable("editor.vi.op").is_err());

        // `input` resolves through `input -> text`, allows text.* and input.*, rejects editor.* & sequences.
        assert_eq!(km.chain_for("input").unwrap(), ["input", "text"]);
        assert!(km.check_binding_target("input", "ctrl+l", Some("text.line_end")).is_ok());
        assert!(km.check_binding_target("input", "enter", Some("input.accept")).is_ok());
        assert!(km.check_binding_target("input", "ctrl+x", Some("steer")).unwrap_err().contains("not available"));
        assert!(km.check_binding_target("input", "g g", None).is_err());

        let missing = km.missing_essential();
        assert!(missing.contains(&("editor.submit", "editor")));
        assert!(missing.contains(&("input.accept", "input")));
        assert!(missing.contains(&("input.cancel", "input")));
        km.set("editor", "enter".into(), vec![e("editor.submit", 1)]);
        km.set("input", "enter".into(), vec![e("input.accept", 1)]);
        km.set("input", "esc".into(), vec![e("input.cancel", 1)]);
        assert!(km.missing_essential().is_empty());
    }

    #[test]
    fn priority_orders_above_seq_within_same_rank_and_when_overlaps() {
        let mut p1 = e("first", 1);
        p1.priority = Priority::new(5.0).unwrap();
        let mut p2 = e("second", 2);
        p2.priority = Priority::new(10.0).unwrap();
        let p0 = e("low", 3);
        let mut km = Keymap::default();
        km.set("editor", "ctrl+a".into(), vec![p0, p2.clone(), p1.clone()]);
        assert_eq!(km.hint("first", &["editor"]), ["ctrl+a"]);

        // Two keys bound to the same action within the same rank: higher priority comes first.
        let mut ka = r("my.act", 2, 1);
        ka.priority = Priority::new(1.0).unwrap();
        let mut kb = r("my.act", 2, 2);
        kb.priority = Priority::new(10.0).unwrap();
        km.set("editor", "ctrl+x".into(), vec![ka]);
        km.set("editor", "ctrl+y".into(), vec![kb]);
        assert_eq!(km.hint("my.act", &["editor"]), ["ctrl+y", "ctrl+x"]);

        // Higher rank always wins over higher priority.
        let mut k_user = r("my.act", 3, 10);
        k_user.priority = Priority::new(-5.0).unwrap();
        km.set("editor", "ctrl+z".into(), vec![k_user]);
        assert_eq!(km.hint("my.act", &["editor"]), ["ctrl+z", "ctrl+y", "ctrl+x"]);

        assert!(Priority::new(f64::NAN).is_err());
        assert!(Priority::new(f64::INFINITY).is_err());

        let w_work = When::parse(["working"]).unwrap();
        let w_idle = When::parse(["!working"]).unwrap();
        let w_empty = When::parse(["editor_empty"]).unwrap();
        assert!(!w_work.overlaps(w_idle));
        assert!(w_work.overlaps(w_empty));
        assert!(When::default().overlaps(w_work));
    }

    #[test]
    fn prefix_respects_when_state_and_reachable_in_chain_handles_disjoint_and_declines() {
        let mut km = Keymap::default();
        let ed = ["editor", "text", "global"];
        km.set("editor", "f6".into(), vec![w("steer", 1, &["working"])]);
        km.set("editor", "f6 g".into(), vec![w("transcript", 2, &["!working"])]);
        assert!(!km.is_prefix_for_state(&ed, "f6", WORKING));
        assert!(km.is_prefix_for_state(&ed, "f6", 0));

        km.set("editor", "f9".into(), vec![w("a", 1, &["!working"]), w("b", 2, &["working"])]);
        km.set("transcript", "esc".into(), vec![e("view.back", 1), Entry { declines: true, ..e("agents.back", 2) }]);
        let r_ed = km.reachable_in_chain(&ed);
        let f9: Vec<_> = r_ed.iter().filter(|(_, k, _)| *k == "f9").map(|(_, _, e)| e.action.as_str()).collect();
        assert_eq!(f9, ["b", "a"]);

        let tr = ["transcript", "global"];
        let r_tr = km.reachable_in_chain(&tr);
        let esc: Vec<_> = r_tr.iter().filter(|(_, k, _)| *k == "esc").map(|(_, _, e)| e.action.as_str()).collect();
        assert_eq!(esc, ["agents.back", "view.back"]);

        // P1.3: child mask on `f5 g` blocks parent `f5 g` from entering prefix mode.
        km.register_context_with("editor.sub".into(), "editor".into(), false, true).unwrap();
        km.set("editor", "f5 g".into(), vec![e("redraw", 1)]);
        assert!(km.is_prefix_for_state(&["editor.sub", "editor"], "f5", 0));
        km.set("editor.sub", "f5 g".into(), vec![Entry { unbound: true, ..e("", 2) }]);
        assert!(!km.is_prefix_for_state(&["editor.sub", "editor"], "f5", 0));

        // P1.7: declining core action (`quit-if-empty`) leaves `text.delete_forward` reachable; `When::labels()` formats labels.
        km.set("editor", "ctrl+d".into(), vec![Entry { declines: true, ..e("quit-if-empty", 1) }]);
        km.set("text", "ctrl+d".into(), vec![e("text.delete_forward", 1)]);
        let r_ed_cd = km.reachable_in_chain(&ed);
        let cd: Vec<_> = r_ed_cd.iter().filter(|(_, k, _)| *k == "ctrl+d").map(|(_, _, e)| e.action.as_str()).collect();
        assert_eq!(cd, ["quit-if-empty", "text.delete_forward"]);
        assert_eq!(When::parse(["working", "!completion_open"]).unwrap().labels(), ["working", "!completion_open"]);
    }

    #[test]
    fn hints_for_chain_matches_single_action_hint_across_masks_declines_and_priority() {
        let mut km = Keymap::default();
        let chain = ["editor", "text", "global"];

        let mut p_low = r("act.a", 2, 1);
        p_low.priority = Priority::new(1.0).unwrap();
        let mut p_high = r("act.a", 2, 2);
        p_high.priority = Priority::new(10.0).unwrap();
        let mut p_user = r("act.a", 3, 5);
        p_user.priority = Priority::new(-2.0).unwrap();

        km.set("editor", "ctrl+a".into(), vec![p_low]);
        km.set("editor", "ctrl+b".into(), vec![p_high]);
        km.set("global", "ctrl+c".into(), vec![p_user]);
        // Conditional + declining stack shadowing an unconditional binding below.
        km.set(
            "editor",
            "ctrl+d".into(),
            vec![
                e("act.shadowed", 1),
                e("act.blocker", 2),
                Entry { declines: true, ..e("act.declines", 3) },
                w("act.cond", 4, &["working"]),
            ],
        );
        km.set("text", "ctrl+d".into(), vec![e("act.lower_blocked", 5)]);
        // Unbound mask blocks lower contexts.
        km.set("editor", "ctrl+e".into(), vec![Entry { unbound: true, ..e("", 6) }]);
        km.set("global", "ctrl+e".into(), vec![e("act.masked", 7)]);
        // Printable key in global is blocked past `text`.
        km.set("global", "?".into(), vec![e("act.printable_global", 8)]);
        km.set("editor", "?".into(), vec![w("act.help", 9, &["editor_empty"])]);

        let all = km.hints_for_chain(&chain);
        for action in [
            "act.a",
            "act.shadowed",
            "act.blocker",
            "act.declines",
            "act.cond",
            "act.lower_blocked",
            "act.masked",
            "act.printable_global",
            "act.help",
            "act.missing",
        ] {
            let expected = km.hint(action, &chain);
            let actual = all.get(action).cloned().unwrap_or_default();
            assert_eq!(actual, expected, "mismatch for action `{action}`");
        }
    }
}
