//! Slot image rendering: circular avatar, green ring while speaking, dimmed
//! (desaturated and translucent) while muted or deafened, and the `+K`
//! overflow badge (drawn with a tiny built-in digit bitmap, no fonts).
//!
//! Pure functions: bytes in, PNG bytes out. No I/O.

use std::io::Cursor;

use image::{imageops, ImageFormat, ImageReader, Limits, RgbaImage};
use tiny_skia::{
    Color, ColorU8, FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, Rect, Shader,
    SpreadMode, Stroke, Transform,
};

/// Default width and height of every rendered slot image, in pixels.
pub const DEFAULT_SIZE: u32 = 64;
/// Discord's "speaking" green.
pub const DEFAULT_RING_COLOR: (u8, u8, u8) = (0x23, 0xa5, 0x5a);
/// Default opacity of the avatar while muted or deafened.
pub const DEFAULT_DIM: f32 = 0.4;
/// Fill used when no avatar is available (or it cannot be decoded), and
/// for the overflow badge.
const PLACEHOLDER_COLOR: (u8, u8, u8) = (0x4e, 0x50, 0x58);
/// Overflow badge text color.
const BADGE_TEXT_COLOR: (u8, u8, u8) = (0xff, 0xff, 0xff);
/// The badge shows at most this count (the tooltip lists every name).
const MAX_BADGE_COUNT: usize = 99;
/// Source avatars larger than this are rejected (decompression bombs).
const MAX_SOURCE_DIM: u32 = 1024;

/// A 1x1 fully transparent PNG, used when rendering fails.
pub const TRANSPARENT_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0xe9, 0xfa, 0xdc, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

/// Configurable look (the `size`, `speaking_color` and `dim` settings).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderConfig {
    /// Image width and height in pixels.
    pub size: u32,
    pub speaking_color: (u8, u8, u8),
    /// Avatar opacity while muted or deafened (0.0..=1.0).
    pub dim: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            size: DEFAULT_SIZE,
            speaking_color: DEFAULT_RING_COLOR,
            dim: DEFAULT_DIM,
        }
    }
}

impl RenderConfig {
    /// Speaking ring width: 4 px at 64 px, scaled with the size.
    fn ring(&self) -> u32 {
        (self.size / 16).max(1)
    }

    /// Avatars are drawn inside the ring so the image never changes size.
    fn avatar_diameter(&self) -> u32 {
        self.size - 2 * self.ring()
    }
}

/// Visual state of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub speaking: bool,
    /// Muted or deafened.
    pub dimmed: bool,
}

/// Renders a `size`x`size` PNG for an avatar (PNG bytes; `None` or
/// undecodable data gives a plain placeholder circle). Never panics; falls
/// back to [`TRANSPARENT_PNG`] if encoding fails.
pub fn render_slot(avatar_png: Option<&[u8]>, style: Style, cfg: &RenderConfig) -> Vec<u8> {
    let avatar = avatar_png.and_then(|b| match decode_avatar(b, cfg.avatar_diameter()) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::debug!(error = %e, "cannot decode avatar, using placeholder");
            None
        }
    });
    encode(draw(avatar.as_ref(), style, cfg))
}

/// Renders the overflow badge: a placeholder-colored circle with `+K`.
pub fn render_overflow(count: usize, cfg: &RenderConfig) -> Vec<u8> {
    encode(draw_overflow(count, cfg))
}

fn encode(pixmap: Option<Pixmap>) -> Vec<u8> {
    pixmap
        .and_then(|p| p.encode_png().ok())
        .unwrap_or_else(|| TRANSPARENT_PNG.to_vec())
}

/// Decodes a PNG and scales it to a `diameter` square.
fn decode_avatar(bytes: &[u8], diameter: u32) -> Result<RgbaImage, image::ImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIM);
    limits.max_image_height = Some(MAX_SOURCE_DIM);
    reader.limits(limits);
    let img = reader.decode()?.to_rgba8();
    Ok(imageops::resize(
        &img,
        diameter,
        diameter,
        imageops::FilterType::Triangle,
    ))
}

/// Converts straight-alpha RGBA to a premultiplied pixmap, optionally
/// desaturated.
fn to_pixmap(img: &RgbaImage, desaturate: bool) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(img.width(), img.height())?;
    for (dst, src) in pixmap.pixels_mut().iter_mut().zip(img.pixels()) {
        let [mut r, mut g, mut b, a] = src.0;
        if desaturate {
            let luma = ((77 * r as u32 + 150 * g as u32 + 29 * b as u32) >> 8) as u8;
            (r, g, b) = (luma, luma, luma);
        }
        *dst = ColorU8::from_rgba(r, g, b, a).premultiply();
    }
    Some(pixmap)
}

