//! Dynamic tray icon rendering and HICON creation.

use tiny_skia::{Color, Pixmap};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::fonts::{Fonts, MEDIUM, REGULAR};
use crate::render::{
    draw_direction_bar, draw_text_centered, fill_circle, fill_rounded, fit_font_size,
    stroke_rounded,
};
use crate::state::TilingDirection;
use crate::theme::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrayConnection {
    Ready,
    Connecting,
    Disconnected,
    Degraded,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrayIconKey {
    pub label: String,
    pub direction: Option<TilingDirection>,
    pub paused: bool,
    pub connection: TrayConnection,
    pub dark: bool,
    pub size: u32,
}

/// Resolve a workspace name to a short tray label (design §5.4).
pub fn tray_label(name: &str) -> String {
    let drawable = |c: char| c.is_ascii() || c.is_alphanumeric();
    let chars: Vec<char> = name.chars().collect();
    match chars.len() {
        0 => "•".to_string(),
        1 => {
            let c = chars[0];
            if drawable(c) {
                c.to_string()
            } else {
                "▣".to_string()
            }
        }
        2 => {
            if chars.iter().all(|c| drawable(*c)) {
                name.to_string()
            } else {
                "▣".to_string()
            }
        }
        _ => {
            let c = chars[0];
            if c.is_alphanumeric() || c.is_ascii_punctuation() {
                c.to_string()
            } else {
                "▣".to_string()
            }
        }
    }
}

/// Render the tray icon RGBA buffer (premultiplied) for a key.
pub fn render_tray_icon(key: &TrayIconKey, palette: &Palette, fonts: &Fonts) -> Vec<u8> {
    let s = key.size.max(8) as f32;
    let mut px = Pixmap::new(s as u32, s as u32).expect("tray pixmap");

    // Background plate.
    let bg = if key.dark {
        Color::from_rgba8(0x2B, 0x2B, 0x2B, 0xE8)
    } else {
        Color::from_rgba8(0xF3, 0xF3, 0xF3, 0xE8)
    };
    fill_rounded(&mut px, 0.5, 0.5, s - 1.0, s - 1.0, s * 0.24, bg, None);
    stroke_rounded(
        &mut px,
        0.5,
        0.5,
        s - 1.0,
        s - 1.0,
        s * 0.24,
        palette.border,
        (s * 0.05).max(0.5),
        None,
    );

    // Label.
    let label = &key.label;
    let has_glyph = label
        .chars()
        .all(|c| fonts.has_glyph(REGULAR, c) || fonts.has_glyph(MEDIUM, c));
    let shown = if has_glyph { label.as_str() } else { "▣" };
    let font = fonts.get(MEDIUM);
    let max_w = s * 0.72;
    let size = fit_font_size(font, MEDIUM, shown, max_w, s * 0.52, s * 0.3);
    draw_text_centered(
        &mut px,
        font,
        MEDIUM,
        size,
        shown,
        s * 0.14,
        s * 0.10,
        s * 0.72,
        s * 0.62,
        palette.text_primary,
        None,
    );

    // Direction mark (second visual signal).
    let dim = matches!(
        key.connection,
        TrayConnection::Disconnected | TrayConnection::Degraded
    );
    if key.paused {
        let bar_w = (s * 0.10).max(1.2);
        let bar_h = s * 0.32;
        let y = s * 0.54;
        for x in [s * 0.38, s * 0.62] {
            fill_rounded(
                &mut px,
                x - bar_w / 2.0,
                y,
                bar_w,
                bar_h,
                bar_w / 2.0,
                palette.warning,
                None,
            );
        }
    } else if let Some(dir) = key.direction {
        let color = if dim {
            palette.text_disabled
        } else {
            palette.accent
        };
        let angle = match dir {
            TilingDirection::Horizontal => 0.0,
            TilingDirection::Vertical => std::f32::consts::FRAC_PI_2,
        };
        draw_direction_bar(&mut px, s / 2.0, s * 0.60, s * 0.55, angle, color, None);
    }

    // Connection warning dot.
    match key.connection {
        TrayConnection::Ready => {}
        TrayConnection::Connecting => {
            fill_circle(
                &mut px,
                s * 0.22,
                s * 0.22,
                s * 0.09,
                palette.text_secondary,
                None,
            );
        }
        TrayConnection::Disconnected | TrayConnection::Degraded => {
            fill_circle(&mut px, s * 0.22, s * 0.22, s * 0.09, palette.error, None);
        }
    }

    px.data().to_vec()
}

/// Create an HICON from a premultiplied RGBA buffer.
pub fn create_hicon(rgba: &[u8], w: u32, h: u32) -> anyhow::Result<HICON> {
    unsafe {
        let dc = CreateCompatibleDC(None);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color_bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| anyhow::anyhow!("CreateDIBSection: {e}"))?;

        // Convert premultiplied RGBA to premultiplied BGRA.
        let bgra = crate::render::rgba_to_bgra_premultiplied(rgba);
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());

        // Monochrome AND mask, all zeros (alpha channel of the color bitmap
        // drives transparency).
        let mask = create_mono_mask(w, h)?;

        let icon_info = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color_bmp,
        };
        let icon = CreateIconIndirect(&icon_info).map_err(|e| {
            delete_hbitmap(color_bmp);
            delete_hbitmap(mask);
            anyhow::anyhow!("CreateIconIndirect: {e}")
        })?;
        delete_hbitmap(color_bmp);
        delete_hbitmap(mask);
        let _ = DeleteDC(dc);
        Ok(icon)
    }
}

