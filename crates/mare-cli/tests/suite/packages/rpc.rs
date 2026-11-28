//! RPC & resource discovery tests for `plugins.{list,install,update,uninstall}`
//! and Lua `mare.plugins.{dirs,manifest,install,update,uninstall}` / `ctx.plugins`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use super::{git, package_repo};
use crate::common::rpc::{PeerClient, RpcClient};
use crate::common::*;

fn spawn_pkg_rpc(home: &Path, extra_args: &[&str], extra_env: &[(&str, String)]) -> RpcClient {
    let mut args = vec!["--rpc"];
    args.extend_from_slice(extra_args);
    let mut env = BTreeMap::from([("MARE_HOME".to_string(), home.to_string_lossy().into_owned())]);
    for (k, v) in extra_env {
        env.insert((*k).to_string(), v.clone());
    }
    let mut client = RpcClient::spawn(&args, &env, home);
    let init = client.call("initialize", json!({ "protocol_version": "3.0" }));
    assert!(init.get("server").is_some(), "{init}");
    client
}

#[test]
fn rpc_pkg_lifecycle_and_resource_discovery() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(cfg.join("plugins")).unwrap();
    std::fs::write(
        cfg.join("plugins/veto.lua"),
        r#"return { name = 'veto',
  config_schema = { mode = { type = 'string', default = 'yes', apply = 'live', desc = 'test refusal' } },
  apply = function(ctx)
    ctx:on('option/apply', function(ev)
      if ev.name == 'plugins.veto.mode' and ev.value == 'no' then error('veto refuses') end
    end)
  end,
}"#,
    )
    .unwrap();

    // Create a git package repo with plugins, custom skills, prompts, themes, types, and mare-plugin.toml.
    let manifest = r#"
