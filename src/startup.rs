//! Startup (autostart) support via the current-user Run registry key.

use windows::Win32::System::Registry::{
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_SZ,
};

use crate::win32;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "GlazeTray";

pub fn startup_enabled() -> bool {
    unsafe {
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            win32::pcwstr(&win32::wide(RUN_KEY)),
            None,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }
        let mut buf = [0u8; 1024];
        let mut len = buf.len() as u32;
        let mut typ = REG_SZ;
        let err = RegQueryValueExW(
            hkey,
            win32::pcwstr(&win32::wide(VALUE_NAME)),
            None,
            Some(&mut typ),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        err.is_ok()
    }
}

pub fn set_startup(enabled: bool) -> std::io::Result<()> {
    unsafe {
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            win32::pcwstr(&win32::wide(RUN_KEY)),
            None,
            None,
            windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_err()
        {
            return Err(std::io::Error::last_os_error());
        }
        let result = if enabled {
            let exe = win32::module_file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "无法获取程序路径")
            })?;
            // Quote the path: Run keys parse command lines.
            let cmd = format!("\"{exe}\"");
            let bytes = cmd.encode_utf16().flat_map(|c| c.to_le_bytes()).collect::<Vec<_>>();
            RegSetValueExW(
                hkey,
                win32::pcwstr(&win32::wide(VALUE_NAME)),
                None,
                REG_SZ,
                Some(&bytes),
            )
        } else {
            RegDeleteValueW(hkey, win32::pcwstr(&win32::wide(VALUE_NAME)))
        };
        let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        if result.is_ok() {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}
