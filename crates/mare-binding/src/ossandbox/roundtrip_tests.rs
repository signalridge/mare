use super::*;
use mlua::{Lua, Value};
use std::path::Path;

/// A normalized policy is also fed back into intersections and must retain every restriction.
#[cfg(target_os = "linux")]
#[test]
fn normalized_policy_preserves_replaced_write_roots_and_pinned_reads() {
    let lua = Lua::new();
    let input: Value = lua
        .load(
            r#"return { level = "workspace-write", cwd = "/usr",
        write = { "/usr/share" }, network = true }"#,
        )
        .eval()
        .unwrap();
    let mut policy = Policy::from_lua(&lua, &input, Path::new("/usr")).unwrap().unwrap();
    policy.restrict_read = true;
    policy.reads_pinned = true;
    policy.readable = vec![PathBuf::from("/usr/share")];
    policy.profile = Some("(version 1)(allow default)".into());
    let output = Value::Table(policy.to_lua(&lua).unwrap());
    let restored = Policy::from_lua(&lua, &output, Path::new("/usr")).unwrap().unwrap();
    assert!(!restored.root_writable, "the workspace root was silently re-granted");
    assert_eq!(restored.read_roots(), policy.read_roots(), "the pinned read roots changed");
    assert_eq!(restored.profile, policy.profile, "the session-level profile disappeared");
    assert!(!restored.can_write(Path::new("/usr/mare-roundtrip-probe")));
    assert!(restored.can_write(Path::new("/usr/share/mare-roundtrip-probe")));
}

#[test]
fn temp_exclusions_roundtrip_and_intersect_exactly_under_a_default_temp_root() {
    let lua = Lua::new();
    let fixture = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(fixture.path()).unwrap();
    let allowed = root.join("allowed");
    let sibling = root.join("sibling");
    let elsewhere = root.join("elsewhere");
    for dir in [&allowed, &sibling, &elsewhere] {
        std::fs::create_dir(dir).unwrap();
    }
    let make = |spec| {
        Policy::from_lua(&lua, &mare_shared::lua::json_to_lua(&lua, &spec).unwrap(), &elsewhere).unwrap().unwrap()
    };
    let narrow = make(serde_json::json!({"level":"workspace-write", "write":[allowed], "network":false,
        "exclude_tmpdir_env_var":true,"exclude_slash_tmp":true}));
    assert!(narrow.can_write(&allowed.join("new")));
    assert!(!narrow.can_write(&sibling.join("new")));
    let restored = Policy::from_lua(&lua, &Value::Table(narrow.to_lua(&lua).unwrap()), &elsewhere).unwrap().unwrap();
    assert!(restored.exclude_tmpdir_env_var && restored.exclude_slash_tmp);
    assert_eq!(restored.writable, narrow.writable);
    let broad = make(serde_json::json!({"level":"workspace-write", "network":true}));
    assert!(broad.can_write(&allowed.join("new")), "default temp roots cover this fixture");
    for intersection in [broad.intersect(&narrow), narrow.intersect(&broad)] {
        assert!(intersection.exclude_tmpdir_env_var && intersection.exclude_slash_tmp);
        assert!(intersection.can_write(&allowed.join("new")));
        assert!(!intersection.can_write(&sibling.join("new")));
        assert!(matches!(intersection.network, Network::Proxy(_)));
    }
    let grants = narrow.landlock_grants();
    assert!(grants.rw.iter().any(|path| path == &allowed));
    let base = make(serde_json::json!({"level":"read-only", "network":false})).landlock_grants();
    assert!(
        grants.rw.iter().all(|path| path == &allowed || base.rw.contains(path)),
        "only the requested root and pre-existing device/IPC exceptions may remain writable"
    );
    let bwrap = narrow.bwrap_args(&elsewhere);
    assert!(!bwrap.windows(2).any(|args| args == ["--tmpfs", "/tmp"]));
    let default_bwrap = broad.bwrap_args(&elsewhere);
    assert!(default_bwrap.windows(2).any(|args| args == ["--tmpfs", "/tmp"]));
}
