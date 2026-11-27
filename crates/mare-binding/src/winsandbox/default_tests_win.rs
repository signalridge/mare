//! Unit tests for the Windows default sandbox tier (restricted token, NUL ACL, environment, pipe stdio).

use super::acl::{ACCESS_ALLOWED_ACE_TYPE, allow_null_device, has_exact};
use super::token::restricted_token;
use super::*;

mod enhanced_token_tests {
    use super::*;
    use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, TOKEN_GROUPS, TokenRestrictedSids, WinWorldSid};

    #[test]
    fn enhanced_token_restricts_to_capability_account_login_and_everyone() {
        if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
            return;
        }
        let (user, logon) = current_sids().unwrap();
        let caps = [LocalSid::parse("S-1-4-918271-123456").unwrap()];
        let account = sid_string(user.ptr()).unwrap();
        unsafe {
            let token = restricted_token(&caps, Some(&account)).unwrap();
            let mut needed = 0;
            GetTokenInformation(token, TokenRestrictedSids, std::ptr::null_mut(), 0, &mut needed);
            let mut storage = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            assert_ne!(
                GetTokenInformation(token, TokenRestrictedSids, storage.as_mut_ptr().cast(), needed, &mut needed),
                0
            );
            let groups = &*storage.as_ptr().cast::<TOKEN_GROUPS>();
            assert_eq!(groups.GroupCount, 4);
            let groups = std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
            let world = BufSid::well_known(WinWorldSid).unwrap();
            for (actual, expected) in groups.iter().zip([caps[0].0, user.ptr(), logon.ptr(), world.ptr()]) {
                assert_ne!(EqualSid(actual.Sid, expected), 0);
            }
            CloseHandle(token);
        }
    }
}