fn delete_hbitmap(bmp: HBITMAP) {
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(
            windows::Win32::Graphics::Gdi::HGDIOBJ(bmp.0),
        );
    }
}

fn create_mono_mask(w: u32, h: u32) -> windows::core::Result<HBITMAP> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 1,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        // Zero-fill the 1bpp buffer (bytes per row rounded to 4).
        let row_bytes = w.div_ceil(32) * 4;
        std::ptr::write_bytes(bits as *mut u8, 0, (row_bytes * h) as usize);
        let _ = DeleteDC(dc);
        Ok(bmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_degradation() {
        assert_eq!(tray_label("1"), "1");
        assert_eq!(tray_label("2"), "2");
        assert_eq!(tray_label("10"), "10");
        assert_eq!(tray_label("工作"), "工作");
        assert_eq!(tray_label("工作区"), "工");
        assert_eq!(tray_label("mail"), "m");
        assert_eq!(tray_label(""), "•");
        assert_eq!(tray_label("💻"), "▣");
    }

    #[test]
    fn renders_all_sizes() {
        let fonts = Fonts::load();
        for dark in [true, false] {
            for size in [16, 20, 24, 32, 40, 48] {
                let key = TrayIconKey {
                    label: "3".into(),
                    direction: Some(TilingDirection::Vertical),
                    paused: false,
                    connection: TrayConnection::Ready,
                    dark,
                    size,
                };
                let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
                    dark,
                    accent: None,
                    use_system_accent: false,
                    high_contrast: false,
                });
                let buf = render_tray_icon(&key, &pal, &fonts);
                assert_eq!(buf.len(), (size * size * 4) as usize);
                // Some pixel must be non-transparent.
                assert!(buf.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
            }
        }
    }

    #[test]
    fn renders_connection_states() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        for conn in [
            TrayConnection::Ready,
            TrayConnection::Connecting,
            TrayConnection::Disconnected,
            TrayConnection::Degraded,
        ] {
            let key = TrayIconKey {
                label: "2".into(),
                direction: Some(TilingDirection::Horizontal),
                paused: false,
                connection: conn,
                dark: true,
                size: 24,
            };
            let buf = render_tray_icon(&key, &pal, &fonts);
            assert!(buf.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
        }
    }

    #[test]
    fn creates_valid_hicon() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let key = TrayIconKey {
            label: "3".into(),
            direction: Some(TilingDirection::Vertical),
            paused: false,
            connection: TrayConnection::Ready,
            dark: true,
            size: 32,
        };
        let buf = render_tray_icon(&key, &pal, &fonts);
        let icon = create_hicon(&buf, 32, 32).expect("hicon");
        assert!(!icon.is_invalid());
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon).ok();
        }
    }
}
