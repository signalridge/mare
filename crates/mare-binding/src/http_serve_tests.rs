use std::time::Duration;

use super::*;
use futures_util::{SinkExt, StreamExt};
use mlua::ObjectLike;
use tokio_tungstenite::tungstenite::Message as ClientWsMsg;

fn setup_lua() -> Lua {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    crate::codec::install(&lua, &mare).unwrap();
    crate::task::install(&lua, &mare).unwrap();
    crate::http::install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua
}

async fn start_serve_with_hooks(lua: &Lua, opts_expr: &str) -> (mlua::AnyUserData, Arc<super::ws::ServeTestHooks>) {
    let hooks = Arc::new(super::ws::ServeTestHooks::default());
    let opts: mlua::Table = lua.load(opts_expr).eval().unwrap();
    if opts.get::<mlua::Value>("token").unwrap().is_nil() {
        opts.set("token", false).unwrap();
    }
    let mv = super::serve_with_hooks(lua.clone(), opts, hooks.clone()).await.unwrap();
    let ud = match mv.into_iter().next().unwrap() {
        mlua::Value::UserData(ud) => ud,
        other => panic!("expected HttpServer userdata, got {other:?}"),
    };
    (ud, hooks)
}

#[path = "http_serve_tests_body.rs"]
mod body;
#[path = "http_serve_tests_limits.rs"]
mod limits;
#[path = "http_serve_tests_plain.rs"]
mod plain;
#[path = "http_serve_tests_sse.rs"]
mod sse;
#[path = "http_serve_tests_websocket.rs"]
mod websocket;
#[path = "http_serve_tests_websocket_backpressure.rs"]
mod websocket_backpressure;
#[path = "http_serve_tests_websocket_lifecycle.rs"]
mod websocket_lifecycle;
