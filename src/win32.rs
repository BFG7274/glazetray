//! Thin, safe-ish wrappers over the Win32 APIs used by GlazeTray.
//!
//! All `unsafe` usage is confined to this module (plus the two WndProc entry
//! points in `app.rs` / `flyout.rs`).

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, POINT, RECT,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint,
    DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_STATE_FLAGS, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{SHAppBarMessage, Shell_NotifyIconGetRect, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, SystemParametersInfoW, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// Convert a Rust string into a null-terminated UTF-16 buffer.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR::from_raw(buf.as_ptr())
}

/// `HMONITOR` values are pointer-sized; keep them as `isize` so they can be
/// compared against the `hMonitor` field GlazeWM reports over IPC.
pub type HMonitorRaw = isize;

#[derive(Clone, Debug)]
pub struct WinMonitor {
    #[allow(dead_code)]
    pub hmonitor: HMonitorRaw,
    #[allow(dead_code)]
    pub device_name: String,
    pub friendly_name: Option<String>,
    pub rect: RECT,
    #[allow(dead_code)]
    pub work: RECT,
    #[allow(dead_code)]
    pub is_primary: bool,
}

/// Enumerate all monitors and their friendly names.
pub fn enum_monitors() -> Vec<WinMonitor> {
    let mut out = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum_proc),
            LPARAM(&mut out as *mut Vec<WinMonitor> as isize),
        );
    }
    out
}

