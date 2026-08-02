//! Theme palette computation: dark/light, system accent, high contrast.

use tiny_skia::Color;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub surface: Color,
    pub surface_alt: Color,
    pub border: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_disabled: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub error: Color,
    pub warning: Color,
    pub shadow: Color,
    #[allow(dead_code)]
    pub dark: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ThemeInput {
    #[allow(dead_code)]
    pub dark: bool,
    pub accent: Option<(u8, u8, u8)>,
    pub use_system_accent: bool,
    pub high_contrast: bool,
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color::from_rgba8(r, g, b, a)
}

/// WCAG relative luminance (0..=1).
fn luminance(c: Color) -> f32 {
    let f = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (c.red(), c.green(), c.blue());
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = (la.max(lb), la.min(lb));
    (hi + 0.05) / (lo + 0.05)
}

/// Adjust accent brightness until it reaches at least `target` contrast
/// against `text` (usually `accent_text`).
pub fn ensure_contrast(accent: Color, text: Color, target: f32, dark: bool) -> Color {
    let mut c = accent;
    for _ in 0..24 {
        if contrast_ratio(c, text) >= target {
            break;
        }
        // Lighten in dark mode, darken in light mode.
        let step = if dark { 0.06 } else { -0.06 };
        let (mut r, mut g, mut b) = (c.red(), c.green(), c.blue());
        r = (r + step).clamp(0.0, 1.0);
        g = (g + step).clamp(0.0, 1.0);
        b = (b + step).clamp(0.0, 1.0);
        let alpha = c.alpha();
        c = Color::from_rgba(r, g, b, alpha).unwrap_or(c);
    }
    c
}

/// System colors for high-contrast mode.
fn hc_palette(dark: bool) -> Palette {
    // COLOR_WINDOW=5, COLOR_WINDOWTEXT=8, COLOR_HIGHLIGHT=13, COLOR_HIGHLIGHTTEXT=14
    use windows::Win32::Graphics::Gdi::{GetSysColor, SYS_COLOR_INDEX};
    let get = |i: u32| {
        let v = unsafe { GetSysColor(SYS_COLOR_INDEX(i as i32)) };
        let r = (v & 0xFF) as u8;
        let g = ((v >> 8) & 0xFF) as u8;
        let b = ((v >> 16) & 0xFF) as u8;
        rgba(r, g, b, 255)
    };
    let surface = get(5);
    let text = get(8);
    let accent = get(13);
    let accent_text = get(14);
    Palette {
        surface,
        surface_alt: surface,
        border: text,
        text_primary: text,
        text_secondary: text,
        text_disabled: text,
        accent,
        accent_text,
        error: text,
        warning: text,
        shadow: rgba(0, 0, 0, 0),
        dark,
    }
}

pub fn compute_palette(input: ThemeInput) -> Palette {
    if input.high_contrast {
        return hc_palette(input.dark);
    }

    let (
        surface,
        surface_alt,
        border,
        text_primary,
        text_secondary,
        text_disabled,
        accent,
        accent_text,
        error,
        warning,
    ) = if input.dark {
        (
            rgba(0x20, 0x20, 0x20, 0xF2),
            rgba(0x2B, 0x2B, 0x2B, 0xF2),
            rgba(0xFF, 0xFF, 0xFF, 0x14),
            rgba(0xFF, 0xFF, 0xFF, 0xFF),
            rgba(0xCF, 0xCF, 0xCF, 0xFF),
            rgba(0x8A, 0x8A, 0x8A, 0xFF),
            rgba(0x60, 0xCD, 0xFF, 0xFF),
            rgba(0x00, 0x1F, 0x2A, 0xFF),
            rgba(0xFF, 0x99, 0xA4, 0xFF),
            rgba(0xFF, 0xB9, 0x00, 0xFF),
        )
    } else {
        (
            rgba(0xF7, 0xF7, 0xF7, 0xF2),
            rgba(0xFF, 0xFF, 0xFF, 0xF2),
            rgba(0x00, 0x00, 0x00, 0x14),
            rgba(0x1A, 0x1A, 0x1A, 0xFF),
            rgba(0x5D, 0x5D, 0x5D, 0xFF),
            rgba(0x92, 0x92, 0x92, 0xFF),
            rgba(0x00, 0x67, 0xC0, 0xFF),
            rgba(0xFF, 0xFF, 0xFF, 0xFF),
            rgba(0xC4, 0x2B, 0x1C, 0xFF),
            rgba(0x8A, 0x4F, 0x00, 0xFF),
        )
    };

    let mut accent = accent;
    if input.use_system_accent
        && let Some((r, g, b)) = input.accent
    {
        accent = rgba(r, g, b, 0xFF);
    }
    let accent = ensure_contrast(accent, accent_text, 4.5, input.dark);

    Palette {
        surface,
        surface_alt,
        border,
        text_primary,
        text_secondary,
        text_disabled,
        accent,
        accent_text,
        error,
        warning,
        shadow: rgba(0, 0, 0, if input.dark { 0x59 } else { 0x40 }),
        dark: input.dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_defaults_have_contrast() {
        for dark in [true, false] {
            let p = compute_palette(ThemeInput {
                dark,
                accent: None,
                use_system_accent: false,
                high_contrast: false,
            });
            assert!(
                contrast_ratio(p.text_primary, p.surface) >= 4.5,
                "primary text contrast (dark={dark})"
            );
            assert!(
                contrast_ratio(p.accent, p.accent_text) >= 4.5,
                "accent contrast (dark={dark})"
            );
            assert!(
                contrast_ratio(p.text_secondary, p.surface) >= 3.0,
                "secondary text contrast (dark={dark})"
            );
        }
    }

    #[test]
    fn weak_system_accent_is_corrected() {
        // A very light accent in light mode must be darkened for white text.
        let p = compute_palette(ThemeInput {
            dark: false,
            accent: Some((255, 250, 240)),
            use_system_accent: true,
            high_contrast: false,
        });
        assert!(contrast_ratio(p.accent, p.accent_text) >= 4.5);
        // A very dark accent in dark mode must be lightened for dark text.
        let p2 = compute_palette(ThemeInput {
            dark: true,
            accent: Some((0, 0, 10)),
            use_system_accent: true,
            high_contrast: false,
        });
        assert!(contrast_ratio(p2.accent, p2.accent_text) >= 4.5);
    }

    #[test]
    fn high_contrast_is_opaque_and_high_contrast() {
        let p = compute_palette(ThemeInput {
            dark: false,
            accent: None,
            use_system_accent: true,
            high_contrast: true,
        });
        // High-contrast themes guarantee text/surface contrast themselves;
        // accent pairs may be identical in some schemes (e.g. black/white).
        assert!(contrast_ratio(p.text_primary, p.surface) >= 4.5);
        assert_eq!(p.surface.alpha(), 1.0);
    }
}
