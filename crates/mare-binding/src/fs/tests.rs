use super::dir::list_dir_entries;
use super::read::read_capped;
use super::write::{WriteOptions, capture_directory_sync_plan, sync_parent_dirs, write_file};

#[test]
fn path_globs_respect_components_and_recursive_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    super::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
        local one = assert(mare.fs.compile_path_glob('docs/*.txt', mare.fs.cwd))
        assert(one:matches(mare.fs.cwd .. '/docs/a.txt'))
        assert(not one:matches(mare.fs.cwd .. '/docs/nested/a.txt'))
        assert(not one:matches(mare.fs.cwd .. '/docs2/a.txt'))
        assert(one:overlaps(mare.fs.cwd .. '/docs'))
        assert(not one:overlaps(mare.fs.cwd .. '/src'))
        assert(not one:covers_recursive(mare.fs.cwd .. '/docs'))
        local tree = assert(mare.fs.compile_path_glob('docs/**', mare.fs.cwd))
        assert(tree:matches(mare.fs.cwd .. '/docs/nested/a.txt'))
        assert(tree:covers_recursive(mare.fs.cwd .. '/docs'))
        assert(tree:covers_recursive(mare.fs.cwd .. '/docs/sub'))
        assert(not tree:covers_recursive(mare.fs.cwd .. '/docs2'))
        assert(not mare.fs.compile_path_glob('', mare.fs.cwd))
        assert(not mare.fs.compile_path_glob('docs/[broken', mare.fs.cwd))
        assert(not mare.fs.compile_path_glob('docs/*/../secret', mare.fs.cwd))
        assert(mare.fs.path_within('/ws/docs', '/ws/docs/'))
        assert(mare.fs.path_within('/ws/docs/', '/ws/docs'))
        assert(mare.fs.path_within('/', '/'))
        assert(not mare.fs.path_within('/ws/docs2', '/ws/docs/'))
        "#,
    )
    .exec()
    .unwrap();
}

#[cfg(unix)]
#[test]
fn realpath_rejects_dangling_loops_and_unresolvable_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("real")).unwrap();
    std::fs::write(ws.join("file"), "data").unwrap();
    std::os::unix::fs::symlink("missing-target", ws.join("dangling")).unwrap();
    std::os::unix::fs::symlink("loop", ws.join("loop")).unwrap();
    std::os::unix::fs::symlink("real", ws.join("alias")).unwrap();
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    super::install(&lua, &mare, ws.to_path_buf()).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let (valid, missing_leaf, dangling, cycle, not_dir, missing_parent): (String, String, bool, bool, bool, bool) = lua
        .load(
            r#"
            local function rejected(path)
                local ok, result = pcall(mare.fs.realpath, path)
                return not ok or result == nil
            end
            return mare.fs.realpath('alias'), mare.fs.realpath('real/new-file'),
                rejected('dangling/file'), rejected('loop/file'), rejected('file/child'),
                rejected('missing/../real/file')
            "#,
        )
        .eval()
        .unwrap();
    assert_eq!(std::path::Path::new(&valid), dunce::canonicalize(ws.join("real")).unwrap());
    assert_eq!(std::path::Path::new(&missing_leaf), dunce::canonicalize(ws.join("real")).unwrap().join("new-file"));
    assert!(dangling && cycle && not_dir && missing_parent, "unresolvable paths must not become allow identities");
}

