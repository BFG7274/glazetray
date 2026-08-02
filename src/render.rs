//! Drawing primitives on top of tiny-skia + DirectWrite text masks.

use tiny_skia::Color;
use tiny_skia::{FillRule, Mask, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

fn solid(color: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color);
    p
}

/// Build a rounded-rect path.
pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

pub fn fill_rounded(
    px: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    mask: Option<&Mask>,
) {
    if let Some(path) = rounded_rect(x, y, w, h, r) {
        px.fill_path(&path, &solid(color), FillRule::Winding, Transform::identity(), mask);
    }
}

pub fn stroke_rounded(
    px: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    width: f32,
    mask: Option<&Mask>,
) {
    if let Some(path) = rounded_rect(x, y, w, h, r) {
        let stroke = Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            miter_limit: 1.0,
            dash: None,
        };
        px.stroke_path(&path, &solid(color), &stroke, Transform::identity(), mask);
    }
}

pub fn fill_circle(px: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color, mask: Option<&Mask>) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        px.fill_path(&path, &solid(color), FillRule::Winding, Transform::identity(), mask);
    }
}

// ---------------------------------------------------------------------------
// Text (DirectWrite engine; see fonts.rs)
// ---------------------------------------------------------------------------

use crate::fonts::TextEngine;

pub fn measure_text(fonts: &TextEngine, weight: usize, text: &str, size: f32) -> f32 {
    fonts.measure(weight, text, size).0
}

pub fn text_height(fonts: &TextEngine, weight: usize, size: f32) -> f32 {
    fonts.line_height(weight, size)
}

/// Draw text with the DirectWrite layout box at (x, y). Returns its width.
pub fn draw_text(
    px: &mut Pixmap,
    fonts: &TextEngine,
    weight: usize,
    size: f32,
    text: &str,
    x: f32,
    y: f32,
    color: Color,
    mask: Option<&Mask>,
) -> f32 {
    fonts.draw_into(px, weight, text, size, x, y, color, mask)
}

/// Draw text centered horizontally in [x, x+w], vertically centered in
/// [y, y+h]. Returns the text width.
pub fn draw_text_centered(
    px: &mut Pixmap,
    fonts: &TextEngine,
    weight: usize,
    size: f32,
    text: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: Color,
    mask: Option<&Mask>,
) -> f32 {
    let tw = measure_text(fonts, weight, text, size);
    let th = text_height(fonts, weight, size);
    let tx = x + (w - tw) / 2.0;
    let ty = y + (h - th) / 2.0;
    draw_text(px, fonts, weight, size, text, tx, ty, color, mask);
    tw
}

/// Largest font size (from `start`, stepping down to `min`) at which `text`
/// fits into `max_width`.
pub fn fit_font_size(
    fonts: &TextEngine,
    weight: usize,
    text: &str,
    max_width: f32,
    start: f32,
    min: f32,
) -> f32 {
    let mut size = start;
    while size > min {
        if measure_text(fonts, weight, text, size) <= max_width {
            return size;
        }
        size -= 1.0;
    }
    size.max(min)
}

/// Draw a tiling-direction glyph: a bar along the bottom (horizontal, angle 0)
/// or the right edge (vertical, angle π/2). `angle` is in radians.
pub fn draw_direction_bar(
    px: &mut Pixmap,
    cx: f32,
    cy: f32,
    size: f32,
    angle: f32,
    color: Color,
    mask: Option<&Mask>,
) {
    let margin = size * 0.22;
    let bar_len = size - margin * 2.0;
    let bar_thick = (size * 0.14).max(1.5);
    let x = cx - bar_len / 2.0;
    let y = cy + size / 2.0 - margin - bar_thick; // horizontal bar sits at the bottom
    let mut pb = PathBuilder::new();
    pb.push_rect(Rect::from_xywh(x, y, bar_len, bar_thick).unwrap());
    if let Some(path) = pb.finish()
        && let Some(rotated) = path.transform(Transform::from_rotate_at(angle, cx, cy)) {
            px.fill_path(
                &rotated,
                &solid(color),
                FillRule::Winding,
                Transform::identity(),
                mask,
            );
        }
}

// ---------------------------------------------------------------------------
// Shadow
// ---------------------------------------------------------------------------

