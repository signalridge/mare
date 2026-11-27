//! The plugin definitions a configuration asks for.

use mare_kernel::Kernel;
use mare_plugin::loader;

/// The plugin definitions this config asks for: every discovered source whose name slot is on
/// . A slot is one plugin name across every
/// source; `plugins.<name>.enabled = false` closes all of them (a deliberate disable, never a
/// failure), and replacing a definition always takes `override = true` (resolved by the kernel).
/// Read errors go to `on_error` with the services of the same-name definition the unreadable one
/// would have replaced (read this time, a closed slot included). Same-source duplicates and a
/// definition named `preset` (a reserved field of the `plugins` table) are refusals, returned
/// apart: they are decided here, never "a definition that no longer reads".
pub(crate) fn wanted_defs(
    kernel: &Kernel,
    config: &serde_json::Value,
    cwd: &std::path::Path,
    trusted: bool,
    no_user: bool,
    on_error: &mut dyn FnMut(&mare_kernel::PluginSource, String, &[String]),
) -> (Vec<mare_kernel::PluginDef>, Vec<mare_kernel::OverrideRefusal>) {
    let mut defs = Vec::new();
    let mut errors = Vec::new();
    for s in loader::discover(config, cwd, trusted, no_user) {
        match kernel.load_def(&s) {
            Ok(d) => defs.push(d),
            Err(e) => errors.push((s, e.to_string())),
        }
    }
    // An unreadable file whose definition is running stays as that running definition (a failed
    // read never unloads what runs). A new same-name definition from the same source is then its
    // twin: both are refused below, before the slot is decided, so the new
    // one never keeps a built-in's slot open as a "replacement" that ends up refused.
    let mut kept_twins: Vec<mare_kernel::PluginSource> = vec![];
    for (s, e) in errors {
        let slot = defs.iter().find(|d| d.name == s.name_hint).map(|d| d.services()).unwrap_or_default();
        if kernel.def(&s.name_hint).is_some_and(|run| run.source.origin == s.origin && run.source.path == s.path) {
            kept_twins.push(s.clone());
        }
        on_error(&s, e, &slot);
    }
    // what each one declares stays known while it is turned off or refused (`sources`)
    let mut declared: Vec<mare_kernel::Declared> = defs.iter().map(mare_kernel::PluginDef::declared).collect();
    let mut refusals = vec![];
    // Same-name definitions from one source are an error found while reading: none
    // of them loads, whatever the slot or the preset decides, and one error names both files.
    // Taken out first, so a refused definition never stands for the slot below.
    let same_source = |d: &mare_kernel::PluginDef, all: &[mare_kernel::PluginDef]| {
        all.iter().filter(|x| x.name == d.name && x.source.origin == d.source.origin).count() > 1
            || kept_twins
                .iter()
                .any(|k| k.name_hint == d.name && k.origin == d.source.origin && k.path != d.source.path)
    };
    let twins: Vec<bool> = defs.iter().map(|d| same_source(d, &defs)).collect();
    let mut twins = twins.into_iter();
    let (twins, defs): (Vec<_>, Vec<_>) = defs.into_iter().partition(|_| twins.next().unwrap_or(false));
    let mut reported: Vec<(String, mare_kernel::Origin)> = vec![];
    for d in &twins {
        if reported.contains(&(d.name.clone(), d.source.origin)) {
            continue;
        }
        reported.push((d.name.clone(), d.source.origin));
        let other = twins
            .iter()
            .find(|x| x.name == d.name && x.source.origin == d.source.origin && !std::ptr::eq(*x, d))
            .map(|o| o.source.clone())
            .or_else(|| kept_twins.iter().find(|k| k.name_hint == d.name && k.origin == d.source.origin).cloned());
        let refusal = mare_kernel::OverrideRefusal {
            name: d.name.clone(),
            source: d.source.clone(),
            kept: d.source.origin,
            twin: other,
        };
        refusals.push(refusal);
    }
    // The effective definition of a slot is a built-in unless a higher source replaces it with
    // `override = true`; a preset only ever decides built-in slots.
    let replaced: std::collections::HashSet<String> = defs
        .iter()
        .filter(|d| d.source.origin != mare_kernel::Origin::Builtin && d.overrides)
        .map(|d| d.name.clone())
        .collect();
    let builtin_tags = |name: &str| -> Vec<String> {
        defs.iter()
            .find(|d| d.name == name && d.source.origin == mare_kernel::Origin::Builtin)
            .map(|d| d.tags.clone())
            .unwrap_or_default()
    };
    let open: std::collections::HashSet<String> = defs
        .iter()
        .map(|d| d.name.clone())
        .filter(|name| {
            let builtin = !replaced.contains(name)
                && defs.iter().any(|d| &d.name == name && d.source.origin == mare_kernel::Origin::Builtin);
            loader::slot_enabled(name, builtin, &builtin_tags(name), config)
        })
        .collect();
    let (reserved, defs): (Vec<_>, Vec<_>) = defs.into_iter().partition(|d| d.name == "preset");
    for d in reserved {
        refusals.push(mare_kernel::OverrideRefusal::reserved(d.source.clone()));
    }
    // the same-name rule's pick per name (as the kernel merges: in precedence order, a higher
    // source replaces only with `override = true`), for queries about a name that does not run
    // (`declaring`, `status(..).tags`)
    let mut picks: Vec<&mare_kernel::PluginDef> = vec![];
    for d in &defs {
        match picks.iter_mut().find(|p| p.name == d.name) {
            Some(p) if d.source.origin > p.source.origin && d.overrides => *p = d,
            Some(_) => {}
            None => picks.push(d),
        }
    }
    for d in picks {
        if let Some(entry) = declared
            .iter_mut()
            .find(|e| e.name == d.name && e.origin == d.source.origin.as_str() && e.path == d.source.path)
        {
            entry.effective = true;
        }
    }
    kernel.set_declared(declared);
    (defs.into_iter().filter(|d| open.contains(&d.name)).collect(), refusals)
}
