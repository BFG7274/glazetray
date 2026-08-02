//! Low-memory text rendering with DirectWrite.
//!
//! DirectWrite performs shaping, hinting, and per-glyph system font fallback
//! without parsing an entire CJK collection into this process. Text is drawn
//! into a small transparent DIB through a Direct2D DC render target, then its
//! alpha is composited into the existing tiny-skia surface.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use tiny_skia::{Color, Mask, Pixmap};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory,
    ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection,
    IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::core::{BOOL, PCWSTR, w};
use windows_numerics::Vector2;

pub const REGULAR: usize = 0;
pub const MEDIUM: usize = 1;

fn weight_for(index: usize) -> DWRITE_FONT_WEIGHT {
    match index {
        MEDIUM => DWRITE_FONT_WEIGHT_SEMI_BOLD,
        _ => DWRITE_FONT_WEIGHT_NORMAL,
    }
}

const FAMILY_CANDIDATES: &[&str] = &["Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI"];

pub struct Fonts {
    engine: TextEngine,
    #[allow(dead_code)]
    pub sources: Vec<String>,
}

impl Fonts {
    pub fn load() -> Self {
        let (engine, family) = TextEngine::new();
        tracing::info!(family, "fonts loaded (DirectWrite)");
        Self {
            engine,
            sources: vec![format!("{family} + DirectWrite fallback")],
        }
    }

    pub fn get(&self, _index: usize) -> &TextEngine {
        &self.engine
    }

    pub fn has_glyph(&self, _index: usize, ch: char) -> bool {
        !ch.is_control()
    }
}

pub struct TextEngine {
    dwrite: IDWriteFactory,
    target: ID2D1DCRenderTarget,
    white_brush: ID2D1SolidColorBrush,
    dc: HDC,
    bitmap: Cell<HBITMAP>,
    stock_bitmap: Cell<HGDIOBJ>,
    bits: Cell<*mut u8>,
    buffer_width: Cell<usize>,
    buffer_height: Cell<usize>,
    formats: RefCell<HashMap<(i32, i32), IDWriteTextFormat>>,
    family: String,
}

