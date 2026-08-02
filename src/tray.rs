//! Notification area icon lifecycle (GUID-based, resilient to Explorer
//! restarts) and the right-click context menu.

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::Shell::{
    NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFY_ICON_DATA_FLAGS, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, MENU_ITEM_FLAGS, MF_CHECKED, MF_STRING,
    SetForegroundWindow, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
};
use windows::core::GUID;

use crate::win32;

pub const TRAY_GUID: GUID = GUID::from_u128(0x6f1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d);

pub struct Tray {
    hwnd: HWND,
    cb_msg: u32,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    added: bool,
    tooltip: String,
}

impl Tray {
    pub fn new(hwnd: HWND, cb_msg: u32) -> Self {
        Self {
            hwnd,
            cb_msg,
            hicon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
            added: false,
            tooltip: String::new(),
        }
    }

    fn nid(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 0,
            uFlags: flags,
            uCallbackMessage: self.cb_msg,
            hIcon: self.hicon,
            szTip: [0u16; 128],
            ..Default::default()
        };
        if flags.contains(NIF_GUID) {
            nid.guidItem = TRAY_GUID;
        }
        if flags.contains(NIF_TIP) {
            let chars: Vec<u16> = self.tooltip.encode_utf16().take(127).collect();
            for (i, c) in chars.iter().enumerate() {
                nid.szTip[i] = *c;
            }
        }
        nid
    }

    /// Add the icon (also used to recover after Explorer restarts).
    pub fn add(&mut self) -> bool {
        let ok = unsafe {
            Shell_NotifyIconW(
                NIM_ADD,
                &self.nid(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID),
            )
            .as_bool()
        };
        if ok {
            self.added = true;
            // Version 4: proper WM_CONTEXTMENU / balloon behavior.
            let mut nid = self.nid(NOTIFY_ICON_DATA_FLAGS(0));
            nid.uFlags = NIF_GUID;
            unsafe {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
            }
        }
        ok
    }

    pub fn readd(&mut self) {
        if self.added {
            self.remove_icon_only();
        }
        self.add();
    }

    fn remove_icon_only(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid(NIF_GUID));
        }
        self.added = false;
    }

    /// Point the shell icon at `hicon`. Ownership stays with the caller (the
    /// app's icon cache); this only updates the notification area icon.
    pub fn set_icon(&mut self, hicon: windows::Win32::UI::WindowsAndMessaging::HICON) {
        if self.hicon == hicon {
            return;
        }
        self.hicon = hicon;
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid(NIF_ICON | NIF_GUID));
            }
        }
    }

    pub fn set_tooltip(&mut self, text: &str) {
        if self.tooltip == text {
            return;
        }
        self.tooltip = text.to_string();
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid(NIF_TIP | NIF_GUID));
            }
        }
    }

    /// Screen rect of the icon, when the shell can provide it.
    pub fn rect(&self) -> Option<RECT> {
        win32::tray_icon_rect(self.hwnd, TRAY_GUID)
    }

    /// Remove the shell icon. The handle is owned by the caller's cache.
    pub fn remove(&mut self) {
        self.remove_icon_only();
        self.hicon = windows::Win32::UI::WindowsAndMessaging::HICON::default();
    }
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

pub const MENU_OPEN: usize = 1;
pub const MENU_RECONNECT: usize = 2;
pub const MENU_OPEN_CONFIG: usize = 3;
pub const MENU_STARTUP: usize = 4;
pub const MENU_ABOUT: usize = 5;
pub const MENU_EXIT: usize = 6;

/// Show the tray context menu; returns the selected item id (0 = dismissed).
pub fn show_menu(hwnd: HWND, at: POINT, startup_enabled: bool, reconnect_enabled: bool) -> usize {
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        if menu.is_invalid() {
            return 0;
        }
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN,
            win32::pcwstr(&win32::wide("打开 GlazeTray")),
        );
        let flags = if reconnect_enabled {
            MF_STRING
        } else {
            MENU_ITEM_FLAGS(MF_STRING.0 | 0x00000002) // MF_GRAYED
        };
        let _ = AppendMenuW(
            menu,
            flags,
            MENU_RECONNECT,
            win32::pcwstr(&win32::wide("重新连接 GlazeWM")),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_CONFIG,
            win32::pcwstr(&win32::wide("打开配置目录")),
        );
        let startup_flags = if startup_enabled {
            MENU_ITEM_FLAGS(MF_STRING.0 | MF_CHECKED.0)
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(
            menu,
            startup_flags,
            MENU_STARTUP,
            win32::pcwstr(&win32::wide("开机启动")),
        );
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0x800),
            0,
            windows::core::PCWSTR::null(),
        ); // MF_SEPARATOR
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ABOUT,
            win32::pcwstr(&win32::wide("关于")),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_EXIT,
            win32::pcwstr(&win32::wide("退出")),
        );

        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            at.x,
            at.y,
            Some(0),
            hwnd,
            None,
        );
        // TPM_RETURNCMD returns the id as the BOOL value.
        let selected = cmd.0 as usize;
        let _ = DestroyMenu(menu);
        selected
    }
}
