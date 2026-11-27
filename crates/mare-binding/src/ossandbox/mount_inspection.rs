//! Conservative procfs visibility and writable mount alias checks for bubblewrap.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Mount {
    device: Vec<u8>,
    root: PathBuf,
    target: PathBuf,
    filesystem: Vec<u8>,
    options: Vec<Vec<u8>>,
}

fn invalid() -> io::Error {
    io::Error::other("cannot prove mount or procfs visibility")
}

fn path_field(raw: &[u8]) -> io::Result<PathBuf> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        if i + 3 >= raw.len() || !raw[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            return Err(invalid());
        }
        let n = ((raw[i + 1] - b'0') as u16) * 64 + ((raw[i + 2] - b'0') as u16) * 8 + (raw[i + 3] - b'0') as u16;
        if n > 255 {
            return Err(invalid());
        }
        out.push(n as u8);
        i += 4;
    }
    Ok(PathBuf::from(OsString::from_vec(out)))
}

fn mounts() -> io::Result<Vec<Mount>> {
    let data = std::fs::read("/proc/self/mountinfo")?;
    let mut out = Vec::new();
    for line in data.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split(|b| *b == b' ').collect();
        let sep = fields.iter().position(|x| *x == b"-").ok_or_else(invalid)?;
        if sep < 6 || fields.len() <= sep + 3 {
            return Err(invalid());
        }
        let mut options: Vec<Vec<u8>> = fields[5].split(|b| *b == b',').map(|x| x.to_vec()).collect();
        options.extend(fields[sep + 3].split(|b| *b == b',').map(|x| x.to_vec()));
        out.push(Mount {
            device: fields[2].to_vec(),
            root: path_field(fields[3])?,
            target: path_field(fields[4])?,
            filesystem: fields[sep + 1].to_vec(),
            options,
        });
    }
    if out.is_empty() {
        return Err(invalid());
    }
    Ok(out)
}

/// Require the procfs PID view to match our namespace and reject filtered or ambiguous mounts.
fn validate_proc(table: &[Mount]) -> io::Result<()> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c"/proc".as_ptr(), &mut stat) } != 0 || stat.f_type as u64 != 0x9fa0 {
        return Err(invalid());
    }
    // Name is an opaque byte string too; decode only the numeric namespace field.
    let status = std::fs::read("/proc/self/status")?;
    let ids: Vec<u32> = status
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"NSpid:"))
        .ok_or_else(invalid)?
        .split(|b| b.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .map(|field| std::str::from_utf8(field).map_err(|_| invalid())?.parse().map_err(|_| invalid()))
        .collect::<io::Result<_>>()?;
    if ids.as_slice() != [std::process::id()] {
        return Err(invalid());
    }
    let proc_mounts: Vec<_> = table.iter().filter(|m| m.target == Path::new("/proc")).collect();
    if proc_mounts.is_empty() {
        return Err(invalid());
    }
    for m in proc_mounts {
        if m.filesystem != b"proc"
            || m.root != Path::new("/")
            || m.options.iter().any(|o| {
                (o.starts_with(b"hidepid=") && o != b"hidepid=0" && o != b"hidepid=off") || o.starts_with(b"subset=")
            })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn visible_proc() -> io::Result<()> {
    validate_proc(&mounts()?)
}

/// All mount views containing `path`, including hidden overmounts: false positives are safe.
fn addresses<'a>(table: &'a [Mount], path: &Path) -> io::Result<Vec<(&'a [u8], PathBuf)>> {
    let out: Vec<_> = table
        .iter()
        .filter_map(|m| path.strip_prefix(&m.target).ok().map(|suffix| (m.device.as_slice(), m.root.join(suffix))))
        .collect();
    if out.is_empty() {
        return Err(invalid());
    }
    Ok(out)
}

fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Reject writable access to the protection registry, including through nested bind mounts.
pub(super) fn writable_mounts_safe(roots: &[PathBuf], registry: Option<&Path>) -> io::Result<bool> {
    if registry.is_none() {
        return Ok(true);
    }
    let table = mounts()?;
    validate_proc(&table)?;
    writable_mounts_safe_in(&table, roots, registry)
}

/// Project protected paths through the finite mount table, including bind views below
/// writable roots. Canonicalization alone does not resolve a bind-mount alias.
pub(super) fn protection_aliases(paths: &[PathBuf], roots: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let table = mounts()?;
    validate_proc(&table)?;
    protection_aliases_in(paths, roots, &table)
}

/// Project all visible mount aliases of `path` (or its deepest existing ancestor when `path` does
/// not exist yet), failing if procfs/mountinfo cannot be verified.
pub(super) fn all_aliases_or_fail(path: &Path) -> io::Result<Vec<PathBuf>> {
    let table = mounts()?;
    validate_proc(&table)?;

    let mut cur = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while std::fs::metadata(&cur).is_err() {
        let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
            return Err(invalid());
        };
        let Some(parent) = cur.parent() else {
            return Err(invalid());
        };
        tail.push(name);
        cur = parent.to_path_buf();
    }

    let anc_aliases = protection_aliases_in(&[cur], &[PathBuf::from("/")], &table)?;
    let mut out: Vec<PathBuf> = anc_aliases
        .into_iter()
        .map(|mut a| {
            for part in tail.iter().rev() {
                a.push(part);
            }
            a
        })
        .collect();
    out.sort_by_key(|p| p.components().count());
    out.dedup();
    Ok(out)
}

