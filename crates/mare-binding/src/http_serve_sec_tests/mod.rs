use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use futures_util::{SinkExt, StreamExt};
use mlua::ObjectLike;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as ClientWsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub(super) use super::{next_listener_cookie_name, serve, serve_with_hooks, ws};

fn setup_lua() -> Lua {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    crate::task::install(&lua, &mare).unwrap();
    crate::http::install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua
}

async fn start_with_token_path(lua: &Lua, opts_expr: &str, token_path: PathBuf) -> mlua::AnyUserData {
    let opts: mlua::Table = lua.load(opts_expr).eval().unwrap();
    let mv = super::serve_with_token_path(lua.clone(), opts, token_path).await.unwrap();
    match mv.into_iter().next().unwrap() {
        mlua::Value::UserData(ud) => ud,
        other => panic!("expected HttpServer userdata, got {other:?}"),
    }
}

async fn start_server(lua: &Lua, opts_expr: &str) -> mlua::AnyUserData {
    let opts: mlua::Table = lua.load(opts_expr).eval().unwrap();
    let mv = super::serve(lua.clone(), opts).await.unwrap();
    match mv.into_iter().next().unwrap() {
        mlua::Value::UserData(ud) => ud,
        other => panic!("expected HttpServer userdata, got {other:?}"),
    }
}

#[derive(Clone, Default)]
struct CaptureSubscriber(Arc<Mutex<Vec<String>>>);