unsafe extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let out = unsafe { &mut *(data.0 as *mut Vec<WinMonitor>) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() {
        let device_name = unsafe {
            let mut dev = windows::Win32::Graphics::Gdi::MONITORINFOEXW {
                monitorInfo: MONITORINFO {
                    cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFOEXW>()
                        as u32,
                    ..Default::default()
                },
                szDevice: [0u16; 32],
            };
            let _ = GetMonitorInfoW(hmonitor, &mut dev.monitorInfo);
            let s = String::from_utf16_lossy(&dev.szDevice);
            s.trim_end_matches('\0').to_string()
        };
        out.push(WinMonitor {
            hmonitor: hmonitor.0 as isize,
            friendly_name: friendly_name_for_device(&device_name),
            device_name,
            rect: info.rcMonitor,
            work: info.rcWork,
            is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    windows::core::BOOL(1)
}

fn friendly_name_for_device(device_name: &str) -> Option<String> {
    let mut dev = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let name = wide(device_name);
    let ok = unsafe { EnumDisplayDevicesW(pcwstr(&name), 0, &mut dev, 0) }.as_bool();
    if ok && dev.StateFlags & DISPLAY_DEVICE_ACTIVE != DISPLAY_DEVICE_STATE_FLAGS(0) {
        let s = String::from_utf16_lossy(&dev.DeviceString);
        let s = s.trim_end_matches('\0').trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Resolve a friendly display name for a monitor reported by GlazeWM.
///
/// `hmonitor_str` is the `hMonitor` field from the IPC JSON (e.g. `"0x..."`),
/// `rect` is the monitor rect in physical pixels. Matching falls back from the
/// handle to the rect, then to `None` (caller falls back to "显示器 N").
pub fn friendly_name_for_glazewm_monitor(
    device_name: Option<&str>,
    rect: (f64, f64, f64, f64),
) -> Option<String> {
    // Prefer the device name reported by GlazeWM itself.
    if let Some(dn) = device_name
        && let Some(name) = friendly_name_for_device(dn) {
            return Some(name);
        }
    let monitors = enum_monitors();
    if monitors.is_empty() {
        return None;
    }
    let (x, y, w, h) = rect;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let mut best: Option<&WinMonitor> = None;
    let mut best_dist = f64::MAX;
    for m in &monitors {
        let r = m.rect;
        let (mcx, mcy) = (
            r.left as f64 + (r.right - r.left) as f64 / 2.0,
            r.top as f64 + (r.bottom - r.top) as f64 / 2.0,
        );
        let d = (mcx - cx).powi(2) + (mcy - cy).powi(2);
        if d < best_dist {
            best_dist = d;
            best = Some(m);
        }
    }
    best.and_then(|m| m.friendly_name.clone())
}

// DPI
// ---------------------------------------------------------------------------

pub fn dpi_for_window(hwnd: HWND) -> u32 {
    unsafe { GetDpiForWindow(hwnd) }
}

pub fn scale_factor(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

// ---------------------------------------------------------------------------
// Theme (registry based; no WinRT dependency)
// ---------------------------------------------------------------------------

/// `true` when apps should use the light theme (AppsUseLightTheme).
pub fn apps_use_light_theme() -> bool {
    reg_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .map(|v| v != 0)
    .unwrap_or(true)
}

/// Accent color as (r, g, b) from the DWM ColorizationColor value.
pub fn system_accent_color() -> Option<(u8, u8, u8)> {
    let v = reg_dword(r"Software\Microsoft\Windows\DWM", "ColorizationColor")?;
    Some((((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8))
}

/// Whether a high-contrast theme is active.
pub fn high_contrast_enabled() -> bool {
    let mut hc = windows::Win32::UI::Accessibility::HIGHCONTRASTW {
        cbSize: std::mem::size_of::<windows::Win32::UI::Accessibility::HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SYSTEM_PARAMETERS_INFO_ACTION(66), // SPI_GETHIGHCONTRAST
            hc.cbSize,
            Some(&mut hc as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok();
    }
    (hc.dwFlags.0 & 1) != 0 // HCF_HIGHCONTRASTON
}

/// Whether the system animates UI (SPI_GETCLIENTAREAANIMATION).
pub fn animations_enabled() -> bool {
    let mut v: i32 = 1;
    unsafe {
        SystemParametersInfoW(
            SYSTEM_PARAMETERS_INFO_ACTION(0x1042),
            0,
            Some(&mut v as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok();
    }
    v != 0
}

fn reg_dword(subkey: &str, name: &str) -> Option<u32> {
    let mut buf = [0u8; 8];
    let mut len = buf.len() as u32;
    let mut typ = REG_DWORD;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&wide(subkey)),
            pcwstr(&wide(name)),
            windows::Win32::System::Registry::REG_ROUTINE_FLAGS(0x0001), // RRF_RT_REG_DWORD
            Some(&mut typ),
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    if err.is_ok() && len >= 4 {
        Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
    } else {
        None
    }
}

#[allow(dead_code)]
pub fn reg_string(subkey: &str, name: &str) -> Option<String> {
    let mut buf = vec![0u8; 512];
    let mut len = buf.len() as u32;
    let mut typ = REG_SZ;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&wide(subkey)),
            pcwstr(&wide(name)),
            windows::Win32::System::Registry::REG_ROUTINE_FLAGS(0x0002), // RRF_RT_REG_SZ
            Some(&mut typ),
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    if err.is_ok() {
        buf.truncate(len as usize);
        let s = String::from_utf16_lossy(
            &buf
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        Some(s.trim_end_matches('\0').to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tray / taskbar
// ---------------------------------------------------------------------------

/// Returns the screen rect of a GUID-identified tray icon, if available.
pub fn tray_icon_rect(hwnd: HWND, guid: GUID) -> Option<RECT> {
    let id = windows::Win32::UI::Shell::NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<windows::Win32::UI::Shell::NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 0,
        guidItem: guid,
    };
    let rect = unsafe { Shell_NotifyIconGetRect(&id) }.ok()?;
    if rect.left == 0 && rect.top == 0 && rect.right == 0 && rect.bottom == 0 {
        None
    } else {
        Some(rect)
    }
}

pub const ABE_LEFT: u32 = 0;
pub const ABE_TOP: u32 = 1;
#[allow(dead_code)]
pub const ABE_RIGHT: u32 = 2;
pub const ABE_BOTTOM: u32 = 3;

/// Taskbar edge (`ABE_*`), falling back to `ABE_BOTTOM`.
pub fn taskbar_edge() -> u32 {
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let _ = unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) };
    match abd.uEdge {
        0..=3 => abd.uEdge,
        _ => ABE_BOTTOM,
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

pub fn cursor_pos() -> Option<POINT> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    Some(pt)
}

pub fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r) }.ok();
    r
}

pub fn work_area_at(x: i32, y: i32) -> RECT {
    let pt = POINT { x, y };
    let mon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(mon, &mut info) }.as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    }
}

pub fn point_in_rect(pt: POINT, r: RECT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

/// Create a named mutex. Returns `(handle, is_first_instance)`.
pub fn create_single_instance_mutex(name: &str) -> (HANDLE, bool) {
    let handle = unsafe { CreateMutexW(None, false, pcwstr(&wide(name))) }.unwrap_or_default();
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    (handle, !already)
}

pub fn close_handle(handle: HANDLE) {
    if !handle.is_invalid() {
        unsafe { CloseHandle(handle).ok() };
    }
}

pub fn module_file_name() -> Option<String> {
    unsafe {
        let h = GetModuleHandleW(None).ok()?;
        let mut buf = vec![0u16; 2048];
        let n = windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(h), &mut buf);
        if n == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        }
    }
}

/// `SIZE` helper.
#[allow(dead_code)]
pub fn size(w: i32, h: i32) -> windows::Win32::Foundation::SIZE {
    windows::Win32::Foundation::SIZE { cx: w, cy: h }
}