#[tokio::test]
async fn trust_git_dir_checks_read_policy_for_gitfile_target() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join("gitdata/objects")).unwrap();
    std::fs::create_dir_all(ws.join("gitdata/refs")).unwrap();
    std::fs::write(ws.join("gitdata/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(ws.join(".git"), "gitdir: gitdata\n").unwrap();
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::proc::install(&lua, &mare, ws.to_string_lossy().into_owned()).unwrap();
    super::install(&lua, &mare, ws.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let (found, denied, error): (bool, bool, String) = lua
        .load(
            r#"
            local found = mare.fs.git_dir('.') ~= nil
            mare.proc.sandbox.set_current({ level = 'workspace-write', deny_read = { 'gitdata/HEAD' },
                accept_partial = { 'fs_deny_read' } })
            return found, mare.exec.run({ tool = 'read' }, function()
                local ok, err = pcall(mare.fs.git_dir, '.')
                return not ok, tostring(err)
            end)
        "#,
        )
        .eval_async()
        .await
        .unwrap();
    assert!(found && denied && error.contains("denied"), "{found} {denied} {error}");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let gitdir = ws.join(std::ffi::OsString::from_vec(b"git\xffdata".to_vec()));
        match std::fs::create_dir_all(gitdir.join("objects")) {
            Ok(()) => {
                std::fs::create_dir_all(gitdir.join("refs")).unwrap();
                std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            }
            // macOS APFS rejects non-UTF-8 filenames at creation time (EILSEQ); the gitfile
            // target path is still checked and rejected as non-UTF-8 before accessing disk.
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {}
            Err(e) => panic!("failed to create non-UTF-8 gitdir: {e}"),
        }
        std::fs::write(ws.join(".git"), b"gitdir: git\xffdata\n").unwrap();
        let (ok, error): (bool, String) =
            lua.load("local ok, err = pcall(mare.fs.git_dir, '.'); return ok, tostring(err)").eval().unwrap();
        assert!(!ok && error.contains("UTF-8"), "{ok} {error}");
    }
}

#[tokio::test]
async fn git_branch_reads_head_and_enforces_read_policy() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let sub = ws.join("nested/dir");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(ws.join("gitdata/objects")).unwrap();
    std::fs::create_dir_all(ws.join("gitdata/refs")).unwrap();
    std::fs::write(ws.join("gitdata/HEAD"), "ref: refs/heads/trunk\n").unwrap();
    std::fs::write(ws.join(".git"), "gitdir: gitdata\n").unwrap();

    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::proc::install(&lua, &mare, ws.to_string_lossy().into_owned()).unwrap();
    super::install(&lua, &mare, ws.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let (default_branch, nested_branch, denied, error): (Option<String>, Option<String>, bool, String) = lua
        .load(
            r#"
            local default_branch = mare.fs.git_branch()
            local nested_branch = mare.fs.git_branch('nested/dir')
            mare.proc.sandbox.set_current({ level = 'workspace-write', deny_read = { 'gitdata/HEAD' },
                accept_partial = { 'fs_deny_read' } })
            return default_branch, nested_branch, mare.exec.run({ tool = 'read' }, function()
                local ok, err = pcall(mare.fs.git_branch, 'nested/dir')
                return not ok, tostring(err)
            end)
        "#,
        )
        .eval_async()
        .await
        .unwrap();
    assert_eq!(default_branch.as_deref(), Some("trunk"));
    assert_eq!(nested_branch.as_deref(), Some("trunk"));
    assert!(denied && error.contains("denied"), "{denied} {error}");
}

#[test]
fn atomic_write_validates_boundary_and_propagates_directory_sync_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let boundary = root.path().join("boundary");
    let objects = boundary.join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    let path = objects.join("image.bin");
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    assert!(
        write_file(
            &path,
            b"bytes",
            WriteOptions { atomic: true, sync_boundary: Some(&other), ..Default::default() },
            &None
        )
        .is_err()
    );
    assert!(!path.exists(), "invalid boundary is rejected before writing");
    assert!(
        write_file(
            &path,
            b"bytes",
            WriteOptions { atomic: true, sync_boundary: Some(&boundary), ..Default::default() },
            &Some(vec![objects.clone()])
        )
        .is_err()
    );
    // The object directory already exists, possibly created by a concurrent first saver.
    // A write can succeed with execute/write permission while directory fsync cannot open
    // the vouched boundary. Publication must report that real OS error.
    std::fs::set_permissions(&boundary, std::fs::Permissions::from_mode(0o300)).unwrap();
    let result = write_file(
        &path,
        b"complete",
        WriteOptions { atomic: true, sync_boundary: Some(&boundary), ..Default::default() },
        &None,
    );
    std::fs::set_permissions(&boundary, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err(), "directory sync failure must prevent acknowledgement");
    assert_eq!(std::fs::read(&path).unwrap(), b"complete");
    write_file(
        &path,
        b"retry",
        WriteOptions { atomic: true, sync_boundary: Some(&boundary), ..Default::default() },
        &None,
    )
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"retry");
}

#[test]
fn directory_sync_refuses_a_symlink_replaced_after_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(root.join("objects")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let physical = root.join("objects/image.bin");
    std::fs::remove_dir(root.join("objects")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("objects")).unwrap();
    assert!(
        sync_parent_dirs(&physical, &root, &Some(vec![root.clone()])).is_err(),
        "sync must open relative to the controlled root without following a swapped symlink"
    );
}