name = "demo-pkg"
version = "0.1.0"
description = "Demo git package"
skills = ["custom-skills"]
plugins = ["helper"]
[deps]
required = ["someone/helper"]
optional = ["someone/optional"]
"#;
    let repo = package_repo("demo-plugin", Some(manifest), "v0.1.0");
    for kind in ["custom-skills", "prompts", "themes", "types"] {
        std::fs::create_dir_all(repo.path().join(kind)).unwrap();
        std::fs::write(repo.path().join(kind).join("README.md"), kind).unwrap();
    }
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "add resources"]);

    // Declare the package on `branch = "main"` so `plugins.update` can fast-forward to a new commit.
    let init_lua = cfg.join("init.lua");
    std::fs::write(
        &init_lua,
        format!(
            "mare.o.plugins.timeout = 5\nmare.plugin('plugins', {{ watch_ms = 0 }})\nmare.config_sensitive('plugins.plugins.watch_ms')\nmare.plugins.add {{ {{ 'me/demo', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();

    let mut rpc = spawn_pkg_rpc(home.path(), &[], &[]);
    let sid = rpc.session_id();

    let options = rpc.call("options.get", json!({ "session_id": sid, "keys": ["plugins"] }));
    assert_eq!(options["plugins"]["timeout"], 5, "plugin manager options must remain readable: {options}");
    assert_eq!(
        options["plugins"]["plugins"]["watch_ms"], "[REDACTED]",
        "plugin options must remain present and redacted: {options}"
    );
    rpc.call("options.set", json!({ "session_id": sid, "values": { "plugins.timeout": 6 } }));
    let updated =
        rpc.call("options.get", json!({ "session_id": sid, "keys": ["plugins.timeout", "plugins.plugins.watch_ms"] }));
    assert_eq!(updated["plugins.timeout"], 6, "{updated}");
    assert_eq!(updated["plugins.plugins.watch_ms"], "[REDACTED]", "{updated}");
    let invalid = rpc.call_err(
        "options.set",
        json!({ "session_id": sid, "values": { "plugins.timeout": "invalid", "plugins.concurrency": 3 } }),
    );
    assert_eq!(invalid["code"], -32602, "{invalid}");
    let unchanged =
        rpc.call("options.get", json!({ "session_id": sid, "keys": ["plugins.timeout", "plugins.concurrency"] }));
    assert_eq!(unchanged["plugins.timeout"], 6, "{unchanged}");
    assert_eq!(unchanged["plugins.concurrency"], 8, "the rejected batch must be atomic: {unchanged}");
    let refused = rpc.call_err(
        "options.set",
        json!({ "session_id": sid, "values": { "plugins.timeout": 9, "plugins.veto.mode": "no" } }),
    );
    assert!(refused["message"].as_str().unwrap_or_default().contains("veto refuses"), "{refused}");
    let rolled_back =
        rpc.call("options.get", json!({ "session_id": sid, "keys": ["plugins.timeout", "plugins.veto.mode"] }));
    assert_eq!(rolled_back["plugins.timeout"], 6, "{rolled_back}");
    assert_eq!(rolled_back["plugins.veto.mode"], "yes", "{rolled_back}");

    // 1. `plugins.list` lists installed package, commit, parsed `mare-plugin.toml` manifest, and roots.
    let status = rpc.call("plugins.list", json!({}));
    let pkgs = status["data"].as_array().unwrap();
    assert_eq!(pkgs.len(), 1, "{status}");
    assert_eq!(pkgs[0]["name"], "me/demo");
    assert_eq!(pkgs[0]["scope"], "init.lua");
    assert_eq!(pkgs[0]["installed"], true);
    assert_eq!(pkgs[0]["manifest"]["description"], "Demo git package");
    assert_eq!(pkgs[0]["manifest"]["version"], "0.1.0");
    assert_eq!(pkgs[0]["manifest"]["skills"], json!(["custom-skills"]));
    assert_eq!(pkgs[0]["manifest"]["plugins"], json!(["helper"]));
    assert_eq!(pkgs[0]["manifest"]["deps"]["required"], json!(["someone/helper"]));
    assert_eq!(pkgs[0]["manifest"]["deps"]["optional"], json!(["someone/optional"]));
    let checkout_dir = pkgs[0]["dir"].as_str().unwrap().to_string();
    let first_commit = pkgs[0]["commit"].as_str().unwrap().to_string();

    let roots = status["dirs"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "{status}");
    assert_eq!(roots[0]["name"], "me/demo");

    // 2. `plugins.list` queries each resource kind (including custom skills), and rejects unknown kind / extra param with -32602.
    for (kind, suffix) in
        [("skills", "/custom-skills"), ("prompts", "/prompts"), ("themes", "/themes"), ("types", "/types")]
    {
        let res = rpc.call("plugins.list", json!({ "kind": kind }));
        let dirs = res["dirs"].as_array().unwrap();
        assert_eq!(dirs.len(), 1, "{kind}: {res}");
        assert!(dirs[0]["dir"].as_str().unwrap().ends_with(suffix), "{res}");
    }
    let bad = rpc.call_err("plugins.list", json!({ "kind": "bogus" }));
    assert_eq!(bad["code"], -32602, "{bad}");
    assert_eq!(bad["data"]["code"], "invalid_arguments", "{bad}");
    let removed = rpc.call_err("plugins.list", json!({ "kind": "extensions" }));
    assert_eq!(removed["code"], -32602, "{removed}");
    let extra_param = rpc.call_err("plugins.list", json!({ "bogus": true }));
    assert_eq!(extra_param["code"], -32602, "{extra_param}");

    // Old `packages.*` methods are gone.
    let legacy = rpc.call_err("packages.status", json!({}));
    assert_eq!(legacy["code"], -32601, "{legacy}");
    let retired_remove = rpc.call_err("plugins.remove", json!({ "name": "me/demo" }));
    assert_eq!(retired_remove["code"], -32601, "{retired_remove}");

    // 3. Push a new commit upstream, then call `plugins.update` and verify HEAD advances and `{log}` is returned.
    std::fs::write(repo.path().join("UPDATED"), "v2").unwrap();
    git(repo.path(), &["add", "UPDATED"]);
    git(repo.path(), &["commit", "-q", "-m", "update v2"]);
    let upd = rpc.call("plugins.update", json!({ "name": "me/demo" }));
    assert!(upd["log"].is_array(), "{upd}");
    assert!(upd.get("ok").is_none(), "contract drops legacy `ok` field: {upd}");
    let status_after = rpc.call("plugins.list", json!({}));
    let second_commit = status_after["data"][0]["commit"].as_str().unwrap().to_string();
    assert_ne!(first_commit, second_commit);

    // 4. Active enabled package cannot be removed by `plugins.uninstall`; failure is a coded error `plugins.failed` with `data.log`.
    let rem_active = rpc.call_err("plugins.uninstall", json!({ "name": "me/demo" }));
    assert_eq!(rem_active["code"], -32000, "{rem_active}");
    assert_eq!(rem_active["data"]["code"], "plugins.failed", "{rem_active}");
    assert!(rem_active["data"]["data"]["log"].is_array(), "{rem_active}");

    // 5. Disable package and reload, then `plugins.uninstall` cleans the checkout cleanly.
    std::fs::write(
        &init_lua,
        format!(
            "mare.plugins.add {{ {{ 'me/demo', url = {:?}, branch = 'main', enabled = false }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();
    rpc.shutdown_and_wait();

    let mut rpc2 = spawn_pkg_rpc(home.path(), &[], &[]);
    let rem = rpc2.call("plugins.uninstall", json!({ "name": "me/demo" }));
    assert!(rem["log"].is_array(), "{rem}");
    assert!(rem.get("ok").is_none(), "{rem}");
    let after_rem = rpc2.call("plugins.list", json!({}));
    assert_eq!(after_rem["data"][0]["installed"], false, "{after_rem}");
    assert_eq!(after_rem["data"][0]["disabled"], true, "{after_rem}");
    assert!(!Path::new(&checkout_dir).exists(), "checkout should be removed from disk");
    rpc2.shutdown_and_wait();

    // 6. Re-enable in init.lua, restart with `--clean` to confirm `plugins.list` `dirs` is empty under `--clean`.
    std::fs::write(
        &init_lua,
        format!("mare.plugins.add {{ {{ 'me/demo', url = {:?}, branch = 'main' }} }}\n", repo.path().to_string_lossy()),
    )
    .unwrap();
    let mut rpc_np = spawn_pkg_rpc(home.path(), &["--clean"], &[]);
    let dirs_np = rpc_np.call("plugins.list", json!({}));
    assert_eq!(dirs_np["dirs"], json!([]), "{dirs_np}");
    rpc_np.shutdown_and_wait();

    // 7. Start enabled RPC, delete the checkout on disk, and verify `plugins.install` restores it and reloads.
    let mut rpc3 = spawn_pkg_rpc(home.path(), &[], &[]);
    std::fs::remove_dir_all(&checkout_dir).unwrap();
    assert!(!Path::new(&checkout_dir).exists());

    let missing_status = rpc3.call("plugins.list", json!({}));
    assert_eq!(missing_status["data"][0]["installed"], false, "{missing_status}");
    let missing_dirs = rpc3.call("plugins.list", json!({ "kind": "skills" }));
    assert_eq!(missing_dirs["dirs"], json!([]), "{missing_dirs}");

    let inst = rpc3.call("plugins.install", json!({ "name": "me/demo", "reload": true }));
    assert!(inst["log"].is_array(), "{inst}");
    assert!(inst["reload"].is_object(), "{inst}");
    assert!(Path::new(&checkout_dir).join("plugins/demo-plugin/init.lua").is_file());

    let restored_status = rpc3.call("plugins.list", json!({}));
    assert_eq!(restored_status["data"][0]["installed"], true, "{restored_status}");
    assert_eq!(restored_status["data"][0]["disabled"], false, "{restored_status}");
    assert_eq!(restored_status["data"][0]["manifest"]["version"], "0.1.0", "{restored_status}");

    let restored_dirs = rpc3.call("plugins.list", json!({ "kind": "skills" }));
    let dirs_arr = restored_dirs["dirs"].as_array().unwrap();
    assert_eq!(dirs_arr.len(), 1, "{restored_dirs}");
    assert!(dirs_arr[0]["dir"].as_str().unwrap().ends_with("/custom-skills"), "{restored_dirs}");

    rpc3.shutdown_and_wait();
}

#[test]
fn rpc_update_forwards_stash_option_to_plugin_manager() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let repo = package_repo("rpc-stash", None, "v1.0.0");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "mare.plugins.add {{ {{ 'me/rpc-stash', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();
    let env = [
        ("GIT_AUTHOR_NAME", "Test".into()),
        ("GIT_AUTHOR_EMAIL", "test@example.invalid".into()),
        ("GIT_COMMITTER_NAME", "Test".into()),
        ("GIT_COMMITTER_EMAIL", "test@example.invalid".into()),
    ];
    let mut rpc = spawn_pkg_rpc(home.path(), &[], &env);
    let status = rpc.call("plugins.list", json!({}));
    let dir = Path::new(status["data"][0]["dir"].as_str().unwrap());
    let initial = status["data"][0]["commit"].as_str().unwrap().to_string();
    std::fs::write(dir.join("notes.txt"), "local notes\n").unwrap();
    std::fs::write(repo.path().join("new"), "updated\n").unwrap();
    git(repo.path(), &["add", "new"]);
    git(repo.path(), &["commit", "-q", "-m", "updated"]);
    let denied = rpc.call_err("plugins.update", json!({ "name": "me/rpc-stash" }));
    assert_eq!(denied["data"]["code"], "plugins.failed", "{denied}");
    let updated = rpc.call("plugins.update", json!({ "name": "me/rpc-stash", "stash": true }));
    assert!(updated["log"].to_string().contains("stash"), "{updated}");
    let after = rpc.call("plugins.list", json!({}));
    assert_ne!(after["data"][0]["commit"].as_str().unwrap(), initial);
    let checked = rpc.call("plugins.check", json!({ "names": ["me/rpc-stash"] }));
    assert_eq!(checked["data"][0]["status"], "up_to_date", "{checked}");
    let saved = fixture_git(dir).args(["show", "stash@{0}^3:notes.txt"]).output().unwrap();
    assert!(saved.status.success(), "{}", String::from_utf8_lossy(&saved.stderr));
    assert_eq!(String::from_utf8_lossy(&saved.stdout), "local notes\n");
    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn lua_pkg_dirs_manifest_and_install_update_uninstall() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo(
            "lua-pkg-probe",
            Some("name = 'lua-pkg-probe'\nversion = '1.2.3'\ndescription = 'From toml'\n"),
            "v1.0.0",
        );
        std::fs::create_dir_all(repo.path().join("skills")).unwrap();
        std::fs::write(repo.path().join("skills").join("README.md"), "skills").unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "skills"]);

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{ {{ 'me/probe', url = {:?}, branch = 'main' }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let mut b = env.host().user_config(false).no_session();
        b.opts.config = json!({ "model": "faux/echo" });
        let h = mare_host::Host::new(b.opts).await.unwrap();

        let (root_count, skill_count, desc, version): (usize, usize, String, String) = eval(
            &h,
            r#"
            local roots = mare.plugins.dirs()
            local skills = mare.plugins.dirs("skills")
            local m = mare.plugins.manifest("me/probe")
            return #roots, #skills, m.description, m.version
            "#,
        )
        .await;
        assert_eq!(root_count, 1);
        assert_eq!(skill_count, 1);
        assert_eq!(desc, "From toml");
        assert_eq!(version, "1.2.3");

        let ok_upd: bool = eval(&h, "return mare.plugins.update('me/probe').ok").await;
        assert!(ok_upd);
        let ok_rem: bool = eval(&h, "return mare.plugins.uninstall('me/probe').ok").await;
        assert!(!ok_rem, "enabled package should refuse clean");
        assert!(eval::<bool>(&h, "return mare.plugins.remove == nil").await, "retired remove alias must be absent");

        // Contract service calls on `ctx.plugins`: list, update, and coded error on failed uninstall.
        global(&h, "PKG", h.service_table("plugins").unwrap());
        let (svc_pkgs, svc_dirs, svc_upd_log, rem_err_code): (usize, usize, bool, String) = eval(
            &h,
            r#"
            local listed = PKG:list("skills")
            local upd = PKG:update("me/probe")
            local ok, err = pcall(function() return PKG:uninstall("me/probe") end)
            assert(not ok, "expected plugins:uninstall of active package to raise")
            return #listed.data, #listed.dirs, type(upd.log) == "table", err.code
            "#,
        )
        .await;
        assert_eq!(svc_pkgs, 1);
        assert_eq!(svc_dirs, 1);
        assert!(svc_upd_log);
        assert_eq!(rem_err_code, "plugins.failed");

        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_pkg_update_cancels_lock_wait_on_cancel_and_disconnect() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    let repo = package_repo("cancel-demo", Some("name = 'cancel-demo'\nversion = '0.1.0'\n"), "v0.1.0");
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "mare.plugins.add {{ {{ 'me/cancel-demo', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();

    let mut rpc = spawn_pkg_rpc(home.path(), &[], &[]);

    // Hold the cross-process lockfile guard while issuing `plugins.update`, then cancel it.
    let lock_path = cfg.join("mare-lock.json.lock");
    let lock_file =
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path).unwrap();
    lock_file.try_lock().unwrap();

    let id = rpc.request("plugins.update", json!({ "name": "me/cancel-demo" }));
    rpc.notify("$/cancel_request", json!({ "id": id }));
    let cancelled = rpc.wait(id).expect_err("expected cancel error");
    assert_eq!(cancelled["code"], -32800, "{cancelled}");
    assert_eq!(cancelled["data"]["code"], "cancelled", "{cancelled}");

    // Release the lock and verify a subsequent `plugins.update` succeeds.
    lock_file.unlock().unwrap();
    drop(lock_file);

    let ok = rpc.call("plugins.update", json!({ "name": "me/cancel-demo" }));
    assert!(ok["log"].is_array(), "{ok}");

    // Re-acquire the lock, issue `plugins.update`, and close stdin (disconnect).
    // The server must cancel the blocked package operation and exit instead of deadlocking.
    let lock_file2 =
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path).unwrap();
    lock_file2.try_lock().unwrap();

    let _ = rpc.request("plugins.update", json!({ "name": "me/cancel-demo" }));
    rpc.wait_exit();
    lock_file2.unlock().unwrap();
}

