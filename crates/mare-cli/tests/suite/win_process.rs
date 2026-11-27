//! Windows process control: stopping a terminal sends Ctrl+C (`\x03`) through the
//! ConPTY before the hard stop, so a program that handles Ctrl+C
//! itself gets to. The Unix counterpart is `process::stopping_a_pty_sends_the_interrupt_character_first`;
//! the Job Object setup failure path is covered by mare-binding's `platform::windows_tests`.
#![cfg(windows)]

use crate::common::*;

#[tokio::test]
async fn stopping_a_terminal_sends_ctrl_c_first() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let out: String = eval(
            &h,
            r#"local script = "[Console]::TreatControlCAsInput = $true; Write-Output 'ready'; "
                 .. "while ($true) { $k = [Console]::ReadKey($true); "
                 .. "if (($k.Modifiers -band [ConsoleModifiers]::Control) -and $k.Key -eq 'C') { Write-Output 'got-ctrl-c'; exit 0 } }"
               local p = mare.pty.spawn({ cmd = "powershell.exe", args = { "-NoProfile", "-Command", script } })
               local seen, t0 = "", mare.task.clock()
               while not seen:find("ready", 1, true) and mare.task.clock() - t0 < 30000 do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               p:stop({ grace_ms = 5000 })
               while true do
                   local chunk = p:read()
                   if not chunk then break end
                   seen = seen .. chunk
               end
               return seen"#,
        )
        .await;
        assert!(out.contains("got-ctrl-c"), "{out:?}");
        h.shutdown().await;
    })
    .await
}
