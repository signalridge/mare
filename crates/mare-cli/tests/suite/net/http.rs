use super::*;

/// `mare.http.request` from inside a tool call with this sandbox `network` table (Lua source).
const IN_TOOL: &str = r#"
    local function in_tool(network, url)
        local ctx = { sandbox = { level = "workspace-write", network = network } }
        local ok, r = mare.exec.run(ctx, function()
            return pcall(function() return mare.http.request({ url = url }):text() end)
        end)
        return ok, tostring(r), ctx.denials and ctx.denials[1] and ctx.denials[1].host or ""
    end
"#;

#[tokio::test]
async fn http_redirects_are_reauthorized_every_hop() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (port, _) = http_server("http://evil.test/steal".into());
        fake_dns(&["good.test", "evil.test"]);
        global(&h, "URL", format!("http://good.test:{port}/redirect"));
        let (ok, err, denied): (bool, String, String) =
            eval(&h, &format!(r#"{IN_TOOL} return in_tool({{ allow = {{ "good.test" }}, allow_local = true }}, URL)"#))
                .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(!ok && err.contains("evil.test") && err.contains("not in the network allow list"), "{err}");
        assert_eq!(denied, "evil.test", "the refused hop is recorded as evidence");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn http_names_resolving_to_private_addresses_are_blocked_and_connections_pinned() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (port, hosts) = http_server(String::new());
        fake_dns(&["rebind.test"]);
        global(&h, "URL", format!("http://rebind.test:{port}/"));
        let (blocked, err, ok, body): (bool, String, bool, String) = eval(
            &h,
            &format!(
                r#"{IN_TOOL}
                   local b, e = in_tool({{ allow = {{ "rebind.test" }} }}, URL)
                   local ok, body = in_tool({{ allow = {{ "rebind.test" }}, allow_local = true }}, URL)
                   return not b, e, ok, body"#
            ),
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(blocked && err.contains("resolves to a local/private address"), "{err}");
        // rebind.test only exists in the authorization layer's resolution: reaching the server
        // proves the connection used the checked address (no second, unchecked lookup)
        assert!(ok && body == "ok", "{body}");
        assert_eq!(hosts.lock().unwrap().as_slice(), [format!("rebind.test:{port}")]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn http_redirect_policy_and_final_url_are_public() {
    local(async {
        let env = TestEnv::new().await;
        let (target, hits) = http_server("/plain".into());
        let (port, _) = http_server(format!("http://127.0.0.1:{target}/plain"));
        let (cred_port, _) = http_server(format!("http://other:secret@127.0.0.1:{target}/plain"));
        let h = env.host().no_session().build().await;
        global(&h, "URL", format!("http://127.0.0.1:{port}/redirect"));
        global(&h, "CRED_URL", format!("http://user:pass@127.0.0.1:{cred_port}/redirect"));
        let (status, manual_url, final_url, cred_final_url, blocked, zero): (u16, String, String, String, String, String) =
            eval(
                &h,
                r#"
            local r = mare.http.request({ url = URL, redirect = "manual" })
            local status, url = r.status, r.url
            r:close()
            local f = mare.http.request({ url = URL })
            local final = f.url
            assert(f:text() == "ok")
            local cf = mare.http.request({ url = CRED_URL })
            local cred_final = cf.url
            assert(cf:text() == "ok")
            local ok, err = pcall(function() mare.http.request({ url = URL, redirect = "same-origin" }) end)
            assert(not ok)
            local ok2, err2 = pcall(function() mare.http.request({ url = URL, max_redirects = 0 }) end)
            assert(not ok2)
            return status, url, final, cred_final, tostring(err), tostring(err2)
        "#,
            )
            .await;
        assert_eq!(status, 302);
        assert_eq!(manual_url, format!("http://127.0.0.1:{port}/redirect"));
        assert_eq!(final_url, format!("http://127.0.0.1:{target}/plain"));
        assert_eq!(cred_final_url, format!("http://127.0.0.1:{target}/plain"));
        assert!(blocked.contains("redirect"), "{blocked}");
        assert!(zero.contains("redirect"), "{zero}");
        assert_eq!(hits.lock().unwrap().len(), 2);

        // Restricted network (send_authorized) also enforces redirect="manual", "same-origin", max_redirects, and userinfo stripping.
        fake_dns(&["good.test", "other.test"]);
        let (same_port, _) = http_server(format!("http://other.test:{target}/plain"));
        global(&h, "AUTH_URL", format!("http://user:pw@good.test:{same_port}/redirect"));
        let (ok_man, man_status, man_url, ok_same, same_err, ok_zero, zero_err, ok_follow, follow_url): (
            bool,
            u16,
            String,
            bool,
            String,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "good.test", "other.test" }, allow_local = true } } }
            return mare.exec.run(ctx, function()
                local r = mare.http.request({ url = AUTH_URL, redirect = "manual" })
                local ms, mu = r.status, r.url
                r:close()
                local ok1, e1 = pcall(function() mare.http.request({ url = AUTH_URL, redirect = "same-origin" }) end)
                local ok2, e2 = pcall(function() mare.http.request({ url = AUTH_URL, max_redirects = 0 }) end)
                local f = mare.http.request({ url = AUTH_URL })
                local fu = f.url
                assert(f:text() == "ok")
                return true, ms, mu, ok1, tostring(e1), ok2, tostring(e2), true, fu
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(ok_man && man_status == 302 && man_url == format!("http://good.test:{same_port}/redirect"), "{man_url}");
        assert!(!ok_same && same_err.contains("cross-origin"), "{same_err}");
        assert!(!ok_zero && zero_err.contains("too many redirects"), "{zero_err}");
        assert!(ok_follow && follow_url == format!("http://other.test:{target}/plain"), "{follow_url}");
        h.shutdown().await;
    })
    .await
}
