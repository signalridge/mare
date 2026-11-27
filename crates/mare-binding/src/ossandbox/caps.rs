//! Capability dimensions and the `MARE_SANDBOX_CAPS` narrowing.

/// One capability dimension of a sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cap {
    /// reads restricted to allowed roots
    FsRead,
    /// explicit paths hidden from reads even when their parent is readable
    FsDenyRead,
    /// writes restricted to allowed roots
    FsWrite,
    /// protected workspace metadata remains unwritable by confined processes
    FsProtect,
    /// host runtime secret directories hidden from confined processes
    FsHostProtect,
    /// network fully blocked by the kernel
    NetBlock,
    /// outbound connections only to mare's local proxy, enforced by the kernel
    NetProxy,
    /// Unix sockets restricted (only explicitly listed ones reachable)
    UnixSocket,
}

impl Cap {
    pub const ALL: [Self; 8] = [
        Self::FsRead,
        Self::FsDenyRead,
        Self::FsWrite,
        Self::FsProtect,
        Self::FsHostProtect,
        Self::NetBlock,
        Self::NetProxy,
        Self::UnixSocket,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::FsRead => "fs_read",
            Self::FsDenyRead => "fs_deny_read",
            Self::FsWrite => "fs_write",
            Self::FsProtect => "fs_protect",
            Self::FsHostProtect => "fs_host_protect",
            Self::NetBlock => "net_block",
            Self::NetProxy => "net_proxy",
            Self::UnixSocket => "unix_socket",
        }
    }

    pub(super) fn why(self) -> &'static str {
        match self {
            Self::FsRead => "reads restricted to the allowed roots",
            Self::FsDenyRead => "explicit denied-read paths hidden from processes",
            Self::FsWrite => "writes restricted to the writable roots",
            Self::FsProtect => "protected workspace metadata blocked from process writes",
            Self::FsHostProtect => "host runtime secret directories hidden from confined processes",
            Self::NetBlock => "the network blocked by the kernel",
            Self::NetProxy => "connections forced through the filtering proxy (proxy variables alone don't count)",
            Self::UnixSocket => "Unix sockets (local daemons) blocked except the listed ones",
        }
    }

    /// `accept_partial` entries: capability names, plus the coarse groups "network" (net_block,
    /// net_proxy, unix_socket), "read" and "write".
    pub fn parse_accept(name: &str) -> Result<Vec<Self>, String> {
        Ok(match name {
            "network" => vec![Self::NetBlock, Self::NetProxy, Self::UnixSocket],
            "read" | "fs_read" => vec![Self::FsRead],
            "fs_deny_read" => vec![Self::FsDenyRead],
            "write" | "fs_write" => vec![Self::FsWrite],
            "fs_protect" => vec![Self::FsProtect],
            "fs_host_protect" => vec![Self::FsHostProtect],
            "net_block" => vec![Self::NetBlock],
            "net_proxy" => vec![Self::NetProxy],
            "unix_socket" | "unix" => vec![Self::UnixSocket],
            o => {
                return Err(format!(
                    "accept_partial: unknown capability `{o}` (fs_read, fs_deny_read, fs_write, fs_protect, \
                     fs_host_protect, net_block, net_proxy, unix_socket, network)"
                ));
            }
        })
    }
}

/// `MARE_SANDBOX_CAPS=fs_write,net_block` narrows the probed backend's capabilities (tests: a
/// backend that declares only some dimensions; debugging). It is intersected with the probed set
/// ([`narrow`]), so naming a dimension the backend lacks never grants it —
/// it can only make matching refuse more.
pub(super) fn caps_override() -> Option<Vec<Cap>> {
    let v = std::env::var("MARE_SANDBOX_CAPS").ok()?;
    Some(
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .flat_map(|n| Cap::parse_accept(n).unwrap_or_default())
            .collect(),
    )
}

/// The probed capabilities, narrowed to `over` when given (never widened).
pub(super) fn narrow(probed: Vec<Cap>, over: Option<&[Cap]>) -> Vec<Cap> {
    match over {
        Some(o) => probed.into_iter().filter(|c| o.contains(c)).collect(),
        None => probed,
    }
}
