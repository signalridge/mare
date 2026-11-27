use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn tool_cannot_write_outside_the_workspace_by_any_route() {
    local(async {
        let env = TestEnv::new().await;
        // Keep the probe outside the workspace and default temp roots even when cargo runs
        // from /tmp. The per-user home is not a default workspace-write root.
        let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME is set for tests"));
        let outside = tempfile::tempdir_in(home).unwrap();
        let out = dunce::canonicalize(outside.path()).unwrap();
        let script = json!([{ "tool_calls": [{ "name": "sneaky", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "OUT", out.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current("workspace-write")
            T:register("sneaky", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local log = {}
                    local function try(name, f) local ok, e = pcall(f) log[#log + 1] = name .. "=" .. tostring(ok) .. ":" .. tostring(e) end
                    try("direct", function() return mare.fs.write(OUT .. "/a", "x") end)
                    try("task", function() return mare.task.spawn(function() return mare.fs.write(OUT .. "/b", "x") end):wait() end)
                    try("coroutine", function() return coroutine.wrap(function() return mare.fs.write(OUT .. "/c", "x") end)() end)
                    try("io", function() return io.open(OUT .. "/d", "w") end)
                    try("timeout", function() return mare.task.timeout(1000, function() return mare.fs.write(OUT .. "/e", "x") end) end)
                    try("inside", function() return mare.fs.write("ok.txt", "fine") end)
                    return table.concat(log, "\n")
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        for f in ["a", "b", "c", "d", "e"] {
            assert!(!out.join(f).exists(), "{f} written outside the workspace: {dump}");
        }
        assert!(env.cwd.join("ok.txt").exists(), "writes inside the workspace must work: {dump}");
        assert!(dump.contains("[sandbox: file access denied under workspace-write mode]"), "{dump}");
        // outside a tool call the same primitive is unrestricted (plugins are trusted)
        let ok: bool = eval(&h, "return mare.fs.write(OUT .. '/plugin', 'x') == true").await;
        assert!(ok && out.join("plugin").exists());
        exec(&h, "mare.proc.sandbox.set_current(nil)").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn io_update_modes_require_read_and_write_before_opening() {
    local(async {
        let env = TestEnv::new().await;
        let outside_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let outside = dunce::canonicalize(outside_dir.path()).unwrap();
        let secret = outside.join("secret");
        std::fs::write(&secret, "secret").unwrap();
        let script = json!([{ "tool_calls": [{ "name": "io_modes", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "OUT", outside.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write", writable = { OUT },
                restrict_read = true, reads_pinned = true, readable = { "." } })
            T:register("io_modes", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local results = {}
                    for _, mode in ipairs({ "r", "r+", "w+", "a+" }) do
                        local ok = pcall(io.open, OUT .. "/secret", mode)
                        results[#results + 1] = mode .. "=" .. tostring(ok)
                    end
                    return table.concat(results, ",")
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        for mode in ["r", "r+", "w+", "a+"] {
            assert!(dump.contains(&format!("{mode}=false")), "{dump}");
        }
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret", "w+ must not truncate before denial");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn image_load_obeys_tool_read_policy() {
    local(async {
        let env = TestEnv::new().await;
        let outside_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let outside = dunce::canonicalize(outside_dir.path()).unwrap();
        let secret = outside.join("secret.png");
        let inside = env.cwd.join("inside.png");
        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        pixel.save(&secret).unwrap();
        pixel.save(&inside).unwrap();
        let script = json!([{ "tool_calls": [{ "name": "image_policy", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "OUT", secret.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({
                level = "workspace-write",
                restrict_read = true,
                exclude_tmpdir_env_var = true,
                exclude_slash_tmp = true,
            })
            T:register("image_policy", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local denied = pcall(mare.image.load, OUT)
                    local allowed, img = pcall(mare.image.load, "inside.png")
                    return "denied=" .. tostring(denied) .. ",allowed=" .. tostring(allowed and img and img.width == 1)
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        assert!(dump.contains("denied=false,allowed=true"), "{dump}");
        let trusted: bool = eval(&h, "local img = mare.image.load(OUT) return img and img.width == 1").await;
        assert!(trusted, "calls outside a tool context remain unrestricted");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn file_metadata_obeys_tool_read_policy() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let secret = dunce::canonicalize(outside.path()).unwrap().join("secret");
        std::fs::write(&secret, "secret").unwrap();
        std::fs::write(env.cwd.join("inside"), "inside").unwrap();
        let script = json!([{ "tool_calls": [{ "name": "metadata_policy", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "OUT", secret.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({
                level = "workspace-write",
                restrict_read = true,
                exclude_tmpdir_env_var = true,
                exclude_slash_tmp = true,
            })
            T:register("metadata_policy", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local results = {}
                    for _, name in ipairs({ "exists", "stat", "identity", "realpath" }) do
                        local denied = pcall(mare.fs[name], OUT)
                        local allowed, value = pcall(mare.fs[name], "inside")
                        results[#results + 1] = name .. "=" .. tostring(denied) .. ":" .. tostring(allowed and value ~= nil)
                    end
                    return table.concat(results, ",")
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        for name in ["exists", "stat", "identity", "realpath"] {
            assert!(dump.contains(&format!("{name}=false:true")), "{dump}");
        }
        let trusted: bool = eval(&h, "return mare.fs.exists(OUT) and mare.fs.stat(OUT).kind == 'file'").await;
        assert!(trusted, "calls outside a tool context remain unrestricted");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn rename_cannot_move_an_unreadable_file_into_a_readable_root() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let outside_path = dunce::canonicalize(outside.path()).unwrap();
        let secret = outside_path.join("secret");
        std::fs::write(&secret, "secret").unwrap();
        std::fs::write(env.cwd.join("inside"), "inside").unwrap();
        let script = json!([{ "tool_calls": [{ "name": "rename_policy", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "OUT", outside_path.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write", writable = { OUT },
                restrict_read = true, reads_pinned = true, readable = { "." } })
            T:register("rename_policy", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local fs = pcall(mare.fs.rename, OUT .. "/secret", "stolen-fs")
                    local std = pcall(os.rename, OUT .. "/secret", "stolen-std")
                    local allowed = pcall(mare.fs.rename, "inside", "renamed")
                    return "fs=" .. tostring(fs) .. ",std=" .. tostring(std) .. ",allowed=" .. tostring(allowed)
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        assert!(dump.contains("fs=false,std=false,allowed=true"), "{dump}");
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
        assert!(!env.cwd.join("stolen-fs").exists() && !env.cwd.join("stolen-std").exists());
        assert_eq!(std::fs::read_to_string(env.cwd.join("renamed")).unwrap(), "inside");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn native_lua_file_apis_check_the_process_cwd() {
    const CHILD_CWD: &str = "MARE_NATIVE_LUA_CWD_CHILD";
    if std::env::var_os(CHILD_CWD).is_none() {
        // Process cwd is global. Re-exec this test alone, with a cwd outside the
        // workspace and default temp grants, even when the caller runs in /tmp.
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).expect("test home");
        let cwd = tempfile::Builder::new().prefix("mare-native-cwd-").tempdir_in(home).unwrap();
        let cwd_path = dunce::canonicalize(cwd.path()).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "suite::policy::fs::native_lua_file_apis_check_the_process_cwd", "--nocapture"])
            .env(CHILD_CWD, &cwd_path)
            .current_dir(&cwd_path)
            .run_output();
        assert!(output.status.success(), "isolated cwd test failed: {output:?}");
        return;
    }
    local(async {
        let env = TestEnv::new().await;
        let process_cwd = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(process_cwd, std::path::PathBuf::from(std::env::var_os(CHILD_CWD).unwrap()));
        let outside = tempfile::tempdir_in(&process_cwd).unwrap();
        let source = outside.path().join("secret.lua");
        std::fs::write(&source, "return 'secret'").unwrap();
        let number = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as i64;
        let numeric_name = number.to_string();
        let numeric = tempfile::Builder::new().prefix(&numeric_name).rand_bytes(0).tempfile_in(&process_cwd).unwrap();
        std::fs::write(numeric.path(), "numeric secret").unwrap();
        let relative = std::path::Path::new(outside.path().file_name().unwrap()).join("secret.lua");
        let script = json!([{ "tool_calls": [{ "name": "native_policy", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "REL", relative.to_string_lossy().to_string());
        global(&h, "ABS", source.to_string_lossy().to_string());
        global(&h, "NUM", number);
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({
                level = "workspace-write",
                restrict_read = true,
                exclude_tmpdir_env_var = true,
                exclude_slash_tmp = true,
            })
            T:register("native_policy", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local results = {}
                    for _, op in ipairs({ io.open, io.lines, io.input, loadfile, dofile, os.remove }) do
                        results[#results + 1] = tostring(pcall(op, REL))
                    end
                    results[#results + 1] = tostring(pcall(io.input, NUM))
                    results[#results + 1] = tostring(pcall(io.output, NUM))
                    return table.concat(results, ",")
                end,
            })
        "#,
        )
        .await;
        let policy = mare_binding::ossandbox::current(h.lua()).unwrap();
        assert!(!policy.can_read(&process_cwd), "fixture cwd is inside allowed read roots: {process_cwd:?}");
        assert!(!policy.can_write(&process_cwd), "fixture cwd is inside allowed write roots: {process_cwd:?}");
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        assert!(dump.contains("false,false,false,false,false,false,false,false"), "{dump}");
        assert!(source.exists());
        assert_eq!(std::fs::read_to_string(numeric.path()).unwrap(), "numeric secret");
        let trusted: String = eval(&h, "return dofile(ABS)").await;
        assert_eq!(trusted, "secret");
        let trusted_numeric: String = eval(
            &h,
            "local old = io.input(); local f = io.input(NUM); local s = io.read('*a'); io.input(old); f:close(); return s",
        )
        .await;
        assert_eq!(trusted_numeric, "numeric secret");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn require_cannot_load_an_unreadable_absolute_module_path() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/require-probe");
        std::fs::create_dir_all(plugin.join("lua")).unwrap();
        std::fs::write(plugin.join("init.lua"), "return { name = 'require-probe', apply = function() end }").unwrap();
        std::fs::write(plugin.join("lua/allowed.lua"), "return 'allowed'").unwrap();
        let stale = plugin.join("lua/stale.lua");
        std::fs::write(&stale, "return 'old'").unwrap();
        let outside = tempfile::Builder::new().prefix("s8-module-").tempdir().unwrap();
        let module = outside.path().join("secret.lua");
        std::fs::write(&module, "return 'secret'").unwrap();
        let module_name = outside.path().join("secret");
        let script = json!([{ "tool_calls": [{ "name": "require_policy", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).user_plugins().no_session().build().await;
        expose(&h);
        let old: String = eval(&h, "return require('stale')").await;
        assert_eq!(old, "old");
        h.kernel.reload(Some("require-probe")).await.unwrap();
        let modified = std::fs::metadata(&stale).unwrap().modified().unwrap();
        std::fs::write(&stale, "return 'new'").unwrap();
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(modified).unwrap();
        global(&h, "MODULE", module_name.to_string_lossy().to_string());
        global(&h, "PLUGIN_ROOT", plugin.to_string_lossy().to_string());
        global(&h, "ALLOWED_SOURCE", plugin.join("lua/allowed.lua").to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write", restrict_read = true,
                reads_pinned = true, readable = { ".", PLUGIN_ROOT } })
            T:register("require_policy", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local allowed = require("allowed")
                    local fresh = require("stale")
                    return "allowed=" .. allowed .. ",fresh=" .. fresh .. ",require=" .. tostring(pcall(require, MODULE))
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        assert!(dump.contains("allowed=allowed,fresh=new,require=false"), "{dump}");
        let cached: usize = eval(&h, "return mare.loader.reset(ALLOWED_SOURCE)").await;
        assert_eq!(cached, 0, "tool-context compilation must not write the host cache");
        let (ok, err): (bool, Option<String>) = eval(&h, "local ok, e = pcall(require, MODULE); return ok, e").await;
        assert!(!ok && err.as_deref().is_some_and(|m| m.contains("invalid module name")), "{ok} {err:?}");
        let allowed: String = eval(&h, "return require('allowed')").await;
        assert_eq!(allowed, "allowed");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn native_file_apis_reject_nul_before_policy_and_io() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let secret = outside.path().join("secret.lua");
        std::fs::write(&secret, "return 'secret'").unwrap();
        let good = env.cwd.join("good.lua");
        std::fs::write(&good, "return 'good'").unwrap();
        let script = json!([{ "tool_calls": [{ "name": "nul_paths", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        global(&h, "GOOD", good.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write", restrict_read = true,
                deny_read = { SECRET }, accept_partial = { "fs_deny_read" } })
            function NUL_PROBE()
                    local p = SECRET .. "\0/../allowed"
                    local ops = { function() return io.open(p, "r") end, function() return io.open(p, "w") end,
                        function() return io.lines(p) end, function() return io.input(p) end,
                        function() return io.output(p) end, function() return os.remove(p) end,
                        function() return os.rename(p, "ok") end, function() return os.rename("ok", p) end,
                        function() return loadfile(p) end, function() return dofile(p) end }
                    local out = {}
                    for _, op in ipairs(ops) do
                        local ok, err = pcall(op)
                        local msg = tostring(err)
                        out[#out + 1] = tostring(ok) .. ":" .. tostring(msg:match("file path contains NUL")) .. ":" .. tostring(msg:find(SECRET, 1, true) == nil)
                    end
                    return table.concat(out, ",")
            end
            T:register("nul_child", { description = "x", parameters = { type = "object" },
                execute = function() return NUL_PROBE() end })
            T:register("nul_paths", { description = "x", parameters = { type = "object" },
                execute = function()
                    local denied = not pcall(io.open, SECRET, "r")
                    local f = assert(io.open(GOOD, "r"))
                    local text = f:read("*a"); f:close()
                    local loaded = assert(loadfile(GOOD))()
                    local child = T:execute({ id = "child", name = "nul_child", arguments = {} }, {})
                    return "denied=" .. tostring(denied) .. ",normal=" .. text .. ":" .. loaded .. ",outer=" .. NUL_PROBE() .. ",child=" .. child.content
                end })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        assert!(dump.contains("denied=true"), "{dump}");
        assert!(dump.contains("normal=return 'good':good"), "{dump}");
        assert_eq!(dump.matches("false:file path contains NUL:true").count(), 20, "{dump}");
        let trusted: String = eval(&h, "return NUL_PROBE()").await;
        assert_eq!(trusted.matches("false:file path contains NUL:true").count(), 10, "{trusted}");
        let trusted_good: String = eval(&h, "return assert(loadfile(GOOD))()").await;
        assert_eq!(trusted_good, "good");
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "return 'secret'");
        h.shutdown().await;
    })
    .await
}