#[test]
fn captured_directory_sync_plan_never_reopens_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let objects = dir.path().join("objects");
    let saved = dir.path().join("saved");
    std::fs::create_dir(&objects).unwrap();
    let plan = capture_directory_sync_plan(&objects.join("image.bin"), dir.path(), &None).unwrap();
    let held = plan.directories[0].1.metadata().unwrap();
    std::fs::rename(&objects, &saved).unwrap();
    std::fs::create_dir(&objects).unwrap();
    assert!(plan.sync().is_err(), "a new directory at the old name cannot stand in for the held publication directory");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(plan.directories[0].1.metadata().unwrap().ino(), held.ino());
}

/// the cap holds at the size check and again while reading: exactly at the cap is fine, a
/// file that is bigger than its stat said (here a FIFO: size 0, then 100 bytes) is refused
#[test]
fn read_capped_checks_the_size_and_the_bytes_read() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f");
    std::fs::write(&f, [b'x'; 64]).unwrap();
    assert_eq!(read_capped(&f, Some(64), "f").unwrap().len(), 64);
    assert!(read_capped(&f, Some(63), "f").unwrap_err().to_string().contains("(too_large)"));
    let fifo = dir.path().join("p");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let e = read_capped(&fifo, Some(64), "p").unwrap_err().to_string();
    assert!(e.contains("not a regular file"), "{e}");
}

#[test]
fn read_window_rejects_fifo_and_enforces_max_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"alpha\nbeta\ngamma\n").unwrap();
    let (lines, total, size, _) = super::read::read_window(&f, 2, 1, Some(32), "f.txt").unwrap();
    assert_eq!(lines, vec!["beta"]);
    assert_eq!(total, 3);
    assert_eq!(size, 17);
    let too_big = super::read::read_window(&f, 1, 10, Some(10), "f.txt").unwrap_err().to_string();
    assert!(too_big.contains("(too_large)"), "{too_big}");
    let fifo = dir.path().join("pipe");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let fifo_err = super::read::read_window(&fifo, 1, 10, None, "pipe").unwrap_err().to_string();
    assert!(fifo_err.contains("not a regular file"), "{fifo_err}");
}

#[test]
fn read_file_capped_rejects_post_stat_growth() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("grow.txt");
    std::fs::write(&f, [b'a'; 10]).unwrap();
    let f_hook = f.clone();
    super::testing::set_read_capped_after_stat_hook(move || {
        use std::io::Write;
        let mut w = std::fs::OpenOptions::new().append(true).open(&f_hook).unwrap();
        w.write_all(&[b'b'; 20]).unwrap();
    });
    let err = read_capped(&f, Some(16), "grow.txt").unwrap_err().to_string();
    assert!(err.contains("the content exceeds") && err.contains("(too_large)"), "{err}");
}

