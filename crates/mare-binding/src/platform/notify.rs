//! Native desktop notifications (Windows tray balloon).

/// A native desktop notification, for terminals that can't show OSC notifications (Windows
/// legacy conhost, or a terminal other than Windows Terminal). Shown from a background thread;
/// returns false where unsupported (non-Windows: terminals handle OSC 9/777/99).
pub fn notify_native(title: &str, body: &str) -> bool {
    #[cfg(windows)]
    {
        win_notify::show(title, body)
    }
    #[cfg(not(windows))]
    {
        let _ = (title, body);
        false
    }
}

/// Windows notification-area balloon (rendered as a toast on Windows 10/11): a message-only
/// window owns a temporary tray icon for a few seconds. No AUMID / shortcut registration needed,
/// which a WinRT toast from an unpackaged console app would require.
#[cfg(windows)]
mod win_notify {
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, IDI_INFORMATION, LoadIconW,
    };

    fn fill<const N: usize>(dst: &mut [u16; N], s: &str) {
        for (i, c) in s.encode_utf16().take(N - 1).enumerate() {
            dst[i] = c;
        }
    }

    pub fn show(title: &str, body: &str) -> bool {
        let (title, body) = (title.to_string(), body.to_string());
        std::thread::Builder::new()
            .name("mare-notify".into())
            .spawn(move || unsafe {
                let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
                let hwnd = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return;
                }
                let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd;
                nid.uID = 1;
                nid.uFlags = NIF_INFO | NIF_ICON | NIF_TIP;
                nid.hIcon = LoadIconW(std::ptr::null_mut(), IDI_INFORMATION);
                nid.dwInfoFlags = NIIF_INFO;
                fill(&mut nid.szTip, &title);
                fill(&mut nid.szInfoTitle, &title);
                fill(&mut nid.szInfo, &body);
                if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
                    std::thread::sleep(std::time::Duration::from_secs(6));
                    Shell_NotifyIconW(NIM_DELETE, &nid);
                }
                DestroyWindow(hwnd);
            })
            .is_ok()
    }
}
