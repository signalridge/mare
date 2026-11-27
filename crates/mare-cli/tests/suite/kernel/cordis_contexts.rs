//! user callbacks handed to cordis run in the execution context of whoever
//! registered them, never in that of whoever triggered them, and a natively blocking cleanup does
//! not hold up other fibers.
//!
//! Three execution contexts are mixed: the host (no context), a tool call confined to a read-only
//! sandbox, and a peer request, also read-only (a temporary directory is writable under a
//! workspace-write policy, so only read-only separates the outcomes reliably). Every callback reports which context
//! it ran in (`mare.exec.current().tool`) and tries one `mare.fs.write`, whose outcome is decided
//! by the sandbox of the context it actually ran in.
//!
//! The cordis host is boot.lua's (`mare_kernel.test` hands tests the booted host).

use crate::common::*;

/// The cordis host under test: boot.lua's host (T5), with `capture` on the kernel binding (T3).
const CK0_HOST: &str = r#"
    return require("mare_kernel.test").boot().host
"#;

/// Lua prelude shared by the cases: the three contexts and the probe.
fn prelude(dir: &std::path::Path, peer: &std::path::Path) -> String {
    format!(
        r#"
        local cordis = require("cordis")
        local host = assert(load({host:?}, "=ck0-host"))()
        DIR, PEER = {dir:?}, {peer:?}
        TOOL = {{ tool = "ck0-tool", tool_call_id = "ck0-tool-1", cwd = DIR,
                  sandbox = {{ level = "read-only", network = false }} }}
        PEER_CTX = {{ tool = "ck0-peer", tool_call_id = "ck0-peer-1", cwd = PEER,
                      sandbox = {{ level = "read-only", network = false }} }}
        -- "<label>:<context it ran in>:<write outcome>"
        function probe(label, path)
            local current = mare.exec.current()
            local who = current and current.tool or "host"
            local ok = pcall(mare.fs.write, path, label)
            return label .. ":" .. who .. ":" .. (ok and "ok" or "denied")
        end
        function root()
            return cordis.Context.new({{ host = host }})
        end
        return cordis
        "#,
        host = CK0_HOST,
        dir = dir.display().to_string(),
        peer = peer.display().to_string(),
    )
}

async fn run(code: &str) -> String {
    let env = TestEnv::new().await;
    let h = env.host().no_session().build().await;
    h.kernel.expose_test_boot();
    let dir = tempfile::tempdir().unwrap();
    let peer = tempfile::tempdir().unwrap();
    let chunk = format!("local cordis = (function() {} end)()\n{code}", prelude(dir.path(), peer.path()));
    let out: String = eval(&h, &chunk).await;
    h.shutdown().await;
    out
}