#[test]
fn list_dir_entries_strict_surfaces_missing_and_unreadable_dirs() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert_eq!(list_dir_entries(&empty, None, true).unwrap(), vec![]);

    let missing = dir.path().join("missing");
    assert!(list_dir_entries(&missing, None, false).unwrap().is_empty());
    assert_eq!(list_dir_entries(&missing, None, true).unwrap_err().kind(), std::io::ErrorKind::NotFound);

    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("a.txt"), "hi").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let non_strict = list_dir_entries(&locked, None, false);
    let strict = list_dir_entries(&locked, None, true);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(non_strict.unwrap().is_empty());
    assert_eq!(strict.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn anchored_root_rejects_symlinks_under_symlinked_ancestor_and_allows_normal_beneath_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let real_home = dir.path().join("real_home");
    let alias_home = dir.path().join("alias_home");
    let state_dir = real_home.join("state");
    let inside_target = state_dir.join("inside_target");
    std::fs::create_dir_all(&inside_target).unwrap();
    std::os::unix::fs::symlink(&real_home, &alias_home).unwrap();

    let alias_state = alias_home.join("state");
    let q_link = state_dir.join("quarantine");
    std::os::unix::fs::symlink(&inside_target, &q_link).unwrap();

    let err = write_file(
        &alias_state.join("quarantine/entry.jsonl"),
        b"data\n",
        WriteOptions { append: true, mode: Some(0o600), anchored_root: Some(&alias_state), ..Default::default() },
        &None,
    )
    .unwrap_err();
    assert!(
        matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR) | Some(libc::EXDEV))
            || err.kind() == std::io::ErrorKind::PermissionDenied,
        "unexpected error for symlinked quarantine under symlinked MARE_HOME: {err:?}"
    );
    assert_eq!(std::fs::read_dir(&inside_target).unwrap().count(), 0);

    // Outside root via `..` must also be rejected.
    let escape_err = write_file(
        &alias_state.join("../escaped.jsonl"),
        b"data\n",
        WriteOptions { append: true, mode: Some(0o600), anchored_root: Some(&alias_state), ..Default::default() },
        &None,
    )
    .unwrap_err();
    assert_eq!(escape_err.kind(), std::io::ErrorKind::PermissionDenied);

    // Explicit root itself being a symlink (e.g. MARE_HOME/state -> outside) must be rejected (O_NOFOLLOW).
    let symlink_root = dir.path().join("symlink_state_home/state");
    let outside_root = dir.path().join("outside_root");
    std::fs::create_dir_all(symlink_root.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&outside_root).unwrap();
    std::os::unix::fs::symlink(&outside_root, &symlink_root).unwrap();
    let root_link_err = write_file(
        &symlink_root.join("quarantine/entry.jsonl"),
        b"data\n",
        WriteOptions { append: true, mode: Some(0o600), anchored_root: Some(&symlink_root), ..Default::default() },
        &None,
    )
    .unwrap_err();
    assert!(
        matches!(root_link_err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
            || root_link_err.kind() == std::io::ErrorKind::PermissionDenied,
        "expected symlinked explicit root to be rejected, got: {root_link_err:?}"
    );
    assert_eq!(std::fs::read_dir(&outside_root).unwrap().count(), 0);

    // Normal workspace write through a workspace-internal directory symlink (like file_guard new-file
    // create with atomic + create_new) and through a file symlink must still succeed (0908 #11).
    let ws = dir.path().join("ws");
    let ws_real = ws.join("real_sub");
    std::fs::create_dir_all(&ws_real).unwrap();
    std::os::unix::fs::symlink(&ws_real, ws.join("link_sub")).unwrap();
    let ws_canonical = dunce::canonicalize(&ws).unwrap();
    write_file(
        &ws.join("link_sub/new.txt"),
        b"hello",
        WriteOptions { atomic: true, create_new: true, ..Default::default() },
        &Some(vec![ws_canonical.clone()]),
    )
    .unwrap();
    assert_eq!(std::fs::read(ws_real.join("new.txt")).unwrap(), b"hello");

    let file_link = ws.join("link_file.txt");
    std::os::unix::fs::symlink(ws_real.join("new.txt"), &file_link).unwrap();
    write_file(&file_link, b"updated", WriteOptions { atomic: true, ..Default::default() }, &Some(vec![ws_canonical]))
        .unwrap();
    assert_eq!(std::fs::read(ws_real.join("new.txt")).unwrap(), b"updated");
}