/// `plugins.list`, `plugins.update`, `plugins.install`, and `plugins.uninstall` report clear
/// errors under a corrupt lockfile without touching disk, and `--clean` returns empty `packages`/`dirs`/`flags`.
/// Ref: Kimi Code `packages/node-sdk/test/config.test.ts:34-59`; `packages/kap-server/test/plugins.test.ts:563-571`.
#[test]
fn rpc_pkg_corrupt_lock_and_no_plugins_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    let repo = package_repo("rpc-edge", Some("name = 'rpc-edge'\nversion = '0.1.0'\n"), "v0.1.0");
    std::fs::create_dir_all(repo.path().join("skills")).unwrap();
    std::fs::write(repo.path().join("skills/README.md"), "ok").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "skills"]);

    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "mare.flags.register('pkg-probe', {{ type = 'string', default = 'x' }})\nmare.plugins.add {{ {{ 'me/rpc-edge', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();

    // 1. Bootstrap installation cleanly so checkout and lock exist, then shut down.
    let mut sync_cmd = mare_cmd();
    sync_cmd
        .args(["plugin", "sync"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(std::process::Stdio::null());
    let sync = run_output(&mut sync_cmd);
    assert!(sync.status.success(), "{}", String::from_utf8_lossy(&sync.stderr));
    let lock_path = cfg.join("mare-lock.json");
    let valid_lock = std::fs::read(&lock_path).unwrap();
    let checkout_dir = super::checkout_dir_for(home.path(), &repo.path().to_string_lossy());
    assert!(checkout_dir.join("plugins/rpc-edge/init.lua").is_file());

    // Advance upstream so `plugins.update` would otherwise change HEAD if it ignored the lock.
    std::fs::write(repo.path().join("NEXT"), "v2").unwrap();
    git(repo.path(), &["add", "NEXT"]);
    git(repo.path(), &["commit", "-q", "-m", "v2"]);

    // 2. Corrupt lockfile on disk before starting RPC.
    let corrupt_bytes = b"{broken-lock";
    std::fs::write(&lock_path, corrupt_bytes).unwrap();

    let mut rpc = spawn_pkg_rpc(home.path(), &[], &[]);
    let st = rpc.call_err("plugins.list", json!({}));
    assert_eq!(st["code"], -32603, "{st}");

    for method in ["plugins.update", "plugins.install", "plugins.uninstall"] {
        let res = rpc.call_err(method, json!({ "name": "me/rpc-edge" }));
        assert_eq!(res["data"]["code"], "plugins.failed", "{method} must fail on corrupt lock: {res}");
        assert!(res["message"].as_str().unwrap_or("").contains("corrupt"), "{method}: {res}");
    }
    assert_eq!(std::fs::read(&lock_path).unwrap(), corrupt_bytes, "corrupt lockfile untouched");
    assert!(!checkout_dir.join("NEXT").exists(), "checkout untouched on corrupt lock");
    rpc.shutdown_and_wait();

    // 3. Restore valid lock, start `--rpc --clean`: `plugins.list` and `flags.*` stay empty.
    std::fs::write(&lock_path, valid_lock).unwrap();
    let mut rpc_np = spawn_pkg_rpc(home.path(), &["--clean"], &[]);
    let st_np = rpc_np.call("plugins.list", json!({}));
    assert_eq!(st_np["data"], json!([]), "{st_np}");
    assert_eq!(st_np["dirs"], json!([]), "{st_np}");
    let dirs_np = rpc_np.call("plugins.list", json!({ "kind": "skills" }));
    assert_eq!(dirs_np["dirs"], json!([]), "{dirs_np}");
    let fl_np = rpc_np.call("flags.list", json!({}));
    assert_eq!(fl_np["data"], json!([]), "{fl_np}");
    let fg_np = rpc_np.call("flags.get", json!({ "name": "pkg-probe" }));
    assert_eq!(fg_np["registered"], false, "{fg_np}");
    rpc_np.shutdown_and_wait();
}

/// SPEC D16: `plugins.install`, `plugins.update`, and `plugins.uninstall` are `primary`-only and cannot be
/// granted to a peer even under wildcard `peer.allow = { "plugins.*", "*" }`; only `plugins.list` is `peer`.
/// Ref: reference implementation.
#[test]
fn rpc_pkg_peer_access_requires_grant() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();

    let env = BTreeMap::from([("MARE_HOME".to_string(), home.path().to_string_lossy().into_owned())]);
    let mut primary = RpcClient::spawn(&["--rpc", "--peer", "--no-session"], &env, home.path());
    let initialized = primary.call("initialize", json!({ "protocol_version": "3.0" }));
    let advertised = initialized["capabilities"]["methods"].as_array().unwrap();
    for m in ["plugins.list", "plugins.install", "plugins.update", "plugins.uninstall"] {
        assert!(advertised.iter().any(|x| x == m), "{m} not advertised to primary in {initialized}");
    }

    {
        let mut peer = PeerClient::connect(home.path(), primary.pid());
        let peer_init = peer.call("initialize", json!({ "protocol_version": "3.0" }));
        let peer_methods = peer_init["result"]["capabilities"]["methods"].as_array().unwrap();
        assert!(!peer_methods.iter().any(|x| x == "plugins.list"), "{peer_init}");

        for m in ["plugins.list", "plugins.install", "plugins.update", "plugins.uninstall"] {
            let denied = peer.call(m, json!({}));
            assert_eq!(denied["error"]["data"]["code"], "denied", "default peer {m}: {denied}");
        }
    }
    primary.shutdown_and_wait();

    // Exact grants open `plugins.list`, while mutation methods (`primary`) stay closed even under `*` or explicit allow.
    std::fs::write(
        cfg.join("init.lua"),
        "mare.setup { model = 'faux/echo', peer = { allow = { 'plugins.list', 'plugins.install', 'plugins.update', 'plugins.remove', 'options.set', '*' } } }\n",
    )
    .unwrap();
    let mut granted_primary = RpcClient::spawn(&["--rpc", "--peer", "--no-session"], &env, home.path());
    let _ = granted_primary.call("initialize", json!({ "protocol_version": "3.0" }));

    {
        let mut peer = PeerClient::connect(home.path(), granted_primary.pid());
        let peer_init = peer.call("initialize", json!({ "protocol_version": "3.0" }));
        let peer_methods = peer_init["result"]["capabilities"]["methods"].as_array().unwrap();
        assert!(peer_methods.iter().any(|x| x == "plugins.list"), "{peer_init}");
        for m in ["plugins.install", "plugins.update", "plugins.uninstall"] {
            assert!(!peer_methods.iter().any(|x| x == m), "peer must never see {m}: {peer_init}");
        }

        let allowed_list = peer.call("plugins.list", json!({}));
        assert!(allowed_list["result"]["data"].is_array(), "{allowed_list}");
        let manager_denied = peer.call("options.set", json!({ "values": { "plugins.timeout": 4 } }));
        assert_eq!(manager_denied["error"]["data"]["code"], "method_not_found", "{manager_denied}");
        for m in ["plugins.install", "plugins.update", "plugins.uninstall"] {
            let still_denied = peer.call(m, json!({}));
            assert_eq!(still_denied["error"]["data"]["code"], "denied", "wildcard peer {m}: {still_denied}");
        }
    }
    granted_primary.shutdown_and_wait();
}

