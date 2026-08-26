//! Deterministic SVG rasterization for Konnect's visual-feedback tools.
//!
//! Determinism is the design constraint, not a nice-to-have: rendered PNGs
//! become stored visual baselines that are compared pixel-for-pixel across
//! machines and sessions. Two rules follow. The renderer versions are pinned
//! exactly in the workspace manifest, and **no system font is ever
//! consulted** — the font database starts empty, so a schematic SVG that
//! depends on text elements (rather than KiCad's stroke-font path data)
//! fails loudly instead of rendering differently on every machine.

use anyhow::{bail, Context, Result};
use resvg::{tiny_skia, usvg};

/// A rasterized image plus the facts a caller reports about it.
#[derive(Debug)]
pub struct Rendered {
    /// Encoded PNG bytes.
    pub png: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
}

/// Rasterize SVG bytes to a PNG at the given pixel width (height follows the
/// SVG's aspect ratio).
///
/// Refuses an SVG containing `<text>` elements: with an empty fontdb they
/// would render as nothing (or worse, differently once a font sneaks in),
/// and kicad-cli schematic exports draw text as stroke-font paths, so a text
/// element here means the input is not what this renderer is for.
pub fn svg_to_png(svg: &[u8], width_px: u32) -> Result<Rendered> {
    if width_px == 0 || width_px > 8192 {
        bail!("width_px must be 1..=8192, got {width_px}");
    }

    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &options).context("SVG did not parse")?;

    // usvg resolves text during parsing against options.fontdb — empty here.
    // Any text node in the source is therefore a determinism hazard; refuse.
    if svg_contains_text_element(svg) {
        bail!(
            "SVG contains <text> elements; rendering them requires fonts and \
             is not deterministic. KiCad schematic exports use stroke-font \
             paths — re-export, or rasterize elsewhere."
        );
    }

    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        bail!("SVG has no drawable area");
    }
    let scale = width_px as f32 / size.width();
    let height_px = (size.height() * scale).round().max(1.0) as u32;

    let mut pixmap =
        tiny_skia::Pixmap::new(width_px, height_px).context("could not allocate pixmap")?;
    // White ground: schematic renders are compared as opaque images so alpha
    // differences cannot masquerade as "no change".
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let png = pixmap.encode_png().context("PNG encoding failed")?;
    Ok(Rendered {
        png,
        width_px,
        height_px,
    })
}

/// The result of comparing two renders.
#[derive(Debug)]
pub struct PixelDiff {
    pub changed_pixels: u64,
    pub total_pixels: u64,
    /// Percentage in [0, 100], rounded to 3 decimals.
    pub changed_pct: f64,
    /// Bounding box of all changed pixels (x_min, y_min, x_max, y_max),
    /// None when nothing changed.
    pub changed_bbox: Option<(u32, u32, u32, u32)>,
}

/// Per-channel-max luminance threshold under which a pixel difference is
/// noise, ported from the reference implementation (8/255).
pub const DIFF_THRESHOLD: u8 = 8;

/// Compare two PNGs pixel-for-pixel on a shared canvas sized to the larger
/// of each dimension (missing area counts as changed).
pub fn diff_pngs(before: &[u8], after: &[u8]) -> Result<PixelDiff> {
    let a = image::load_from_memory(before)
        .context("before image did not decode")?
        .to_rgba8();
    let b = image::load_from_memory(after)
        .context("after image did not decode")?
        .to_rgba8();

    let width = a.width().max(b.width());
    let height = a.height().max(b.height());
    let mut changed: u64 = 0;
    let mut bbox: Option<(u32, u32, u32, u32)> = None;

    for y in 0..height {
        for x in 0..width {
            let pa = pixel_or_white(&a, x, y);
            let pb = pixel_or_white(&b, x, y);
            let delta = pa
                .iter()
                .zip(pb.iter())
                .map(|(ca, cb)| ca.abs_diff(*cb))
                .max()
                .unwrap_or(0);
            if delta > DIFF_THRESHOLD {
                changed += 1;
                bbox = Some(match bbox {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }

    let total = u64::from(width) * u64::from(height);
    let pct = if total == 0 {
        0.0
    } else {
        (changed as f64 / total as f64 * 100.0 * 1000.0).round() / 1000.0
    };
    Ok(PixelDiff {
        changed_pixels: changed,
        total_pixels: total,
        changed_pct: pct,
        changed_bbox: bbox,
    })
}

/// Flatten onto white outside an image's bounds and under transparency, so
/// alpha deltas register as color deltas instead of disappearing.
fn pixel_or_white(img: &image::RgbaImage, x: u32, y: u32) -> [u8; 3] {
    if x >= img.width() || y >= img.height() {
        return [255, 255, 255];
    }
    let p = img.get_pixel(x, y).0;
    let alpha = u16::from(p[3]);
    let over = |c: u8| ((u16::from(c) * alpha + 255 * (255 - alpha)) / 255) as u8;
    [over(p[0]), over(p[1]), over(p[2])]
}

/// Cheap structural probe for `<text` elements (tag open, not attribute).
fn svg_contains_text_element(svg: &[u8]) -> bool {
    svg.windows(5).any(|w| w == b"<text")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECT_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect x="10" y="10" width="30" height="20" fill="#112233"/></svg>"##;

    #[test]
    fn renders_a_rect_at_requested_width() {
        let out = svg_to_png(RECT_SVG, 200).unwrap();
        assert_eq!(out.width_px, 200);
        assert_eq!(out.height_px, 100, "aspect ratio preserved");
        let img = image::load_from_memory(&out.png).unwrap().to_rgba8();
        assert_eq!(
            img.get_pixel(50, 40).0,
            [0x11, 0x22, 0x33, 255],
            "rect body"
        );
        assert_eq!(img.get_pixel(5, 5).0, [255, 255, 255, 255], "white ground");
    }

    #[test]
    fn same_input_renders_byte_identically_twice() {
        // Smoke test only — the real determinism gate is the cross-OS CI
        // hash comparison added with the visual tools.
        let a = svg_to_png(RECT_SVG, 200).unwrap();
        let b = svg_to_png(RECT_SVG, 200).unwrap();
        assert_eq!(a.png, b.png);
    }

    #[test]
    fn text_elements_are_refused_not_silently_dropped() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text x="0" y="8">hi</text></svg>"##;
        let err = svg_to_png(svg, 100).unwrap_err().to_string();
        assert!(err.contains("<text>"), "{err}");
    }

    #[test]
    fn diff_reports_the_changed_region() {
        let before = svg_to_png(RECT_SVG, 100).unwrap();
        let moved: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect x="60" y="10" width="30" height="20" fill="#112233"/></svg>"##;
        let after = svg_to_png(moved, 100).unwrap();
        let diff = diff_pngs(&before.png, &after.png).unwrap();
        assert!(diff.changed_pixels > 0);
        let (x0, _, x1, _) = diff.changed_bbox.unwrap();
        assert!(
            x0 >= 10 && x1 <= 90,
            "change confined to the two rect sites"
        );
    }

    #[test]
    fn identical_images_diff_to_zero_with_no_bbox() {
        let a = svg_to_png(RECT_SVG, 100).unwrap();
        let diff = diff_pngs(&a.png, &a.png).unwrap();
        assert_eq!(diff.changed_pixels, 0);
        assert_eq!(diff.changed_pct, 0.0);
        assert!(diff.changed_bbox.is_none());
    }
}