#[tokio::test]
async fn controlled_write_symlink_swap_to_git_after_policy_check_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_real = ws.join("real_sub");
    let ws_git = ws.join(".git");
    std::fs::create_dir_all(&ws_real).unwrap();
    std::fs::create_dir_all(&ws_git).unwrap();
    let git_config = ws_git.join("config");
    std::fs::write(&git_config, b"original-git-config").unwrap();

    let sub_link = ws.join("sub");
    std::os::unix::fs::symlink(&ws_real, &sub_link).unwrap();

    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::proc::install(&lua, &mare, ws.to_string_lossy().into_owned()).unwrap();
    super::install(&lua, &mare, ws.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let swapped_link = sub_link.clone();
    let swapped_git = ws_git.clone();
    super::lua_api::before_controlled_write(&lua, move |_| {
        std::fs::remove_file(&swapped_link)?;
        std::os::unix::fs::symlink(&swapped_git, &swapped_link)?;
        Ok(())
    });

    let (ok, err): (mlua::Value, Option<String>) = lua
        .load(
            r#"
            mare.proc.sandbox.set_current({ level = 'workspace-write' })
            return mare.exec.run({ tool = 'write' }, function()
                return mare.fs.write('sub/config', 'pwned')
            end)
        "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert!(matches!(ok, mlua::Value::Nil), "expected write to be denied, got: {ok:?}");
    let err = err.expect("expected denial error message");
    assert!(
        err.contains("denied") || err.contains("Permission denied") || err.contains("sandbox"),
        "expected policy denial error, got: {err}"
    );
    assert_eq!(std::fs::read(&git_config).unwrap(), b"original-git-config");
}

#[tokio::test]
async fn controlled_write_symlink_swap_to_missing_mare_does_not_create_dir() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_real = ws.join("real_sub");
    let ws_mare = ws.join(".mare");
    std::fs::create_dir_all(&ws_real).unwrap();

    let sub_link = ws.join("sub");
    std::os::unix::fs::symlink(&ws_real, &sub_link).unwrap();

    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::proc::install(&lua, &mare, ws.to_string_lossy().into_owned()).unwrap();
    super::install(&lua, &mare, ws.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let swapped_link = sub_link.clone();
    let swapped_mare = ws_mare.clone();
    super::lua_api::before_controlled_write(&lua, move |_| {
        std::fs::remove_file(&swapped_link)?;
        std::os::unix::fs::symlink(&swapped_mare, &swapped_link)?;
        Ok(())
    });

    let (ok, err): (mlua::Value, Option<String>) = lua
        .load(
            r#"
            mare.proc.sandbox.set_current({ level = 'workspace-write' })
            return mare.exec.run({ tool = 'write' }, function()
                return mare.fs.write('sub/init.lua', 'pwned')
            end)
        "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert!(matches!(ok, mlua::Value::Nil), "expected write to be denied, got: {ok:?}");
    assert!(err.is_some(), "expected denial error message");
    assert!(!ws_mare.exists(), ".mare directory must not be created");
}

fn workspace_write_policy(ws: &std::path::Path) -> crate::ossandbox::Policy {
    let lua = mlua::Lua::new();
    let spec = lua.create_table().unwrap();
    spec.set("level", "workspace-write").unwrap();
    crate::ossandbox::Policy::from_lua(&lua, &mlua::Value::Table(spec), ws).unwrap().unwrap()
}

#[test]
fn symlink_swap_between_resolve_and_anchor_does_not_mkdir_mare() {
    use super::write::{WriteSeamPoint, set_write_seam};

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let sub = ws.join("sub");
    let ws_mare = ws.join(".mare");
    std::fs::create_dir_all(&sub).unwrap();
    let mut policy = workspace_write_policy(&ws);
    policy.writable = vec![crate::ossandbox::resolve(&ws).unwrap()];

    let swap_sub = sub.clone();
    let swap_mare = ws_mare.clone();
    set_write_seam(WriteSeamPoint::BeforeAnchor, move || {
        std::fs::remove_dir(&swap_sub)?;
        std::os::unix::fs::symlink(&swap_mare, &swap_sub)?;
        Ok(())
    });

    let err = write_file(
        &sub.join("deep/init.lua"),
        b"pwned",
        WriteOptions { policy: Some(&policy), ..Default::default() },
        &None,
    )
    .unwrap_err();
    assert!(!ws_mare.exists(), ".mare directory must not be created on mid-write swap (err: {err:?})");
}

#[test]
fn post_open_policy_recheck_blocks_directory_renamed_to_git() {
    use super::write::{WriteSeamPoint, set_write_seam};

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let sub = ws.join("sub");
    let ws_git = ws.join(".git");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("config"), b"original").unwrap();
    let mut policy = workspace_write_policy(&ws);
    policy.writable = vec![crate::ossandbox::resolve(&ws).unwrap()];

    let from = sub.clone();
    let to = ws_git.clone();
    set_write_seam(WriteSeamPoint::AfterOpenDirBeforeResolve, move || std::fs::rename(&from, &to));

    let err =
        write_file(&sub.join("config"), b"pwned", WriteOptions { policy: Some(&policy), ..Default::default() }, &None)
            .unwrap_err();
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/proc/self/fd").is_dir() {
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "expected PermissionDenied, got: {err:?}");
    } else {
        assert!(
            matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied),
            "expected NotFound or PermissionDenied without /proc/self/fd, got: {err:?}"
        );
    }
    #[cfg(not(target_os = "linux"))]
    assert!(
        matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied),
        "expected NotFound or PermissionDenied on non-Linux, got: {err:?}"
    );
    assert_eq!(std::fs::read(ws_git.join("config")).unwrap(), b"original");
}