/// Credentialed URLs and git stderr auth strings are redacted before reaching `plugins.list[].url`,
/// `plugins.failed` error messages, and `plugins.failed.data.log`.
/// Ref: reference implementation.
#[test]
fn rpc_pkg_redacts_url_credentials_and_logs() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    let secret_user = "alice-cred-user";
    let secret_token = "ghp_secretTokenValue1234567890";
    let secret_url = format!("https://{secret_user}:{secret_token}@127.0.0.1:1/me/secret.git");
    std::fs::write(cfg.join("init.lua"), format!("mare.plugins.add {{ {{ 'me/secret', url = {secret_url:?} }} }}\n"))
        .unwrap();

    // Force immediate connection refusal with zero DNS/network dependency.
    let mut rpc = spawn_pkg_rpc(
        home.path(),
        &[],
        &[
            ("GIT_CONFIG_COUNT", "1".to_string()),
            ("GIT_CONFIG_KEY_0", "http.proxy".to_string()),
            ("GIT_CONFIG_VALUE_0", "http://127.0.0.1:1".to_string()),
        ],
    );

    let listed = rpc.call("plugins.list", json!({}));
    let url = listed["data"][0]["url"].as_str().unwrap();
    assert!(url.contains("[REDACTED]"), "expected redacted userinfo in plugins.list url: {url}");
    assert!(!url.contains(secret_user) && !url.contains(secret_token), "secret leaked in plugins.list: {url}");

    let failed = rpc.call_err("plugins.install", json!({ "name": "me/secret" }));
    assert_eq!(failed["data"]["code"], "plugins.failed", "{failed}");
    let dumped = failed.to_string();
    assert!(dumped.contains("[REDACTED]"), "expected redaction marker in plugins.failed payload: {dumped}");
    assert!(
        !dumped.contains(secret_user) && !dumped.contains(secret_token),
        "secret leaked in plugins.failed: {dumped}"
    );

    rpc.shutdown_and_wait();
}