mod pipe_handle_tests {
    use super::*;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::GetHandleInformation;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateProcessW};

    fn pipe() -> (OwnedHandle, OwnedHandle) {
        unsafe {
            let mut read = std::ptr::null_mut();
            let mut write = std::ptr::null_mut();
            assert_ne!(CreatePipe(&mut read, &mut write, std::ptr::null(), 0), 0);
            (OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write))
        }
    }

    #[test]
    fn null_device_grants_only_noninheritable_capability_aces() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let cap_strs = ["S-1-4-918271-123457".to_string(), "S-1-4-918271-123458".to_string()];
        let caps = [LocalSid::parse(&cap_strs[0]).unwrap(), LocalSid::parse(&cap_strs[1]).unwrap()];
        let _g1 = allow_null_device(&caps, &cap_strs);
        let _g2 = allow_null_device(&caps, &cap_strs); // SET_ACCESS remains idempotent.
        unsafe {
            let handle = CreateFileW(
                wide(r"\\.\NUL").as_ptr(),
                0x0002_0000,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            assert!(!handle.is_null() && handle != INVALID_HANDLE_VALUE, "{}", last("open NUL ACL"));
            let handle = OwnedHandle::from_raw_handle(handle);
            let mut dacl = std::ptr::null_mut();
            let mut sd = std::ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                ),
                0
            );
            let grants = caps.iter().all(|cap| {
                has_exact(
                    dacl,
                    ACCESS_ALLOWED_ACE_TYPE,
                    0,
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
                    cap.0,
                )
            });
            LocalFree(sd);
            assert!(grants, "NUL must grant every capability read/write/execute without inheritance");
        }
    }

    #[test]
    fn unicode_environment_is_sorted_and_double_terminated() {
        let entries = [
            ("z_key".encode_utf16().collect(), "日本語 🦀".encode_utf16().collect()),
            ("A_key".encode_utf16().collect(), "c:\\私用\\作業".encode_utf16().collect()),
        ];
        let block = environment_block(&entries);
        assert_eq!(String::from_utf16(&block).unwrap(), "A_key=c:\\私用\\作業\0z_key=日本語 🦀\0\0");
        assert_eq!(environment_block(&[]), [0, 0]);
    }

    #[test]
    fn unicode_environment_preserves_hidden_drive_and_unpaired_surrogates() {
        let entries = [
            ("=C:".encode_utf16().collect(), "C:\\作業".encode_utf16().collect()),
            ("ODD".encode_utf16().collect(), vec![0xD800]),
        ];
        let mut expected: Vec<u16> = "=C:=C:\\作業\0ODD=".encode_utf16().collect();
        expected.extend([0xD800, 0, 0]);
        assert_eq!(environment_block(&entries), expected);
    }

    #[test]
    fn pipe_stdio_rejects_missing_and_unusable_handles() {
        use std::os::windows::io::AsRawHandle;
        let (read, write) = pipe();
        let valid = [read.as_raw_handle(), write.as_raw_handle(), write.as_raw_handle()];
        for index in 0..3 {
            for invalid in [std::ptr::null_mut(), INVALID_HANDLE_VALUE] {
                let mut stdio = valid;
                stdio[index] = invalid;
                let mut si = unsafe { std::mem::zeroed() };
                assert!(PipeStdio::prepare(&mut si, stdio).is_err());
            }
        }
        // A non-null value can still be unusable. The kernel must reject it too.
        let mut si = unsafe { std::mem::zeroed() };
        assert!(PipeStdio::prepare(&mut si, [std::ptr::dangling_mut::<c_void>(), valid[1], valid[2]]).is_err());
    }

    #[test]
    fn pipe_stdio_inherits_only_selected_handles() {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        let event_name = format!("Local\\mare-pipe-inheritance-{}", std::process::id());
        let sentinel = unsafe {
            let handle = CreateEventW(std::ptr::null(), 1, 0, wide(&event_name).as_ptr());
            assert!(!handle.is_null());
            OwnedHandle::from_raw_handle(handle)
        };
        let sentinel = sentinel.as_raw_handle();
        unsafe {
            assert_ne!(SetHandleInformation(sentinel, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT), 0);
            let mut flags = 0;
            assert_ne!(GetHandleInformation(sentinel, &mut flags), 0);
            assert_ne!(flags & HANDLE_FLAG_INHERIT, 0);
        }
        // Exercise separate stderr and the common merged-output duplicate handle.
        for merge_stderr in [false, true] {
            let (stdin, _stdin_writer) = pipe();
            let (stdout_reader, stdout) = pipe();
            let (stderr_reader, stderr) = pipe();
            let stderr_handle = if merge_stderr { stdout.as_raw_handle() } else { stderr.as_raw_handle() };
            let stdio = [stdin.as_raw_handle(), stdout.as_raw_handle(), stderr_handle];
            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            let _attributes = PipeStdio::prepare(&mut si, stdio).unwrap();
            // Compare kernel object identity: PowerShell can reuse the absent sentinel's
            // numeric handle slot during startup. A different object is not inheritance.
            let script = format!(
                r#"if($env:MARE_PIPE_UNICODE -ne '日本語 🦀') {{ exit 44 }}; Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class Native {{ [DllImport("kernel32.dll")] public static extern IntPtr GetStdHandle(int n); [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetHandleInformation(IntPtr h, out uint f); [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr OpenEventW(uint a, bool inherit, string name); [DllImport("kernel32.dll")] public static extern bool CompareObjectHandles(IntPtr a, IntPtr b); }}'; $expected=@({},{},{}); $kinds=@(-10,-11,-12); for($i=0;$i -lt 3;$i++) {{ $h=[Native]::GetStdHandle($kinds[$i]); $flags=0; if($h.ToInt64() -ne $expected[$i] -or ![Native]::GetHandleInformation($h,[ref]$flags)) {{ exit 41 }} }}; $event=[Native]::OpenEventW(0x100000,$false,'{}'); if($event -eq [IntPtr]::Zero) {{ exit 43 }}; if([Native]::CompareObjectHandles([IntPtr]{},$event)) {{ exit 42 }}; exit 0"#,
                stdio[0] as usize, stdio[1] as usize, stdio[2] as usize, event_name, sentinel as usize,
            );
            let argv = vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-InputFormat".into(),
                "None".into(),
                "-Command".into(),
                script,
            ];
            let mut command = wide(argv_to_command_line(&argv));
            let mut entries: Vec<_> = std::env::vars_os().filter(|(key, _)| key != "MARE_PIPE_UNICODE").collect();
            entries.push(("MARE_PIPE_UNICODE".into(), "日本語 🦀".into()));
            let entries: Vec<_> = entries
                .into_iter()
                .map(|(key, value)| (key.encode_wide().collect(), value.encode_wide().collect()))
                .collect();
            let environment = environment_block(&entries);
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            unsafe {
                assert_ne!(
                    CreateProcessW(
                        std::ptr::null(),
                        command.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        1,
                        CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                        environment.as_ptr().cast(),
                        std::ptr::null(),
                        &si.StartupInfo,
                        &mut pi,
                    ),
                    0,
                    "{}",
                    last("CreateProcessW(pipe inheritance regression)")
                );
                let process = OwnedHandle::from_raw_handle(pi.hProcess);
                let _thread = OwnedHandle::from_raw_handle(pi.hThread);
                // PowerShell and its compiler can fill a pipe while the parent waits for exit.
                // Close the parent's write ends and drain both streams concurrently.
                drop(stdout);
                drop(stderr);
                let (output_tx, output_rx) = std::sync::mpsc::channel();
                for (name, handle) in [("stdout", stdout_reader), ("stderr", stderr_reader)] {
                    let output_tx = output_tx.clone();
                    std::thread::spawn(move || {
                        let mut bytes = Vec::new();
                        let result = std::fs::File::from(handle).read_to_end(&mut bytes);
                        let _ = output_tx.send((name, result.map(|_| bytes)));
                    });
                }
                drop(output_tx);
                let wait = WaitForSingleObject(process.as_raw_handle(), 30_000);
                if wait != 0 {
                    windows_sys::Win32::System::Threading::TerminateProcess(process.as_raw_handle(), 1);
                    WaitForSingleObject(process.as_raw_handle(), 5_000);
                    panic!("pipe inheritance child did not exit: wait status {wait}");
                }
                let mut output = Vec::new();
                for _ in 0..2 {
                    let (name, result) = output_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("child output pipe remained open after exit");
                    output.push((name, String::from_utf8_lossy(&result.unwrap()).into_owned()));
                }
                let mut code = 1;
                assert_ne!(GetExitCodeProcess(process.as_raw_handle(), &mut code), 0);
                assert_eq!(code, 0, "child stdio/sentinel handle check failed: {output:?}");
            }
        }
    }
}