fn protection_aliases_in(paths: &[PathBuf], roots: &[PathBuf], table: &[Mount]) -> io::Result<Vec<PathBuf>> {
    use std::os::unix::fs::MetadataExt;

    let mut out = paths.to_vec();
    for path in paths {
        std::fs::metadata(path)?;
        for (device, address) in addresses(table, path)? {
            for mount in table {
                if mount.device != device {
                    continue;
                }
                let (source_path, alias) = if let Ok(suffix) = address.strip_prefix(&mount.root) {
                    (path.clone(), mount.target.join(suffix))
                } else if let Ok(suffix) = mount.root.strip_prefix(&address) {
                    // A bind of a descendant exposes only part of the protected
                    // subtree; seal the alias mount itself in that case.
                    (path.join(suffix), mount.target.clone())
                } else {
                    continue;
                };
                if !roots.iter().any(|root| alias.starts_with(root)) || out.contains(&alias) {
                    continue;
                }
                // Hidden overmounts can produce a backing-address candidate that is
                // not visible here. Only project the actual protected object.
                let source = match std::fs::metadata(&source_path) {
                    Ok(source) => source,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                let target = match std::fs::metadata(&alias) {
                    Ok(target) => target,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                if (source.dev(), source.ino()) != (target.dev(), target.ino()) {
                    continue;
                }
                if dunce::canonicalize(&alias)? != alias {
                    return Err(invalid());
                }
                out.push(alias);
            }
        }
    }
    out.sort_by_key(|path| path.components().count());
    out.dedup();
    Ok(out)
}

fn writable_mounts_safe_in(table: &[Mount], roots: &[PathBuf], registry: Option<&Path>) -> io::Result<bool> {
    let registry_addresses = if let Some(registry) = registry { addresses(table, registry)? } else { vec![] };
    for root in roots {
        let mut root_addresses = addresses(table, root)?;
        // A bind mount below a writable root is writable too, even if the root's own
        // backing address is unrelated to the registry.
        root_addresses
            .extend(table.iter().filter(|m| m.target.starts_with(root)).map(|m| (m.device.as_slice(), m.root.clone())));
        if root_addresses.iter().any(|(d, p)| registry_addresses.iter().any(|(r, q)| d == r && overlap(p, q))) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn capable(args: &[&str]) -> bool {
        Command::new("unshare")
            .args(args)
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn protection_alias_projection_tracks_backing_paths_and_visible_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source_root = dir.path().join("source");
        let ancestor = dir.path().join("ancestor");
        let descendant = dir.path().join("descendant");
        let hidden = dir.path().join("hidden");
        for path in [source_root.join("parent/child"), ancestor.join("child"), descendant.clone(), hidden.clone()] {
            std::fs::create_dir_all(path).unwrap();
        }
        let source = source_root.join("parent/child/metadata");
        std::fs::write(&source, "protected").unwrap();
        let ancestor_path = ancestor.join("child/metadata");
        let descendant_path = descendant.join("metadata");
        std::fs::hard_link(&source, &ancestor_path).unwrap();
        std::fs::hard_link(&source, &descendant_path).unwrap();
        std::fs::write(hidden.join("metadata"), "unrelated overmount").unwrap();
        let mount = |root: &str, target: PathBuf| Mount {
            device: b"0:1".to_vec(),
            root: root.into(),
            target,
            filesystem: b"test".to_vec(),
            options: vec![],
        };
        let mut table = vec![mount("/", source_root.clone())];
        let roots = vec![source_root, ancestor.clone(), descendant.clone(), hidden.clone()];
        assert_eq!(protection_aliases_in(std::slice::from_ref(&source), &roots, &table).unwrap(), vec![source.clone()]);
        table.extend([
            mount("/parent", ancestor),
            mount("/parent/child", descendant),
            mount("/parent/child", hidden.clone()),
        ]);
        let projected = protection_aliases_in(std::slice::from_ref(&source), &roots, &table).unwrap();
        assert_eq!(projected.len(), 3);
        assert!(projected.contains(&source));
        assert!(projected.contains(&ancestor_path));
        assert!(projected.contains(&descendant_path));
        assert!(!projected.contains(&hidden.join("metadata")), "hidden backing address is not the visible inode");
    }

    #[test]
    fn filesystem_type_does_not_create_a_gap() {
        for fs in [b"overlay".as_slice(), b"fuse.test", b"nfs", b"9p"] {
            let table = vec![Mount {
                device: b"0:1".to_vec(),
                root: PathBuf::from("/"),
                target: PathBuf::from("/"),
                filesystem: fs.to_vec(),
                options: vec![],
            }];
            assert!(
                writable_mounts_safe_in(&table, &[PathBuf::from("/workspace")], Some(Path::new("/tmp/registry")))
                    .unwrap()
            );
            assert!(
                !writable_mounts_safe_in(&table, &[PathBuf::from("/tmp")], Some(Path::new("/tmp/registry"))).unwrap()
            );
        }
    }

    #[test]
    fn non_utf8_own_process_name_does_not_hide_pid_view() {
        if std::env::var_os("MARE_TEST_MOUNT_NONUTF8_SELF").is_some() {
            if visible_proc().is_err() {
                eprintln!("skipped: complete procfs visibility is unavailable");
                return;
            }
            std::fs::write("/proc/self/comm", b"\xffself").unwrap();
            assert!(std::str::from_utf8(&std::fs::read("/proc/self/status").unwrap()).is_err());
            assert!(visible_proc().is_ok(), "only the ASCII NSpid field must be decoded");
            assert!(super::super::bwrap_guard::group_has_live_processes(u32::MAX).is_ok());
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ossandbox::mount_inspection::tests::non_utf8_own_process_name_does_not_hide_pid_view",
                "--nocapture",
            ])
            .env("MARE_TEST_MOUNT_NONUTF8_SELF", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn rejects_procfs_from_an_ancestor_pid_namespace() {
        if std::env::var_os("MARE_TEST_MOUNT_PIDNS_CHILD").is_some() {
            let mut command = Command::new("sleep");
            command.arg("30").process_group(0);
            let mut child = command.spawn().unwrap();
            assert!(visible_proc().is_err(), "ancestor procfs must not be treated as complete");
            assert!(super::super::bwrap_guard::group_has_live_processes(child.id()).is_err());
            child.kill().unwrap();
            child.wait().unwrap();
            return;
        }
        if !capable(&["--user", "--map-root-user", "--pid", "--fork"]) {
            return;
        }
        let status = Command::new("unshare")
            .args(["--user", "--map-root-user", "--pid", "--fork"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ossandbox::mount_inspection::tests::rejects_procfs_from_an_ancestor_pid_namespace",
                "--nocapture",
            ])
            .env("MARE_TEST_MOUNT_PIDNS_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn rejects_filtered_procfs() {
        if std::env::var_os("MARE_TEST_MOUNT_HIDEPID_CHILD").is_some() {
            assert!(visible_proc().is_ok(), "procfs mounted for the current namespace is valid");
            assert!(Command::new("mount").args(["-o", "remount,hidepid=2", "/proc"]).status().unwrap().success());
            assert!(visible_proc().is_err(), "hidden proc entries make group emptiness unknown");
            return;
        }
        if !capable(&["--user", "--map-root-user", "--pid", "--mount", "--fork", "--mount-proc"]) {
            return;
        }
        let status = Command::new("unshare")
            .args(["--user", "--map-root-user", "--pid", "--mount", "--fork", "--mount-proc"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "ossandbox::mount_inspection::tests::rejects_filtered_procfs", "--nocapture"])
            .env("MARE_TEST_MOUNT_HIDEPID_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn rejects_real_bind_mount_aliases() {
        if let Some(root) = std::env::var_os("MARE_TEST_MOUNT_BIND_CHILD") {
            let base = PathBuf::from(root);
            let registry = base.join("registry");
            let normal = base.join("normal");
            let root_alias = base.join("root alias");
            let ancestor_alias = base.join("ancestor-alias");
            let descendant_alias = base.join("descendant-alias");
            let nested = base.join("nested");
            let self_alias = base.join("self-alias");
            let extra_alias = base.join("extra-alias");
            for path in [
                &registry,
                &normal,
                &root_alias,
                &ancestor_alias,
                &descendant_alias,
                &nested,
                &self_alias,
                &extra_alias,
            ] {
                std::fs::create_dir(path).unwrap();
            }
            std::fs::create_dir(registry.join("subdir")).unwrap();
            std::fs::create_dir(nested.join("alias")).unwrap();
            std::fs::create_dir(self_alias.join("alias")).unwrap();
            let bind = |src: &Path, dst: &Path| {
                assert!(Command::new("mount").arg("--bind").arg(src).arg(dst).status().unwrap().success())
            };
            assert!(writable_mounts_safe(std::slice::from_ref(&normal), Some(&registry)).unwrap());
            bind(&registry, &root_alias);
            assert!(!writable_mounts_safe(std::slice::from_ref(&root_alias), Some(&registry)).unwrap());
            super::super::bwrap_guard::TEST_REGISTRY.with(|slot| *slot.borrow_mut() = Some(registry.clone()));
            let registry_alias_policy = super::super::Policy::build(
                super::super::Level::WorkspaceWrite,
                &root_alias,
                None,
                &mlua::Value::Boolean(true),
            )
            .unwrap();
            assert!(!super::super::bwrap_guard::registry_safe(&registry_alias_policy));
            assert!(registry_alias_policy.bwrap_metadata_gap(), "registry alias must be a policy gap");
            super::super::bwrap_guard::TEST_REGISTRY.with(|slot| *slot.borrow_mut() = None);
            bind(&base, &ancestor_alias);
            assert!(!writable_mounts_safe(std::slice::from_ref(&ancestor_alias), Some(&registry)).unwrap());
            bind(&registry.join("subdir"), &descendant_alias);
            assert!(!writable_mounts_safe(std::slice::from_ref(&descendant_alias), Some(&registry)).unwrap());
            bind(&registry, &nested.join("alias"));
            assert!(!writable_mounts_safe(std::slice::from_ref(&nested), Some(&registry)).unwrap());
            std::fs::create_dir(self_alias.join(".git")).unwrap();
            std::fs::write(self_alias.join(".git/config"), "protected").unwrap();
            std::fs::create_dir(self_alias.join(".git/subdir")).unwrap();
            std::fs::write(self_alias.join(".git/subdir/config"), "protected").unwrap();
            std::fs::create_dir(self_alias.join("metadata-child")).unwrap();
            bind(&self_alias.join(".git/subdir"), &self_alias.join("metadata-child"));
            std::fs::create_dir(self_alias.join(".mare")).unwrap();
            std::fs::write(self_alias.join(".mare/config"), "protected").unwrap();
            bind(&self_alias, &self_alias.join("alias"));
            assert!(writable_mounts_safe(std::slice::from_ref(&self_alias), Some(&registry)).unwrap());
            let policy = super::super::Policy::build(
                super::super::Level::WorkspaceWrite,
                &self_alias,
                None,
                &mlua::Value::Boolean(true),
            )
            .unwrap();
            assert!(!policy.bwrap_metadata_gap(), "a workspace alias must not itself create a gap");
            assert_eq!(
                super::super::backend(),
                super::super::Backend::Bwrap,
                "bind-mount execution requires bubblewrap"
            );
            {
                let prepared = super::super::Prepared::from_policy(policy.clone(), &self_alias).unwrap();
                let lua = mlua::Lua::new();
                assert_eq!(prepared.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "full");
                let wrapped = prepared.wrap("sh", &["-c".into(), "printf ok > ordinary".into()]);
                let output = Command::new(wrapped.prog).args(wrapped.args).output().unwrap();
                assert!(output.status.success(), "ordinary write failed: {output:?}");
                assert_eq!(std::fs::read_to_string(self_alias.join("ordinary")).unwrap(), "ok");
                for path in
                    [".git/config", ".mare/config", "alias/.git/config", "alias/.mare/config", "metadata-child/config"]
                {
                    let wrapped =
                        prepared.wrap("sh", &["-c".into(), "printf changed > \"$1\"".into(), "sh".into(), path.into()]);
                    let output = Command::new(wrapped.prog).args(wrapped.args).output().unwrap();
                    assert!(!output.status.success(), "protected write to {path} succeeded: {output:?}");
                    assert_eq!(std::fs::read_to_string(self_alias.join(path)).unwrap(), "protected");
                }
            }
            // Two overlapping calls must share the missing target's original lease,
            // while each namespace protects all direct and bind-alias spellings.
            std::fs::remove_file(self_alias.join(".mare/config")).unwrap();
            std::fs::remove_dir(self_alias.join(".mare")).unwrap();
            let first = super::super::Prepared::from_policy(policy.clone(), &self_alias).unwrap();
            let second = super::super::Prepared::from_policy(policy, &self_alias).unwrap();
            let lua = mlua::Lua::new();
            let mut children = vec![];
            for (index, prepared) in [&first, &second].into_iter().enumerate() {
                assert_eq!(prepared.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "full");
                let wrapped = prepared.wrap("sh", &[
                    "-c".into(),
                    "printf ok > ordinary-$1; for p in .git/config alias/.git/config .mare/new alias/.mare/new; do if printf changed > \"$p\"; then exit 1; fi; done".into(),
                    "sh".into(), index.to_string(),
                ]);
                children.push(Command::new(wrapped.prog).args(wrapped.args).spawn().unwrap());
            }
            for mut child in children {
                assert!(child.wait().unwrap().success());
            }
            for index in 0..2 {
                assert_eq!(std::fs::read_to_string(self_alias.join(format!("ordinary-{index}"))).unwrap(), "ok");
            }
            assert_eq!(std::fs::read_to_string(self_alias.join(".git/config")).unwrap(), "protected");
            assert!(!self_alias.join(".mare/new").exists());
            drop(first);
            assert!(self_alias.join(".mare").exists(), "second call still holds the shared target lease");
            drop(second);
            assert!(!self_alias.join(".mare").exists(), "last call removes its synthetic target");
            bind(&normal, &extra_alias);
            assert!(writable_mounts_safe(&[normal, extra_alias], Some(&registry)).unwrap());
            return;
        }
        let probe = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "true"])
            .output()
            .expect("probe mount namespace support");
        if !probe.status.success() {
            eprintln!("skipped bind-mount execution: mount namespace unavailable: {probe:?}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "ossandbox::mount_inspection::tests::rejects_real_bind_mount_aliases", "--nocapture"])
            .env("MARE_TEST_MOUNT_BIND_CHILD", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
    }
}