/// Remote failure keeps checkout and lockfile clean and returns `plugins.failed` with `data.log`; when
/// `reload_gate` is busy during `plugins.install { reload = true }`, disk install succeeds and
/// `plugins.failed` carries both the retry reason and the install log.
/// Ref: reference implementation.
#[test]
fn rpc_pkg_reload_gate_and_remote_failure_keep_disk_consistent() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    // 1. Remote failure during initial install must return `plugins.failed` with non-empty `data.log` and no lockfile/checkout.
    let missing_repo = home.path().join("missing-remote");
    let bad_url = format!("file://{}", missing_repo.display());
    std::fs::write(cfg.join("init.lua"), format!("mare.plugins.add {{ {{ 'me/missing', url = {bad_url:?} }} }}\n"))
        .unwrap();

    let mut rpc_bad = spawn_pkg_rpc(home.path(), &[], &[]);
    let bad_res = rpc_bad.call_err("plugins.install", json!({ "name": "me/missing" }));
    assert_eq!(bad_res["data"]["code"], "plugins.failed", "{bad_res}");
    assert!(bad_res["data"]["data"]["log"].as_array().is_some_and(|l| !l.is_empty()), "{bad_res}");
    let lock_after_bad = std::fs::read_to_string(cfg.join("mare-lock.json")).unwrap_or_default();
    assert!(
        !lock_after_bad.contains("me/missing"),
        "failed install must not record package in mare-lock.json: {lock_after_bad}"
    );
    assert!(!super::checkout_dir_for(home.path(), &bad_url).exists(), "failed install must leave checkout absent");
    rpc_bad.shutdown_and_wait();

    // Update failure against an already-locked package leaves checkout and lockfile bytes identical.
    let mut bad_upd_repo = Some(package_repo("upd-fail", Some("name = 'upd-fail'\nversion = '0.1.0'\n"), "v0.1.0"));
    let upd_url = bad_upd_repo.as_ref().unwrap().path().to_string_lossy().into_owned();
    std::fs::write(
        cfg.join("init.lua"),
        format!("mare.plugins.add {{ {{ 'me/upd-fail', url = {:?}, branch = 'main' }} }}\n", upd_url),
    )
    .unwrap();
    let mut rpc_upd = spawn_pkg_rpc(home.path(), &[], &[]);
    let lock_before_upd = std::fs::read(cfg.join("mare-lock.json")).unwrap();
    let upd_checkout = super::checkout_dir_for(home.path(), &upd_url);
    assert!(upd_checkout.join("plugins/upd-fail/init.lua").is_file());
    // Delete upstream repo so `plugins.update` fails remotely, then verify rollback preserves lockfile & checkout.
    drop(bad_upd_repo.take());
    let upd_err = rpc_upd.call_err("plugins.update", json!({ "name": "me/upd-fail" }));
    assert_eq!(upd_err["data"]["code"], "plugins.failed", "{upd_err}");
    assert!(upd_err["data"]["data"]["log"].as_array().is_some_and(|l| !l.is_empty()), "{upd_err}");
    assert_eq!(
        std::fs::read(cfg.join("mare-lock.json")).unwrap(),
        lock_before_upd,
        "failed update must preserve lockfile"
    );
    assert!(upd_checkout.join("plugins/upd-fail/init.lua").is_file(), "failed update must preserve existing checkout");
    rpc_upd.shutdown_and_wait();

    // 2. Busy `reload_gate` (active model request) on `plugins.install { reload = true }`:
    // disk install succeeds, reload returns `plugins.failed` with retry message AND existing install log.
    let repo = package_repo("gate-demo", Some("name = 'gate-demo'\nversion = '0.1.0'\n"), "v0.1.0");
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ 'me/gate', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();
    let checkout_dir = super::checkout_dir_for(home.path(), &repo.path().to_string_lossy());

    let mut rpc = spawn_pkg_rpc(home.path(), &["--no-session"], &[]);
    let _ = rpc.call(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false } }),
    );
    let _ = rpc.call(
        "llm.register_provider",
        json!({
            "provider": "gateprov",
            "default_model": "g1",
            "models": [{ "model_id": "g1", "name": "Gate 1", "context_window": 128000, "max_output": 2048, "input": ["text"] }],
            "callbacks": ["stream"]
        }),
    );
    let sid = rpc.call("status.get", json!({}))["session"]["session_id"].as_str().unwrap().to_owned();
    let _ = rpc.call("llm.set_model", json!({ "session_id": sid, "ref": "gateprov/g1" }));
    let _ = rpc.call("events.subscribe", json!({ "session_id": sid, "include": ["turn/started", "turn/completed"] }));
    // Remove checkout after startup so `plugins.install` performs a real disk install while the request holds `reload_gate`.
    if checkout_dir.exists() {
        std::fs::remove_dir_all(&checkout_dir).unwrap();
    }
    let started = rpc.call("agent.prompt", json!({ "session_id": sid, "input": "hold reload gate" }));
    assert_eq!(started["status"], "started", "{started}");
    let gate_cb_id = loop {
        let msg = rpc.read();
        if msg["method"] == "callback.call" && msg["params"]["callback"] == "stream" {
            break msg["id"].clone();
        }
        assert_ne!(
            msg.pointer("/params/type").and_then(serde_json::Value::as_str),
            Some("turn/completed"),
            "turn completed before stream callback arrived: {msg}"
        );
        rpc.dispatch(msg);
    };

    let busy_err = rpc.call_err("plugins.install", json!({ "name": "me/gate", "reload": true }));
    assert_eq!(busy_err["data"]["code"], "plugins.failed", "{busy_err}");
    assert!(
        busy_err["message"].as_str().unwrap_or("").contains("retry afterwards"),
        "expected retryable reload gate message: {busy_err}"
    );
    assert!(
        busy_err["data"]["data"]["log"].as_array().is_some_and(|l| !l.is_empty()),
        "install log must be preserved when reload fails: {busy_err}"
    );
    assert!(
        checkout_dir.join("plugins/gate-demo/init.lua").is_file(),
        "disk install must remain intact even when reload fails"
    );
    rpc.send(json!({
        "id": gate_cb_id,
        "result": { "role": "assistant", "content": [{ "type": "text", "text": "busy turn" }] }
    }));
    let _ = rpc.wait_event("turn/completed");

    // Once the turn finishes, retrying `plugins.install { reload = true }` publishes cleanly.
    let recovered = rpc.call("plugins.install", json!({ "name": "me/gate", "reload": true }));
    assert!(recovered["reload"].is_object(), "{recovered}");
    rpc.shutdown_and_wait();
}