impl tracing::field::Visit for CaptureSubscriber {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.lock().unwrap().push(format!("{}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = self.clone();
        event.record(&mut visitor);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

fn is_loopback_bind_unavailable_io_err(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    matches!(err.raw_os_error(), Some(97 | 99 | 101 | 47 | 49 | 51 | 10047 | 10049 | 10051))
}

fn is_addr_in_use_serve_err(err: &str) -> bool {
    err.starts_with("cannot listen on ")
        && (err.contains("Address already in use")
            || err.contains("in use")
            || err.contains("os error 98")
            || err.contains("os error 48")
            || err.contains("os error 10048"))
}

fn extract_serve_result(mv: mlua::MultiValue) -> Result<mlua::AnyUserData, String> {
    let mut it = mv.into_iter();
    match it.next() {
        Some(mlua::Value::UserData(ud)) => Ok(ud),
        Some(mlua::Value::Nil) => {
            let err = match it.next() {
                Some(mlua::Value::String(s)) => s.to_string_lossy(),
                other => format!("unknown serve error: {other:?}"),
            };
            Err(err)
        }
        other => Err(format!("unexpected serve return value: {other:?}")),
    }
}

#[tokio::test]
async fn per_listener_cookie_names_do_not_collide_on_same_port() {
    tokio::task::LocalSet::new()
        .run_until(async {
            // Probe which second loopback address genuinely has listen capability before starting servers.
            let mut candidate_host = None;
            // MARE_TEST_NO_SECOND_LOOPBACK simulates a host without one (the skip-report check in
            // ossandbox::host_protection_tests drives this test through nextest that way)
            let hosts: &[&str] =
                if std::env::var_os("MARE_TEST_NO_SECOND_LOOPBACK").is_some() { &[] } else { &["::1", "127.0.0.2"] };
            for &host in hosts {
                match tokio::net::TcpListener::bind((host, 0)).await {
                    Ok(probe) => {
                        drop(probe);
                        candidate_host = Some(host);
                        break;
                    }
                    Err(err) if is_loopback_bind_unavailable_io_err(&err) => continue,
                    Err(err) => panic!("unexpected error probing second loopback {host}: {err}"),
                }
            }
            let Some(host2) = candidate_host else {
                eprintln!(
                    "SKIPPED[capability=second_loopback]: per_listener_cookie_names_do_not_collide_on_same_port \
                     (neither ::1 nor 127.0.0.2 is available to bind)"
                );
                return;
            };

            let lua = Lua::new();
            let mut last_addr_in_use = None;
            let (s1, s2) = loop {
                let s1 = start_server(
                    &lua,
                    r#"{ host = "127.0.0.1", port = 0, token = "tok-one", handler = function() return { status = 200, body = "one" } end }"#,
                )
                .await;
                let a1: String = s1.call_method("addr", ()).unwrap();
                let port: u16 = a1.rsplit(':').next().unwrap().parse().unwrap();

                let opts2: mlua::Table = lua
                    .load(format!(
                        r#"{{ host = "{host2}", port = {port}, token = "tok-two", handler = function() return {{ status = 200, body = "two" }} end }}"#
                    ))
                    .eval()
                    .unwrap();
                match extract_serve_result(super::serve(lua.clone(), opts2).await.unwrap()) {
                    Ok(s2) => break (s1, s2),
                    Err(err) if is_addr_in_use_serve_err(&err) => {
                        s1.call_async_method::<()>("close", ()).await.unwrap();
                        if last_addr_in_use.is_some() {
                            panic!("second loopback {host2}:{port} repeatedly hit AddrInUse: {err}");
                        }
                        last_addr_in_use = Some(err);
                    }
                    Err(err) => {
                        s1.call_async_method::<()>("close", ()).await.unwrap();
                        panic!("second loopback serve failed unexpectedly on {host2}:{port}: {err}");
                    }
                }
            };
            let a1: String = s1.call_method("addr", ()).unwrap();
            let a2: String = s2.call_method("addr", ()).unwrap();
            let no_redirect = crate::http::test_client_no_redirect();

            let r1 = no_redirect.get(format!("http://{a1}/?token=tok-one")).send().await.unwrap();
            assert_eq!(r1.status(), 303);
            let c1 = r1.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            let r2 = no_redirect.get(format!("http://{a2}/?token=tok-two")).send().await.unwrap();
            assert_eq!(r2.status(), 303);
            let c2 = r2.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            assert_ne!(
                c1.split('=').next().unwrap(),
                c2.split('=').next().unwrap(),
                "same-port listeners on different loopbacks must have distinct cookie names"
            );
            let jar = format!("{c1}; {c2}");

            // Shared browser cookie jar authenticates both listeners, while c1 alone cannot auth s2.
            let ok1 = no_redirect.get(format!("http://{a1}/")).header("cookie", &jar).send().await.unwrap();
            assert_eq!(ok1.status(), 200);
            assert_eq!(ok1.text().await.unwrap(), "one");

            let ok2 = no_redirect.get(format!("http://{a2}/")).header("cookie", &jar).send().await.unwrap();
            assert_eq!(ok2.status(), 200);
            assert_eq!(ok2.text().await.unwrap(), "two");

            let cross = no_redirect.get(format!("http://{a2}/")).header("cookie", &c1).send().await.unwrap();
            assert_eq!(cross.status(), 401);

            s1.call_async_method::<()>("close", ()).await.unwrap();
            s2.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

fn run_http_serve_host_protect_child(mare_home: &Path) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(
        tokio::task::LocalSet::new().run_until(async {
            let lua = setup_lua();
            let token_file = mare_shared::paths::host_token_file();
            assert!(!token_file.exists(), "token file must not exist before test");

            // 1. Omitted token without accept_partial must refuse and write NO token file.
            let err_default: String = lua
                .load(r#"local s, e = mare.http.serve { handler = function() return nil end }; assert(s == nil); return e"#)
                .eval_async()
                .await
                .unwrap();
            assert!(err_default.contains("SANDBOX_INSUFFICIENT"), "{err_default}");
            assert!(err_default.contains("fs_host_protect"), "{err_default}");
            assert!(!token_file.exists(), "refused serve must not create token file");

            // 2. Accepting a different capability (fs_protect) must still refuse fs_host_protect.
            let err_other: String = lua
                .load(
                    r#"
                    local s, e = mare.http.serve {
                        accept_partial = { "fs_protect" },
                        handler = function() return nil end,
                    }
                    assert(s == nil)
                    return e
                "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(err_other.contains("SANDBOX_INSUFFICIENT"), "{err_other}");
            assert!(err_other.contains("fs_host_protect"), "{err_other}");
            assert!(!token_file.exists(), "refused serve must not create token file");

            // 3. Static custom token does not use <state>/host/token and succeeds without accept_partial.
            let static_srv = start_server(
                &lua,
                r#"{ token = "static-custom-tok", handler = function() return { status = 200, body = "static" } end }"#,
            )
            .await;
            let static_addr: String = static_srv.call_method("addr", ()).unwrap();
            let resp = crate::http::test_client()
                .get(format!("http://{static_addr}/"))
                .header("authorization", "Bearer static-custom-tok")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert!(!token_file.exists(), "static token must not create runtime token file");
            static_srv.call_async_method::<()>("close", ()).await.unwrap();

            // 4. Explicit accept_partial = { "fs_host_protect" } succeeds, creates 0600 token, and serves.
            let srv = start_server(
                &lua,
                r#"{ accept_partial = { "fs_host_protect" }, handler = function() return { status = 200, body = "accepted" } end }"#,
            )
            .await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            assert!(token_file.exists(), "accepted serve must create runtime token file under {}", mare_home.display());
            let tok = std::fs::read_to_string(&token_file).unwrap();
            assert_eq!(tok.len(), 43);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777, 0o600);
            }
            let resp = crate::http::test_client()
                .get(format!("http://{addr}/"))
                .header("authorization", format!("Bearer {tok}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "accepted");
            srv.call_async_method::<()>("close", ()).await.unwrap();
        }),
    );
}

#[test]
fn http_serve_enforces_fs_host_protect_strictly_unless_accepted() {
    const CHILD: &str = "MARE_TEST_HTTP_SERVE_FS_HOST_PROTECT";
    if let Ok(home_str) = std::env::var(CHILD) {
        run_http_serve_host_protect_child(Path::new(&home_str));
        std::process::exit(0);
    }

    // 1. All platforms: narrowing caps to omit fs_host_protect must strictly refuse unless accepted.
    let home_caps = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "http_serve::sec_tests::http_serve_enforces_fs_host_protect_strictly_unless_accepted"])
        .env(CHILD, home_caps.path())
        .env("MARE_HOME", home_caps.path())
        .env("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "narrowed caps child failed: {out:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. Linux Landlock backend (if available): real Landlock backend lacks fs_host_protect and must refuse unless accepted.
    #[cfg(target_os = "linux")]
    if crate::ossandbox::landlock_abi().is_some() {
        let home_ll = tempfile::tempdir().unwrap();
        let out_ll = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "http_serve::sec_tests::http_serve_enforces_fs_host_protect_strictly_unless_accepted"])
            .env(CHILD, home_ll.path())
            .env("MARE_HOME", home_ll.path())
            .env("MARE_SANDBOX_BACKEND", "landlock")
            .env_remove("MARE_SANDBOX_CAPS")
            .output()
            .unwrap();
        assert!(
            out_ll.status.success(),
            "landlock child failed: {out_ll:?}\nstderr:\n{}",
            String::from_utf8_lossy(&out_ll.stderr)
        );
    }
}

mod limits_and_cookies;
mod tokens_and_hosts;
