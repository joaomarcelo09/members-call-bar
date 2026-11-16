//! Slot image rendering: circular avatar, green ring while speaking, dimmed
//! (desaturated and translucent) while muted or deafened.
//!
//! Pure functions: bytes in, PNG bytes out. No I/O.

use std::io::Cursor;

use image::{imageops, ImageFormat, ImageReader, Limits, RgbaImage};
use tiny_skia::{
    Color, ColorU8, FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, Shader,
    SpreadMode, Stroke, Transform,
};

/// Width and height of every rendered slot image, in pixels.
pub const SIZE: u32 = 64;
/// Width of the speaking ring, in pixels.
const RING: f32 = 4.0;
/// Avatars are drawn inside the ring so the image never changes size.
const AVATAR_DIAMETER: u32 = SIZE - 2 * RING as u32;
/// Discord's "speaking" green.
const RING_COLOR: (u8, u8, u8) = (0x23, 0xa5, 0x5a);
/// Fill used when no avatar is available (or it cannot be decoded).
const PLACEHOLDER_COLOR: (u8, u8, u8) = (0x4e, 0x50, 0x58);
/// Opacity of the avatar while muted or deafened.
const DIM_OPACITY: f32 = 0.4;
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

/// Visual state of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub speaking: bool,
    /// Muted or deafened.
    pub dimmed: bool,
}

/// Renders a `SIZE`x`SIZE` PNG for an avatar (PNG bytes; `None` or
/// undecodable data gives a plain placeholder circle). Never panics; falls
/// back to [`TRANSPARENT_PNG`] if encoding fails.
pub fn render_slot(avatar_png: Option<&[u8]>, style: Style) -> Vec<u8> {
    let avatar = avatar_png.and_then(|b| match decode_avatar(b) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::debug!(error = %e, "cannot decode avatar, using placeholder");
            None
        }
    });
    draw(avatar.as_ref(), style)
        .and_then(|p| p.encode_png().ok())
        .unwrap_or_else(|| TRANSPARENT_PNG.to_vec())
}

/// Decodes a PNG and scales it to an `AVATAR_DIAMETER` square.
fn decode_avatar(bytes: &[u8]) -> Result<RgbaImage, image::ImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIM);
    limits.max_image_height = Some(MAX_SOURCE_DIM);
    reader.limits(limits);
    let img = reader.decode()?.to_rgba8();
    Ok(imageops::resize(
        &img,
        AVATAR_DIAMETER,
        AVATAR_DIAMETER,
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

fn draw(avatar: Option<&RgbaImage>, style: Style) -> Option<Pixmap> {
    let mut canvas = Pixmap::new(SIZE, SIZE)?;
    let center = SIZE as f32 / 2.0;
    let radius = AVATAR_DIAMETER as f32 / 2.0;
    let circle = PathBuilder::from_circle(center, center, radius)?;
    let opacity = if style.dimmed { DIM_OPACITY } else { 1.0 };

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
                Transform::from_translate(RING, RING),
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
        let ring = PathBuilder::from_circle(center, center, center - RING / 2.0)?;
        let (r, g, b) = RING_COLOR;
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        paint.set_color_rgba8(r, g, b, 255);
        let stroke = Stroke {
            width: RING,
            ..Stroke::default()
        };
        canvas.stroke_path(&ring, &paint, &stroke, Transform::identity(), None);
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

    #[test]
    fn transparent_fallback_is_valid_1x1() {
        let img = decode(TRANSPARENT_PNG);
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn output_has_fixed_size() {
        for size in [16, 64, 128, 300] {
            let img = decode(&render_slot(Some(&red_png(size)), Style::default()));
            assert_eq!(img.dimensions(), (SIZE, SIZE));
        }
    }

    #[test]
    fn transparent_outside_circle() {
        let img = decode(&render_slot(Some(&red_png(128)), Style::default()));
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
        let img = decode(&render_slot(
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
        let img = decode(&render_slot(
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
            let img = decode(&render_slot(input, Style::default()));
            assert_eq!(img.dimensions(), (SIZE, SIZE));
            let (r, g, b) = PLACEHOLDER_COLOR;
            assert_eq!(img.get_pixel(SIZE / 2, SIZE / 2).0, [r, g, b, 255]);
            assert_eq!(img.get_pixel(0, 0).0[3], 0);
        }
    }
}
