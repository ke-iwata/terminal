use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use std::collections::HashMap;

/// Placement info for one rasterized glyph inside the atlas texture.
#[derive(Debug, Clone, Copy)]
pub struct AtlasGlyph {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub width: f32,
    pub height: f32,
    /// Bitmap's left edge offset from the pen position, in pixels.
    pub xmin: f32,
    /// Bitmap's bottom edge offset from the baseline, in pixels (fontdue
    /// convention: positive means above the baseline... see `ymin` docs).
    pub ymin: f32,
}

pub struct FontAtlas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Distance from the top of a cell down to the glyph baseline, in px.
    pub baseline: f32,
    pub white_uv: [f32; 2],
    glyphs: HashMap<char, AtlasGlyph>,
}

impl FontAtlas {
    /// Rasterize the printable ASCII range (32..=126) from the best
    /// available system monospace font at `px_size` physical pixels
    /// (already multiplied by the window's scale factor by the caller, so
    /// glyphs come out crisp on Retina displays).
    pub fn new(px_size: f32, family: Option<&str>) -> Self {
        let font = load_system_monospace_font(family);

        let line = font.horizontal_line_metrics(px_size).unwrap_or(fontdue::LineMetrics {
            ascent: px_size * 0.8,
            descent: -px_size * 0.2,
            line_gap: 0.0,
            new_line_size: px_size,
        });
        let cell_height = line.new_line_size.ceil().max(1.0);
        let baseline = line.ascent.ceil();
        let cell_width = font.metrics('M', px_size).advance_width.ceil().max(1.0);

        let mut rasters: Vec<(char, fontdue::Metrics, Vec<u8>)> = Vec::new();
        for code in 32u8..=126u8 {
            let c = code as char;
            let (metrics, bitmap) = font.rasterize(c, px_size);
            rasters.push((c, metrics, bitmap));
        }

        let max_w = rasters.iter().map(|(_, m, _)| m.width).max().unwrap_or(1).max(1);
        let max_h = rasters.iter().map(|(_, m, _)| m.height).max().unwrap_or(1).max(1);
        let cols = 16usize;
        // Slot 0 is a reserved fully-opaque block used for flat-color quads
        // (cell backgrounds), so every quad -- text or background -- can be
        // drawn by the same textured pipeline.
        let slot_count = rasters.len() + 1;
        let rows = slot_count.div_ceil(cols);

        let atlas_w = (cols * max_w) as u32;
        let atlas_h = (rows * max_h) as u32;
        let mut pixels = vec![0u8; (atlas_w as usize) * (atlas_h as usize)];

        for y in 0..max_h {
            for x in 0..max_w {
                pixels[y * atlas_w as usize + x] = 255;
            }
        }
        let white_uv = [
            0.5 / atlas_w as f32,
            0.5 / atlas_h as f32,
        ];

        let mut glyphs = HashMap::with_capacity(rasters.len());
        for (i, (c, metrics, bitmap)) in rasters.iter().enumerate() {
            let slot = i + 1;
            let col = slot % cols;
            let row = slot / cols;
            let ox = col * max_w;
            let oy = row * max_h;
            for y in 0..metrics.height {
                for x in 0..metrics.width {
                    pixels[(oy + y) * atlas_w as usize + (ox + x)] = bitmap[y * metrics.width + x];
                }
            }
            let uv_min = [ox as f32 / atlas_w as f32, oy as f32 / atlas_h as f32];
            let uv_max = [
                (ox + metrics.width) as f32 / atlas_w as f32,
                (oy + metrics.height) as f32 / atlas_h as f32,
            ];
            glyphs.insert(
                *c,
                AtlasGlyph {
                    uv_min,
                    uv_max,
                    width: metrics.width as f32,
                    height: metrics.height as f32,
                    xmin: metrics.xmin as f32,
                    ymin: metrics.ymin as f32,
                },
            );
        }

        FontAtlas {
            width: atlas_w,
            height: atlas_h,
            pixels,
            cell_width,
            cell_height,
            baseline,
            white_uv,
            glyphs,
        }
    }

    pub fn glyph(&self, c: char) -> Option<&AtlasGlyph> {
        self.glyphs.get(&c)
    }
}

/// Look up a system monospace font via CoreText. If `family` is given it's
/// tried first; an unrecognized name (typo, uninstalled font) just falls
/// through to the same SF Mono -> Menlo -> generic-monospace chain used
/// when no family is configured at all, so a bad config value never
/// prevents startup.
fn load_system_monospace_font(family: Option<&str>) -> fontdue::Font {
    let mut names = Vec::new();
    if let Some(family) = family {
        names.push(FamilyName::Title(family.to_string()));
    }
    names.push(FamilyName::Title("SF Mono".to_string()));
    names.push(FamilyName::Title("Menlo".to_string()));
    names.push(FamilyName::Monospace);

    let handle = SystemSource::new()
        .select_best_match(&names, &Properties::new())
        .expect("no monospace font available on this system");

    let font_kit_font = handle.load().expect("failed to load system font");
    let data = font_kit_font
        .copy_font_data()
        .expect("system font has no accessible byte data");

    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
        .unwrap_or_else(|e| panic!("fontdue failed to parse system font: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_builds_from_real_system_font() {
        // Exercises the actual CoreText -> font-kit -> fontdue pipeline
        // (no mocking), since that boundary can't be checked at compile
        // time and this sandbox can't visually confirm rendering.
        let atlas = FontAtlas::new(28.0, None);

        assert!(atlas.cell_width > 0.0);
        assert!(atlas.cell_height > 0.0);
        assert!(atlas.width > 0 && atlas.height > 0);
        assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height) as usize);

        for c in ['A', 'a', '0', '@', ' '] {
            let glyph = atlas.glyph(c).unwrap_or_else(|| panic!("missing glyph for {c:?}"));
            assert!(glyph.uv_min[0] <= glyph.uv_max[0]);
            assert!(glyph.uv_min[1] <= glyph.uv_max[1]);
        }

        // The reserved solid-color texel used for cell backgrounds must be
        // fully opaque, or every background quad would render translucent.
        let wx = (atlas.white_uv[0] * atlas.width as f32) as usize;
        let wy = (atlas.white_uv[1] * atlas.height as f32) as usize;
        assert_eq!(atlas.pixels[wy * atlas.width as usize + wx], 255);
    }

    #[test]
    fn unknown_family_falls_back_instead_of_panicking() {
        let atlas = FontAtlas::new(20.0, Some("Definitely Not An Installed Font Name"));
        assert!(atlas.cell_width > 0.0);
        assert!(atlas.glyph('A').is_some());
    }
}