/// A listener runs in the context it was registered in, whichever context emits.
#[tokio::test]
async fn cordis_contexts_listeners_run_as_their_registrant() {
    local(async {
        let out = run(r#"
            local r = root()
            local out = {}
            r:on("ck0/ping", function() table.insert(out, probe("host-listener", DIR .. "/host-listener")) end)
            mare.exec.run(TOOL, function()
                r:on("ck0/pong", function() table.insert(out, probe("tool-listener", DIR .. "/tool-listener")) end)
            end)
            -- a restricted call emits to a host listener; the host emits to a restricted listener
            mare.exec.run(TOOL, function() r:emit("ck0/ping") end)
            mare.exec.run(PEER_CTX, function() r:emit("ck0/ping") end)
            r:emit("ck0/pong")
            return table.concat(out, ",")
        "#)
        .await;
        assert_eq!(
            out, "host-listener:host:ok,host-listener:host:ok,tool-listener:ck0-tool:denied",
            "listeners must run with their registrant's context, not the emitter's"
        );
    })
    .await
}

/// A Future reaction runs in the context it was registered in, even when another context settles
/// the Future and the job queue is drained by a task some third context started.
#[tokio::test]
async fn cordis_contexts_future_reactions_run_as_their_registrant() {
    local(async {
        let out = run(r#"
            local r = root()
            local out = {}
            local f, resolve = cordis.Future.withResolvers(r.host)
            f:next(function() table.insert(out, probe("host-reaction", DIR .. "/host-reaction")) end)
            mare.exec.run(TOOL, function()
                f:next(function() table.insert(out, probe("tool-reaction", DIR .. "/tool-reaction")) end)
            end)
            mare.exec.run(PEER_CTX, function()
                f:next(function() table.insert(out, probe("peer-reaction", PEER .. "/peer-reaction")) end)
            end)
            -- the peer settles it: none of the reactions may borrow the peer's context
            mare.exec.run(PEER_CTX, function() resolve(1) end)
            local done = f:next(function() end)
            done:await()
            mare.task.sleep(20)
            table.sort(out)
            return table.concat(out, ",")
        "#)
        .await;
        assert_eq!(
            out, "host-reaction:host:ok,peer-reaction:ck0-peer:denied,tool-reaction:ck0-tool:denied",
            "reactions must run with their registrant's context, not the settler's or the drainer's"
        );
    })
    .await
}

/// The three contexts queue work into one root at once: components, their cleanups, listeners and
/// reactions. Each outcome depends only on who handed the callback in.
#[tokio::test]
async fn cordis_contexts_mixed_queue_depends_only_on_the_registrant() {
    local(async {
        let out = run(r#"
            local r = root()
            local out = {}
            local function component(label, path)
                return function(ctx)
                    table.insert(out, probe(label .. "-apply", path .. "-apply"))
                    ctx:effect(function()
                        return function() table.insert(out, probe(label .. "-cleanup", path .. "-cleanup")) end
                    end)
                end
            end
            local fibers = {}
            fibers[1] = r:use(component("host", DIR .. "/host"))
            mare.exec.run(TOOL, function() fibers[2] = r:use(component("tool", DIR .. "/tool")) end)
            mare.exec.run(PEER_CTX, function() fibers[3] = r:use(component("peer", PEER .. "/peer")) end)
            for _, fiber in ipairs(fibers) do cordis.await(fiber) end
            -- dispose them from the other contexts, in a crossed order
            mare.exec.run(PEER_CTX, function() fibers[1]:dispose() end)
            fibers[2]:dispose()
            mare.exec.run(TOOL, function() fibers[3]:dispose() end)
            mare.task.sleep(50)
            table.sort(out)
            return table.concat(out, ",")
        "#)
        .await;
        assert_eq!(
            out,
            concat!(
                "host-apply:host:ok,host-cleanup:host:ok,",
                "peer-apply:ck0-peer:denied,peer-cleanup:ck0-peer:denied,",
                "tool-apply:ck0-tool:denied,tool-cleanup:ck0-tool:denied"
            ),
            "components and cleanups must run with the context of the `use` call"
        );
    })
    .await
}

/// A cleanup that blocks natively for 3 s does not hold up another fiber's load or unrelated
/// Future reactions.
#[tokio::test]
async fn cordis_contexts_blocking_cleanup_does_not_stall_other_fibers() {
    local(async {
        let out = run(r#"
            local r = root()
            local slow = r:use(function(ctx)
                ctx:effect(function() return function() mare.task.sleep(3000) end end)
            end)
            cordis.await(slow)
            slow:dispose()
            local t0 = mare.task.clock()
            local other = r:use(function() end)
            cordis.await(other)
            local loaded = mare.task.clock() - t0
            local f, resolve = cordis.Future.withResolvers(r.host)
            local reacted
            f:next(function() reacted = mare.task.clock() - t0 end)
            resolve(1)
            f:next(function() end):await()
            return string.format("%s %d %d", tostring(other.state == cordis.FiberState.ACTIVE),
                loaded < 1000 and 1 or 0, (reacted or 1e9) < 1000 and 1 or 0)
        "#)
        .await;
        assert_eq!(out, "true 1 1", "another fiber loads and reactions run while a cleanup blocks");
    })
    .await
}

/// Inventory T9(b): mare's own event and policy listeners run in the trigger's task context today
/// (`GRANT` task-local; `policy.rs`), so "listeners run as their registrant" is a behaviour change.
/// When a restricted peer triggers a host listener, the listener keeps the host's permission, and
/// that permission does not leak back to the peer: right after the emit, and in a reaction the
/// peer registers afterwards, the peer is still confined to its workspace.
#[tokio::test]
async fn cordis_contexts_peer_trigger_neither_restricts_nor_inherits_the_registrant() {
    local(async {
        let out = run(r#"
            local r = root()
            local out = {}
            r:on("ck0/ping", function() table.insert(out, probe("host-listener", DIR .. "/host-listener")) end)
            mare.exec.run(PEER_CTX, function()
                r:emit("ck0/ping")
                -- back in the peer: still the peer, still unable to write outside its workspace
                table.insert(out, probe("peer-after-emit", DIR .. "/peer-after-emit"))
                local f, resolve = cordis.Future.withResolvers(r.host)
                f:next(function() table.insert(out, probe("peer-reaction", DIR .. "/peer-reaction")) end)
                resolve(1)
                f:next(function() end):await()
            end)
            mare.task.sleep(20)
            return table.concat(out, ",")
        "#)
        .await;
        assert_eq!(
            out, "host-listener:host:ok,peer-after-emit:ck0-peer:denied,peer-reaction:ck0-peer:denied",
            "the host listener runs as the host; the peer gains nothing from triggering it"
        );
    })
    .await
}

/// A production host never registers the test module `mare_kernel.test`: only test
/// hosts (`mare test`, the kernel tests' `expose`) hand a test the booted host. The tree's own
/// libraries stay requirable (`require("cordis")`).
#[tokio::test]
async fn a_production_host_has_no_test_boot_module() {
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        let h = mare_host::Host::new(env.host().no_session().opts).await.unwrap();
        let (test_module, cordis): (bool, bool) =
            crate::common::eval(&h, r#"return (pcall(require, "mare_kernel.test")), (pcall(require, "cordis"))"#).await;
        h.shutdown().await;
        assert!(!test_module, "a production host registered mare_kernel.test");
        assert!(cordis, "the tree's libraries must stay requirable");
    })
    .await
}