fn draw(avatar: Option<&RgbaImage>, style: Style, cfg: &RenderConfig) -> Option<Pixmap> {
    let mut canvas = Pixmap::new(cfg.size, cfg.size)?;
    let ring = cfg.ring() as f32;
    let center = cfg.size as f32 / 2.0;
    let radius = cfg.avatar_diameter() as f32 / 2.0;
    let circle = PathBuilder::from_circle(center, center, radius)?;
    let opacity = if style.dimmed { cfg.dim } else { 1.0 };

    let source = avatar.and_then(|img| to_pixmap(img, style.dimmed));
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    match &source {
        Some(pixmap) => {
            paint.shader = Pattern::new(
                pixmap.as_ref(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                opacity,
                Transform::from_translate(ring, ring),
            );
        }
        None => {
            let (r, g, b) = PLACEHOLDER_COLOR;
            let mut color = Color::from_rgba8(r, g, b, 255);
            color.apply_opacity(opacity);
            paint.shader = Shader::SolidColor(color);
        }
    }
    canvas.fill_path(
        &circle,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    if style.speaking {
        let path = PathBuilder::from_circle(center, center, center - ring / 2.0)?;
        let (r, g, b) = cfg.speaking_color;
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        paint.set_color_rgba8(r, g, b, 255);
        let stroke = Stroke {
            width: ring,
            ..Stroke::default()
        };
        canvas.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    Some(canvas)
}

/// 3x5 glyphs; each row's bits are left to right (0b100 = left column).
fn glyph(c: char) -> Option<[u8; 5]> {
    Some(match c {
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => return None,
    })
}

fn draw_overflow(count: usize, cfg: &RenderConfig) -> Option<Pixmap> {
    let mut canvas = Pixmap::new(cfg.size, cfg.size)?;
    let center = cfg.size as f32 / 2.0;
    let diameter = cfg.avatar_diameter();
    let circle = PathBuilder::from_circle(center, center, diameter as f32 / 2.0)?;
    let (r, g, b) = PLACEHOLDER_COLOR;
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color_rgba8(r, g, b, 255);
    canvas.fill_path(
        &circle,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let text = format!("+{}", count.min(MAX_BADGE_COUNT));
    let glyphs: Vec<[u8; 5]> = text.chars().filter_map(glyph).collect();
    // Cells: 3 per glyph plus 1 between glyphs; 5 high. The text fits in
    // the square inscribed in the circle.
    let width_cells = (glyphs.len() * 4 - 1) as u32;
    let inner = (diameter as f32 * 0.7) as u32;
    let cell = (inner / width_cells).min(inner * 3 / 5 / 5).max(1);
    let x0 = (cfg.size.saturating_sub(width_cells * cell) / 2) as f32;
    let y0 = (cfg.size.saturating_sub(5 * cell) / 2) as f32;

    let (r, g, b) = BADGE_TEXT_COLOR;
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, 255);
    let cell_f = cell as f32;
    for (i, rows) in glyphs.iter().enumerate() {
        let gx = x0 + (i * 4) as f32 * cell_f;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    let rect = Rect::from_xywh(
                        gx + col as f32 * cell_f,
                        y0 + row as f32 * cell_f,
                        cell_f,
                        cell_f,
                    )?;
                    canvas.fill_rect(rect, &paint, Transform::identity(), None);
                }
            }
        }
    }
    Some(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png: &[u8]) -> RgbaImage {
        image::load_from_memory_with_format(png, ImageFormat::Png)
            .expect("valid png")
            .to_rgba8()
    }

    /// A solid red square PNG of the given size.
    fn red_png(size: u32) -> Vec<u8> {
        let mut p = Pixmap::new(size, size).unwrap();
        p.fill(Color::from_rgba8(255, 0, 0, 255));
        p.encode_png().unwrap()
    }

    const SIZE: u32 = DEFAULT_SIZE;
    const RING_COLOR: (u8, u8, u8) = DEFAULT_RING_COLOR;

    fn render(avatar: Option<&[u8]>, style: Style) -> Vec<u8> {
        render_slot(avatar, style, &RenderConfig::default())
    }

    #[test]
    fn transparent_fallback_is_valid_1x1() {
        let img = decode(TRANSPARENT_PNG);
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn output_has_fixed_size() {
        for size in [16, 64, 128, 300] {
            let img = decode(&render(Some(&red_png(size)), Style::default()));
            assert_eq!(img.dimensions(), (SIZE, SIZE));
        }
    }

    #[test]
    fn transparent_outside_circle() {
        let img = decode(&render(Some(&red_png(128)), Style::default()));
        for (x, y) in [(0, 0), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(img.get_pixel(x, y).0[3], 0, "corner {x},{y}");
        }
        // Ring band is empty when not speaking.
        assert_eq!(img.get_pixel(SIZE / 2, 1).0[3], 0);
        // Center is the opaque avatar.
        assert_eq!(img.get_pixel(SIZE / 2, SIZE / 2).0, [255, 0, 0, 255]);
    }

    #[test]
    fn speaking_draws_green_ring() {
        let img = decode(&render(
            Some(&red_png(128)),
            Style {
                speaking: true,
                dimmed: false,
            },
        ));
        let mid = SIZE / 2;
        for (x, y) in [(mid, 1), (mid, SIZE - 2), (1, mid), (SIZE - 2, mid)] {
            let [r, g, b, a] = img.get_pixel(x, y).0;
            assert_eq!(a, 255, "ring alpha at {x},{y}");
            assert_eq!((r, g, b), RING_COLOR, "ring color at {x},{y}");
        }
        assert_eq!(img.get_pixel(0, 0).0[3], 0);
        assert_eq!(img.get_pixel(mid, mid).0, [255, 0, 0, 255]);
    }

    #[test]
    fn dimmed_is_translucent_and_grey() {
        let img = decode(&render(
            Some(&red_png(64)),
            Style {
                speaking: false,
                dimmed: true,
            },
        ));
        let [r, g, b, a] = img.get_pixel(SIZE / 2, SIZE / 2).0;
        assert!(a > 0 && a < 255, "alpha {a}");
        assert!(r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1, "{r},{g},{b}");
    }

    #[test]
    fn missing_or_invalid_avatar_gives_placeholder() {
        for input in [None, Some(&b"not a png"[..])] {
            let img = decode(&render(input, Style::default()));
            assert_eq!(img.dimensions(), (SIZE, SIZE));
            let (r, g, b) = PLACEHOLDER_COLOR;
            assert_eq!(img.get_pixel(SIZE / 2, SIZE / 2).0, [r, g, b, 255]);
            assert_eq!(img.get_pixel(0, 0).0[3], 0);
        }
    }

    #[test]
    fn honours_size_color_and_dim() {
        let cfg = RenderConfig {
            size: 32,
            speaking_color: (0, 0, 255),
            dim: 0.8,
        };
        let style = Style {
            speaking: true,
            dimmed: true,
        };
        let img = decode(&render_slot(Some(&red_png(64)), style, &cfg));
        assert_eq!(img.dimensions(), (32, 32));
        let [r, g, b, a] = img.get_pixel(16, 0).0;
        assert_eq!((r, g, b, a), (0, 0, 255, 255));
        let default_dim = decode(&render(Some(&red_png(64)), style));
        assert!(img.get_pixel(16, 16).0[3] > default_dim.get_pixel(32, 32).0[3]);
    }

    fn white_pixels(img: &RgbaImage) -> usize {
        img.pixels().filter(|p| p.0 == [255, 255, 255, 255]).count()
    }

    #[test]
    fn overflow_badge_shows_count() {
        let cfg = RenderConfig::default();
        let two = decode(&render_overflow(2, &cfg));
        assert_eq!(two.dimensions(), (SIZE, SIZE));
        assert_eq!(two.get_pixel(0, 0).0[3], 0);
        // Badge background inside the circle, away from the text.
        let (r, g, b) = PLACEHOLDER_COLOR;
        assert_eq!(two.get_pixel(SIZE / 2, 8).0, [r, g, b, 255]);
        assert!(white_pixels(&two) > 0);
        // Every white pixel is inside the badge circle.
        let c = SIZE as f32 / 2.0;
        let radius = cfg.avatar_diameter() as f32 / 2.0;
        for (x, y, p) in two.enumerate_pixels() {
            if p.0 == [255, 255, 255, 255] {
                let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
                assert!(d < radius, "text pixel {x},{y} outside the badge");
            }
        }
        // Different counts look different; two digits fit too.
        let eight = decode(&render_overflow(8, &cfg));
        assert_ne!(two, eight);
        let many = decode(&render_overflow(42, &cfg));
        assert!(white_pixels(&many) > 0);
        assert_ne!(many, decode(&render_overflow(4, &cfg)));
        assert_eq!(render_overflow(150, &cfg), render_overflow(99, &cfg));
        // Small sizes still render.
        let small = RenderConfig { size: 16, ..cfg };
        assert_eq!(decode(&render_overflow(7, &small)).dimensions(), (16, 16));
    }

    #[test]
    fn glyphs_cover_badge_text() {
        for c in "+0123456789".chars() {
            assert!(glyph(c).is_some(), "{c}");
        }
        assert!(glyph('x').is_none());
    }
}
