//! The three-level sandbox ladder.

use mare_shared::lua::rt_err;

/// Ordered: a wider level compares greater.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    ReadOnly,
    WorkspaceWrite,
    /// `danger-full-access`: no sandbox
    FullAccess,
}

impl Level {
    pub fn parse(s: &str) -> mlua::Result<Self> {
        Ok(match s {
            "read-only" => Self::ReadOnly,
            "workspace-write" => Self::WorkspaceWrite,
            "danger-full-access" | "full-access" => Self::FullAccess,
            _ => {
                return Err(rt_err(format!(
                    "sandbox: unknown level `{s}` (read-only|workspace-write|danger-full-access)"
                )));
            }
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "danger-full-access",
        }
    }
    /// The model-facing denial marker, identical for bash and file tools.
    pub fn marker(self) -> String {
        format!("[sandbox: file access denied under {} mode]", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parse_and_as_str_cover_all_variants_and_reject_unknown() {
        assert_eq!(Level::parse("read-only").unwrap(), Level::ReadOnly);
        assert_eq!(Level::parse("workspace-write").unwrap(), Level::WorkspaceWrite);
        assert_eq!(Level::parse("danger-full-access").unwrap(), Level::FullAccess);
        assert_eq!(Level::parse("full-access").unwrap(), Level::FullAccess);
        assert_eq!(Level::FullAccess.as_str(), "danger-full-access");
        let err = Level::parse("bogus").unwrap_err().to_string();
        assert!(err.contains("unknown level"), "{err}");
    }
}
