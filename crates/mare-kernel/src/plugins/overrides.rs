//! The rule for a plugin definition that shares its name with an earlier (lower-precedence)
//! one: replacing it needs an explicit `override = true` in the
//! definition. Without it the definition is refused and the earlier one stays.

use crate::{Origin, PluginDef, PluginSource};

/// A definition that was not loaded because it would silently replace another one.
#[derive(Clone, Debug)]
pub struct OverrideRefusal {
    /// the shared plugin name
    pub name: String,
    /// where the refused definition came from
    pub source: PluginSource,
    /// where the definition that stays came from
    pub kept: Origin,
    /// a same-name definition from the same source: both stay out
    pub twin: Option<PluginSource>,
}

impl OverrideRefusal {
    /// A definition named `preset`: a reserved field of the `plugins` table.
    pub fn reserved(source: PluginSource) -> Self {
        Self { name: "preset".into(), kept: source.origin, source, twin: None }
    }

    /// The error shown like any other plugin failure: what collided and the two fixes.
    pub fn message(&self) -> String {
        let n = &self.name;
        let place = self.source.path.as_ref().map(|p| format!(" ({})", p.display())).unwrap_or_default();
        if self.name == "preset" && self.twin.is_none() && self.kept == self.source.origin {
            return format!(
                "plugin name `preset` is reserved{place}: `plugins.preset` selects a plugin-set preset; rename the plugin"
            );
        }
        if let Some(twin) = &self.twin {
            let other = twin.path.as_ref().map(|p| format!(" and {}", p.display())).unwrap_or_default();
            return format!(
                "plugin `{n}` is defined twice in one {} source{place}{other}: neither was loaded; remove or \
                 rename one of them",
                self.source.origin.as_str()
            );
        }
        let (kept, fix2) = match self.kept {
            Origin::Builtin => (format!("the built-in `{n}`"), format!("rename your plugin `{n}`")),
            o => (
                format!("the {} plugin `{n}`", o.as_str()),
                format!("remove or rename the {} plugin `{n}`", o.as_str()),
            ),
        };
        format!(
            "plugin `{n}` ({}){place} was not loaded: its name collides with {kept}; add `override = true` to its \
             definition to replace it, or {fix2}",
            self.source.origin.as_str()
        )
    }
}

/// Add `incoming` (in precedence order) to `out`. A same-name definition from a higher origin
/// replaces the earlier one only when it declares `override = true`; otherwise it is refused.
/// Same-name definitions from one origin (two package directories, `x.lua` and `x/init.lua`)
/// are an error: none of them is loaded and one refusal names both files; a
/// lower-origin definition of the name stays. Returns the names that replaced a definition and
/// the refusals.
pub(crate) fn merge(out: &mut Vec<PluginDef>, incoming: Vec<PluginDef>) -> (Vec<String>, Vec<OverrideRefusal>) {
    let mut replaced = vec![];
    let mut refused = vec![];
    let twin = |d: &PluginDef, all: &[PluginDef]| {
        all.iter().any(|x| !std::ptr::eq(x, d) && x.name == d.name && x.source.origin == d.source.origin)
    };
    let (twins, incoming): (Vec<_>, Vec<_>) = {
        let flags: Vec<bool> = incoming.iter().map(|d| twin(d, &incoming)).collect();
        let mut flags = flags.into_iter();
        incoming.into_iter().partition(|_| flags.next().unwrap_or(false))
    };
    let mut reported: Vec<(String, Origin)> = vec![];
    for d in &twins {
        if reported.contains(&(d.name.clone(), d.source.origin)) {
            continue;
        }
        reported.push((d.name.clone(), d.source.origin));
        let other =
            twins.iter().find(|x| x.name == d.name && x.source.origin == d.source.origin && !std::ptr::eq(*x, d));
        refused.push(OverrideRefusal {
            name: d.name.clone(),
            source: d.source.clone(),
            kept: d.source.origin,
            twin: other.map(|o| o.source.clone()),
        });
    }
    for mut d in incoming {
        match out.iter().position(|x| x.name == d.name) {
            Some(pos) => {
                let kept = out[pos].source.origin;
                if d.source.origin > kept {
                    if !d.overrides {
                        refused.push(OverrideRefusal {
                            name: d.name.clone(),
                            source: d.source.clone(),
                            kept,
                            twin: None,
                        });
                        continue;
                    }
                    d.replaced = Some(kept);
                } else {
                    d.replaced = out[pos].replaced;
                }
                // the replacement takes over the services the replaced definition was
                // responsible for
                d.replaced_provide = out[pos].services();
                replaced.push(d.name.clone());
                out[pos] = d;
            }
            None => out.push(d),
        }
    }
    (replaced, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_refusal_formats_non_builtin_and_same_origin_twin_messages() {
        let user_src = PluginSource {
            name_hint: "demo".into(),
            origin: Origin::User,
            path: Some(std::path::PathBuf::from("/u/a.lua")),
            code: None,
        };
        let proj_src = PluginSource {
            name_hint: "demo".into(),
            origin: Origin::Project,
            path: Some(std::path::PathBuf::from("/p/a.lua")),
            code: None,
        };
        let non_builtin = OverrideRefusal { name: "demo".into(), source: proj_src, kept: Origin::User, twin: None };
        let msg = non_builtin.message();
        assert!(msg.contains("user plugin `demo`"), "{msg}");
        assert!(msg.contains("remove or rename"), "{msg}");

        let twin_src = PluginSource {
            name_hint: "demo".into(),
            origin: Origin::User,
            path: Some(std::path::PathBuf::from("/u/a/init.lua")),
            code: None,
        };
        let twin = OverrideRefusal { name: "demo".into(), source: user_src, kept: Origin::User, twin: Some(twin_src) };
        let twin_msg = twin.message();
        assert!(twin_msg.contains("defined twice in one user source"), "{twin_msg}");
    }
}