/// Soft shadow: fills `content` rounded rect into a mask, box-blurs the alpha,
/// then composites the shadow color.
pub fn draw_shadow(
    px: &mut Pixmap,
    content: (f32, f32, f32, f32),
    radius: f32,
    spread: f32,
    color: Color,
) {
    if color.alpha() <= 0.0 || spread <= 0.0 {
        return;
    }
    let (w, h) = (px.width(), px.height());
    let (cx, cy, cw, ch) = content;
    let mut mask = Pixmap::new(w, h).expect("shadow mask");
    mask.fill_rect(
        Rect::from_xywh(cx, cy, cw, ch).unwrap(),
        &solid(Color::WHITE),
        Transform::identity(),
        None,
    );
    blur_alpha(&mut mask, radius as u32);

    let x0 = (cx - spread).floor().max(0.0) as i32;
    let y0 = (cy - spread).floor().max(0.0) as i32;
    let x1 = (cx + cw + spread).ceil().min(w as f32) as i32;
    let y1 = (cy + ch + spread).ceil().min(h as f32) as i32;
    for y in y0..y1 {
        for x in x0..x1 {
            let a = mask.pixel(x as u32, y as u32).map(|p| p.alpha() as f32 / 255.0).unwrap_or(0.0);
            if a <= 0.0 {
                continue;
            }
            let src_a = color.alpha() * a;
            let dst = &mut px.pixels_mut()[y as usize * w as usize + x as usize];
            let da = dst.alpha() as f32 / 255.0;
            let out_a = src_a + da * (1.0 - src_a);
            if out_a <= 0.0 {
                continue;
            }
            let blend = |d: f32, s: f32| (s * src_a + d * da * (1.0 - src_a)) / out_a;
            let (r, g, b) = (
                blend(dst.red() as f32 / 255.0, color.red()),
                blend(dst.green() as f32 / 255.0, color.green()),
                blend(dst.blue() as f32 / 255.0, color.blue()),
            );
            *dst = tiny_skia::PremultipliedColorU8::from_rgba(
                (r * 255.0).round() as u8,
                (g * 255.0).round() as u8,
                (b * 255.0).round() as u8,
                (out_a * 255.0).round() as u8,
            )
            .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
        }
    }
}

/// Separable box blur over the alpha channel (in place).
fn blur_alpha(px: &mut Pixmap, radius: u32) {
    if radius == 0 {
        return;
    }
    let (w, h) = (px.width() as usize, px.height() as usize);
    if w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0u8; w * h];
    let alpha = |i: usize| px.pixel((i % w) as u32, (i / w) as u32).map(|p| p.alpha()).unwrap_or(0);
    // horizontal pass
    let r = radius as usize;
    for row in 0..h {
        let mut acc: u32 = 0;
        let base = row * w;
        for i in 0..(r.min(w)) {
            acc += alpha(base + i) as u32;
        }
        for col in 0..w {
            let add = col + r;
            let sub = col as i64 - r as i64 - 1;
            if add < w {
                acc += alpha(base + add) as u32;
            }
            if sub >= 0 {
                acc -= alpha(base + sub as usize) as u32;
            }
            let win = (r * 2 + 1).min(w) as u32;
            tmp[base + col] = (acc / win) as u8;
        }
    }
    // vertical pass, writing back into the pixmap
    for col in 0..w {
        let mut acc: u32 = 0;
        for i in 0..(r.min(h)) {
            acc += tmp[i * w + col] as u32;
        }
        for row in 0..h {
            let add = row + r;
            let sub = row as i64 - r as i64 - 1;
            if add < h {
                acc += tmp[add * w + col] as u32;
            }
            if sub >= 0 {
                acc -= tmp[sub as usize * w + col] as u32;
            }
            let win = (r * 2 + 1).min(h) as u32;
            let a = (acc / win) as u8;
            let p = &mut px.pixels_mut()[row * w + col];
            *p = tiny_skia::PremultipliedColorU8::from_rgba(p.red(), p.green(), p.blue(), a)
                .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Convert premultiplied RGBA (tiny-skia layout) to premultiplied BGRA, which
/// is what `UpdateLayeredWindow` expects.
pub fn rgba_to_bgra_premultiplied(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
        out.push(px[3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::Fonts;

    #[test]
    fn text_bitmap_is_positioned_above_the_baseline() {
        let fonts = Fonts::load();
        let engine = fonts.get(crate::fonts::MEDIUM);
        let mut px = Pixmap::new(120, 48).unwrap();
        draw_text(&mut px, engine, crate::fonts::MEDIUM, 18.0, "Generic", 2.0, 0.0, Color::WHITE, None);

        let ys: Vec<u32> = (0..px.height())
            .filter(|y| (0..px.width()).any(|x| px.pixel(x, *y).unwrap().alpha() > 0))
            .collect();
        assert!(!ys.is_empty());
        assert!(ys[0] < 8, "glyph starts too far below its top: {}", ys[0]);
    }
}
