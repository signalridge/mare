//! What a Linux child installs right before exec: the Landlock grants and the seccomp filter.

use std::path::PathBuf;

/// What Landlock grants a child (applied right before exec).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandlockGrants {
    /// read/execute beneath these; `None` = everywhere
    pub read: Option<Vec<PathBuf>>,
    /// every right beneath these
    pub rw: Vec<PathBuf>,
    /// write rights only (writable but outside the read set)
    pub write_only: Vec<PathBuf>,
}

/// Socket families a Linux child may not create (seccomp, applied right before exec; the bwrap
/// process passes it on to everything it starts). Backs the `unix_socket` and — for Landlock —
/// `net_block` dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SocketBlock {
    /// AF_UNIX (local daemons)
    pub unix: bool,
    /// AF_INET / AF_INET6
    pub inet: bool,
    /// Permit `socket(AF_NETLINK, ..., NETLINK_ROUTE)` only when `bwrap --unshare-net` runs under
    /// this filter inside an isolated network namespace.
    pub netlink_route: bool,
}

impl SocketBlock {
    pub fn any(self) -> bool {
        self.unix || self.inet
    }
}

/// Apply Landlock to the current (child) process: read where `g` grants it (everywhere unless
/// reads are restricted), write only under its writable paths. Called from `pre_exec`, right
/// before exec.
#[cfg(target_os = "linux")]
pub fn landlock_restrict(g: &LandlockGrants) -> std::io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
    };
    let abi = ABI::V5;
    let err = |e: landlock::RulesetError| std::io::Error::other(format!("landlock: {e}"));
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(err)?
        .create()
        .map_err(err)?
        .add_rules(path_beneath_rules(g.read.as_deref().unwrap_or(&[PathBuf::from("/")]), AccessFs::from_read(abi)))
        .map_err(err)?
        .add_rules(path_beneath_rules(&g.rw, AccessFs::from_all(abi)))
        .map_err(err)?
        .add_rules(path_beneath_rules(&g.write_only, AccessFs::from_write(abi)))
        .map_err(err)?
        .restrict_self()
        .map_err(err)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(std::io::Error::other("landlock: not enforced by this kernel"));
    }
    Ok(())
}