impl TextEngine {
    fn new() -> (Self, String) {
        let dwrite: IDWriteFactory = unsafe {
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DirectWrite factory")
        };
        let family = choose_family(&dwrite);
        let d2d: ID2D1Factory = unsafe {
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).expect("Direct2D factory")
        };
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = unsafe {
            d2d.CreateDCRenderTarget(&properties)
                .expect("Direct2D DC render target")
        };
        unsafe {
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        }
        let white = D2D1_COLOR_F {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        };
        let white_brush = unsafe {
            target
                .CreateSolidColorBrush(&white, None)
                .expect("Direct2D text brush")
        };
        let dc = unsafe { CreateCompatibleDC(None) };
        let engine = Self {
            dwrite,
            target,
            white_brush,
            dc,
            bitmap: Cell::new(HBITMAP::default()),
            stock_bitmap: Cell::new(HGDIOBJ::default()),
            bits: Cell::new(std::ptr::null_mut()),
            buffer_width: Cell::new(0),
            buffer_height: Cell::new(0),
            formats: RefCell::new(HashMap::new()),
            family: family.clone(),
        };
        engine.ensure_buffer(64, 64);
        (engine, family)
    }

    fn format(&self, size_px: f32, weight: DWRITE_FONT_WEIGHT) -> IDWriteTextFormat {
        let size = size_px.round().max(1.0) as i32;
        let key = (size, weight.0);
        if let Some(format) = self.formats.borrow().get(&key) {
            return format.clone();
        }

        let family: Vec<u16> = self.family.encode_utf16().chain(Some(0)).collect();
        let format = unsafe {
            self.dwrite
                .CreateTextFormat(
                    PCWSTR(family.as_ptr()),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size as f32,
                    w!("zh-CN"),
                )
                .expect("DirectWrite text format")
        };
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        }
        self.formats.borrow_mut().insert(key, format.clone());
        format
    }

    fn layout(&self, text: &str, size_px: f32, weight: DWRITE_FONT_WEIGHT) -> IDWriteTextLayout {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let format = self.format(size_px, weight);
        unsafe {
            self.dwrite
                .CreateTextLayout(&utf16, &format, 4096.0, 256.0)
                .expect("DirectWrite text layout")
        }
    }

    fn metrics(layout: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe {
            layout
                .GetMetrics(&mut metrics)
                .expect("DirectWrite text metrics");
        }
        metrics
    }

    pub fn measure(&self, weight: usize, text: &str, size_px: f32) -> (f32, f32) {
        let layout = self.layout(text, size_px, weight_for(weight));
        let metrics = Self::metrics(&layout);
        (metrics.widthIncludingTrailingWhitespace, metrics.height)
    }

    pub fn line_height(&self, weight: usize, size_px: f32) -> f32 {
        self.measure(weight, "Mg显示", size_px).1
    }

    fn ensure_buffer(&self, width: usize, height: usize) {
        if width <= self.buffer_width.get() && height <= self.buffer_height.get() {
            return;
        }
        let new_width = width.max(self.buffer_width.get() * 2).max(64);
        let new_height = height.max(self.buffer_height.get() * 2).max(64);
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: new_width as i32,
                biHeight: -(new_height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let new_bitmap = unsafe {
            CreateDIBSection(
                Some(self.dc),
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )
            .expect("text DIB section")
        };
        let old_selected = unsafe { SelectObject(self.dc, HGDIOBJ(new_bitmap.0)) };
        let previous = self.bitmap.replace(new_bitmap);
        if previous.is_invalid() {
            self.stock_bitmap.set(old_selected);
        } else if old_selected.0 == previous.0 {
            unsafe {
                let _ = DeleteObject(old_selected);
            }
        }
        self.bits.set(bits.cast());
        self.buffer_width.set(new_width);
        self.buffer_height.set(new_height);
    }

    pub fn draw_into(
        &self,
        destination: &mut Pixmap,
        weight: usize,
        text: &str,
        size_px: f32,
        x: f32,
        y: f32,
        color: Color,
        mask: Option<&Mask>,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let layout = self.layout(text, size_px, weight_for(weight));
        let metrics = Self::metrics(&layout);
        let render_width = metrics.widthIncludingTrailingWhitespace.ceil() as usize + 8;
        let render_height = metrics.height.ceil() as usize + 8;
        self.ensure_buffer(render_width, render_height);
        unsafe {
            std::ptr::write_bytes(
                self.bits.get(),
                0,
                self.buffer_width.get() * self.buffer_height.get() * 4,
            );
            let bounds = RECT {
                left: 0,
                top: 0,
                right: render_width as i32,
                bottom: render_height as i32,
            };
            self.target.BindDC(self.dc, &bounds).expect("bind text DC");
            self.target.BeginDraw();
            self.target.Clear(Some(&D2D1_COLOR_F::default()));
            self.target.DrawTextLayout(
                Vector2 { X: 0.0, Y: 0.0 },
                &layout,
                &self.white_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            self.target
                .EndDraw(None, None)
                .expect("draw DirectWrite text");
        }

        let stride = self.buffer_width.get();
        let bits = self.bits.get();
        let alpha_at =
            |row: usize, column: usize| unsafe { *bits.add((row * stride + column) * 4 + 3) };
        let mut min_x = render_width;
        let mut min_y = render_height;
        let mut max_x = 0;
        let mut max_y = 0;
        for row in 0..render_height {
            for column in 0..render_width {
                if alpha_at(row, column) > 2 {
                    min_x = min_x.min(column);
                    min_y = min_y.min(row);
                    max_x = max_x.max(column);
                    max_y = max_y.max(row);
                }
            }
        }
        if min_x > max_x || min_y > max_y {
            return metrics.widthIncludingTrailingWhitespace;
        }

        composite_alpha(
            destination,
            bits,
            stride,
            (min_x, min_y, max_x + 1, max_y + 1),
            x.floor() as i32,
            y.floor() as i32,
            color,
            mask,
        );
        metrics.widthIncludingTrailingWhitespace
    }
}

fn choose_family(factory: &IDWriteFactory) -> String {
    let mut collection: Option<IDWriteFontCollection> = None;
    if unsafe { factory.GetSystemFontCollection(&mut collection, false) }.is_ok()
        && let Some(collection) = collection
    {
        for candidate in FAMILY_CANDIDATES {
            let wide: Vec<u16> = candidate.encode_utf16().chain(Some(0)).collect();
            let mut index = 0;
            let mut exists = BOOL::default();
            if unsafe { collection.FindFamilyName(PCWSTR(wide.as_ptr()), &mut index, &mut exists) }
                .is_ok()
                && exists.as_bool()
            {
                return (*candidate).to_string();
            }
        }
    }
    "Segoe UI".to_string()
}

#[allow(clippy::too_many_arguments)]
fn composite_alpha(
    destination: &mut Pixmap,
    source: *const u8,
    source_stride: usize,
    bounds: (usize, usize, usize, usize),
    destination_x: i32,
    destination_y: i32,
    color: Color,
    mask: Option<&Mask>,
) {
    let (min_x, min_y, max_x, max_y) = bounds;
    let width = destination.width() as i32;
    let height = destination.height() as i32;
    let pixels = destination.pixels_mut();
    for row in min_y..max_y {
        let py = destination_y + row as i32;
        if py < 0 || py >= height {
            continue;
        }
        for column in min_x..max_x {
            let px = destination_x + column as i32;
            if px < 0 || px >= width {
                continue;
            }
            let destination_index = py as usize * width as usize + px as usize;
            if mask.is_some_and(|value| value.data()[destination_index] == 0) {
                continue;
            }
            let coverage =
                unsafe { *source.add((row * source_stride + column) * 4 + 3) } as f32 / 255.0;
            if coverage <= 0.0 {
                continue;
            }
            let destination_pixel = &mut pixels[destination_index];
            let destination_alpha = destination_pixel.alpha() as f32 / 255.0;
            let source_alpha = color.alpha() * coverage;
            let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
            if output_alpha <= 0.0 {
                continue;
            }
            let blend = |destination: f32, source: f32| {
                (source * source_alpha + destination * destination_alpha * (1.0 - source_alpha))
                    / output_alpha
            };
            *destination_pixel = tiny_skia::PremultipliedColorU8::from_rgba(
                (blend(destination_pixel.red() as f32 / 255.0, color.red()) * 255.0).round() as u8,
                (blend(destination_pixel.green() as f32 / 255.0, color.green()) * 255.0).round()
                    as u8,
                (blend(destination_pixel.blue() as f32 / 255.0, color.blue()) * 255.0).round()
                    as u8,
                (output_alpha * 255.0).round() as u8,
            )
            .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
        }
    }
}

impl Drop for TextEngine {
    fn drop(&mut self) {
        unsafe {
            let bitmap = self.bitmap.get();
            if !bitmap.is_invalid() {
                let stock = self.stock_bitmap.get();
                if !stock.is_invalid() {
                    let _ = SelectObject(self.dc, stock);
                }
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

// Rendering is confined to the UI thread. The explicit bounds allow Fonts to
// remain inside the app's Arc-owned render input without widening that API.
unsafe impl Send for TextEngine {}
unsafe impl Sync for TextEngine {}