#[test]
fn post_check_write_uses_opened_dir_fd_without_following_swapped_symlink() {
    use super::write::{WriteSeamPoint, set_write_seam};

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let sub = ws.join("sub");
    let sub_old = ws.join("sub_old");
    let ws_git = ws.join(".git");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&ws_git).unwrap();
    std::fs::write(ws_git.join("config"), b"original-git").unwrap();
    let mut policy = workspace_write_policy(&ws);
    policy.writable = vec![crate::ossandbox::resolve(&ws).unwrap()];

    // 1. Swap parent dir `sub` to a symlink pointing to `.git` after the post-open policy check:
    //    the write must still go to the already-opened directory fd (`sub_old`), never `.git`.
    let swap_sub = sub.clone();
    let move_to = sub_old.clone();
    let target_git = ws_git.clone();
    set_write_seam(WriteSeamPoint::AfterPostOpenCheckBeforeWrite, move || {
        std::fs::rename(&swap_sub, &move_to)?;
        std::os::unix::fs::symlink(&target_git, &swap_sub)?;
        Ok(())
    });
    write_file(&sub.join("config"), b"safe-write", WriteOptions { policy: Some(&policy), ..Default::default() }, &None)
        .unwrap();
    assert_eq!(std::fs::read(ws_git.join("config")).unwrap(), b"original-git");
    assert_eq!(std::fs::read(sub_old.join("config")).unwrap(), b"safe-write");

    // 2. Swap the leaf file itself to a symlink pointing to `.git/config` after the post-open
    //    policy check: O_NOFOLLOW / RESOLVE_NO_SYMLINKS must refuse with ELOOP.
    let sub2 = ws.join("sub2");
    std::fs::create_dir_all(&sub2).unwrap();
    let leaf = sub2.join("config");
    let leaf_clone = leaf.clone();
    let git_cfg = ws_git.join("config");
    set_write_seam(WriteSeamPoint::AfterPostOpenCheckBeforeWrite, move || {
        std::os::unix::fs::symlink(&git_cfg, &leaf_clone)
    });
    let err =
        write_file(&leaf, b"pwned", WriteOptions { policy: Some(&policy), ..Default::default() }, &None).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "expected ELOOP on leaf symlink swap, got: {err:?}");
    assert_eq!(std::fs::read(ws_git.join("config")).unwrap(), b"original-git");
}

#[test]
fn anchored_mkdir_remove_and_rename_refuse_protected_and_symlink_escapes() {
    use super::write::{anchored_mkdir_all, anchored_remove, anchored_rename};

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    let ws_git = ws.join(".git");
    std::fs::create_dir_all(&ws_git).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(ws_git.join("config"), b"keep").unwrap();
    std::fs::write(outside.join("secret"), b"secret").unwrap();
    let mut policy = workspace_write_policy(&ws);
    policy.writable = vec![crate::ossandbox::resolve(&ws).unwrap()];

    // Valid mkdir/rename/remove inside workspace succeeds.
    anchored_mkdir_all(&ws.join("a/b"), Some(&policy)).unwrap();
    std::fs::write(ws.join("a/b/f.txt"), b"hello").unwrap();
    anchored_rename(&ws.join("a/b/f.txt"), &ws.join("a/g.txt"), Some(&policy)).unwrap();
    assert_eq!(std::fs::read(ws.join("a/g.txt")).unwrap(), b"hello");
    anchored_remove(&ws.join("a"), true, Some(&policy)).unwrap();
    assert!(!ws.join("a").exists());

    // Symlink intermediate directory to outside or .git is refused.
    std::os::unix::fs::symlink(&outside, ws.join("out_link")).unwrap();
    let err = anchored_mkdir_all(&ws.join("out_link/nested"), Some(&policy)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");
    let err = anchored_remove(&ws.join("out_link/secret"), false, Some(&policy)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");
    assert!(outside.join("secret").exists());
    let err = anchored_rename(&ws.join("out_link/secret"), &ws.join("stolen"), Some(&policy)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");

    // Protected `.git` cannot be modified or removed.
    let err = anchored_remove(&ws_git.join("config"), false, Some(&policy)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");
    assert_eq!(std::fs::read(ws_git.join("config")).unwrap(), b"keep");

    // open_regular_nonblocking rejects FIFOs cleanly without blocking.
    let fifo = ws.join("fifo");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let err = super::read::open_regular_nonblocking(&fifo, "fifo").unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
}