/// Classic-BPF seccomp program applied unconditionally on Linux:
/// - rejects foreign architectures and the x32 ABI
/// - blocks `io_uring_setup` / `io_uring_enter` / `io_uring_register`, `request_key`, `ptrace`,
///   `process_vm_readv`, and `process_vm_writev`
/// - when network is open (`!b.unix && !b.inet`), blocks `AF_VSOCK` on `socket(2)`/`socketpair(2)`
/// - when network is restricted (`b.unix || b.inet`), enforces an allowlist on `socket(2)` and
///   permits only `AF_UNIX` on `socketpair(2)` (blocking `AF_VSOCK`, `AF_NETLINK`, etc.)
#[cfg(target_os = "linux")]
pub fn seccomp_filter(b: SocketBlock) -> std::io::Result<Vec<libc::sock_filter>> {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(std::io::Error::other("seccomp socket filter: unsupported architecture"));
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
        const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
        const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
        const JA: u16 = 0x05; // BPF_JMP | BPF_JA
        const RET_K: u16 = 0x06; // BPF_RET | BPF_K
        const ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
        const DENY: u32 = 0x0005_0000 | libc::EPERM as u32; // SECCOMP_RET_ERRNO | EPERM

        enum Jump {
            Next,
            Skip(u8),
            Allow,
            Deny,
        }
        struct Pending {
            code: u16,
            k: u32,
            jt: Jump,
            jf: Jump,
        }
        let mut ops: Vec<Pending> = vec![
            Pending { code: LD_W_ABS, k: 4, jt: Jump::Next, jf: Jump::Next }, // arch
            Pending { code: JEQ_K, k: ARCH, jt: Jump::Next, jf: Jump::Deny },
            Pending { code: LD_W_ABS, k: 0, jt: Jump::Next, jf: Jump::Next }, // syscall nr
            Pending { code: JGE_K, k: 0x4000_0000, jt: Jump::Deny, jf: Jump::Next }, // x32 ABI
            Pending { code: JEQ_K, k: libc::SYS_io_uring_setup as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_io_uring_enter as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_io_uring_register as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_request_key as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_ptrace as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_process_vm_readv as u32, jt: Jump::Deny, jf: Jump::Next },
            Pending { code: JEQ_K, k: libc::SYS_process_vm_writev as u32, jt: Jump::Deny, jf: Jump::Next },
        ];
        if !b.unix && !b.inet {
            // Open network: block AF_VSOCK on both socket(2) and socketpair(2).
            ops.push(Pending { code: JEQ_K, k: libc::SYS_socket as u32, jt: Jump::Skip(1), jf: Jump::Next });
            ops.push(Pending { code: JEQ_K, k: libc::SYS_socketpair as u32, jt: Jump::Next, jf: Jump::Allow });
            ops.push(Pending { code: LD_W_ABS, k: 16, jt: Jump::Next, jf: Jump::Next });
            ops.push(Pending { code: JEQ_K, k: libc::AF_VSOCK as u32, jt: Jump::Deny, jf: Jump::Allow });
        } else {
            // Restricted network: allowlist socket(2) and socketpair(2) families.
            ops.push(Pending { code: JEQ_K, k: libc::SYS_socketpair as u32, jt: Jump::Next, jf: Jump::Skip(2) });
            ops.push(Pending { code: LD_W_ABS, k: 16, jt: Jump::Next, jf: Jump::Next });
            ops.push(Pending { code: JEQ_K, k: libc::AF_UNIX as u32, jt: Jump::Allow, jf: Jump::Deny });

            let mut allowed = Vec::new();
            if !b.unix {
                allowed.push(libc::AF_UNIX as u32);
            }
            if !b.inet {
                allowed.push(libc::AF_INET as u32);
                allowed.push(libc::AF_INET6 as u32);
            }
            if allowed.is_empty() && !b.netlink_route {
                ops.push(Pending { code: JEQ_K, k: libc::SYS_socket as u32, jt: Jump::Deny, jf: Jump::Allow });
            } else {
                ops.push(Pending { code: JEQ_K, k: libc::SYS_socket as u32, jt: Jump::Next, jf: Jump::Allow });
                ops.push(Pending { code: LD_W_ABS, k: 16, jt: Jump::Next, jf: Jump::Next });
                for fam in allowed {
                    ops.push(Pending { code: JEQ_K, k: fam, jt: Jump::Allow, jf: Jump::Next });
                }
                if b.netlink_route {
                    ops.push(Pending { code: JEQ_K, k: libc::AF_NETLINK as u32, jt: Jump::Next, jf: Jump::Deny });
                    ops.push(Pending { code: LD_W_ABS, k: 32, jt: Jump::Next, jf: Jump::Next });
                    ops.push(Pending { code: JEQ_K, k: libc::NETLINK_ROUTE as u32, jt: Jump::Allow, jf: Jump::Deny });
                } else {
                    ops.push(Pending { code: JA, k: 0, jt: Jump::Deny, jf: Jump::Deny });
                }
            }
        }
        let allow_at = ops.len();
        let deny_at = allow_at + 1;
        let resolve = |j: Jump, at: usize| -> u8 {
            match j {
                Jump::Next => 0,
                Jump::Skip(n) => n,
                Jump::Allow => (allow_at - (at + 1)) as u8,
                Jump::Deny => (deny_at - (at + 1)) as u8,
            }
        };
        let mut p: Vec<libc::sock_filter> = ops
            .into_iter()
            .enumerate()
            .map(|(at, op)| {
                if op.code == JA {
                    libc::sock_filter { code: JA, jt: 0, jf: 0, k: (deny_at - (at + 1)) as u32 }
                } else {
                    libc::sock_filter { code: op.code, jt: resolve(op.jt, at), jf: resolve(op.jf, at), k: op.k }
                }
            })
            .collect();
        p.push(libc::sock_filter { code: RET_K, jt: 0, jf: 0, k: ALLOW });
        p.push(libc::sock_filter { code: RET_K, jt: 0, jf: 0, k: DENY });
        Ok(p)
    }
}

/// In the child, right before exec: Landlock (when `rw` is given), then the seccomp socket
/// filter (when given). Only syscalls — the filter is built in the parent.
#[cfg(target_os = "linux")]
pub fn child_restrict(landlock: Option<&LandlockGrants>, filter: Option<&[libc::sock_filter]>) -> std::io::Result<()> {
    if let Some(g) = landlock {
        landlock_restrict(g)?;
    }
    if let Some(f) = filter {
        let prog = libc::sock_fprog { len: f.len() as u16, filter: f.as_ptr() as *mut libc::sock_filter };
        // SAFETY: plain prctl calls on the current (child) process; `prog` outlives them
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog as *const libc::sock_fprog) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
