use font_kit::family_name::FamilyName;
use font_kit::font::Font as FontKitFont;
use font_kit::properties::{Properties, Style};
use font_kit::source::SystemSource;
use std::cell::RefCell;
use std::collections::HashMap;

/// Side length of the glyph texture, in physical pixels. Fixed rather
/// than grown: the texture is bound once for the pipeline's lifetime, and
/// reallocating it would mean rebuilding the bind group mid-frame. At a
/// typical 28px cell this holds a few thousand distinct glyphs -- far
/// more than a session realistically shows, and only 4MB (R8) even so.
const ATLAS_SIDE: u32 = 2048;
/// Blank margin around each glyph in the atlas, so linear sampling at a
/// glyph's edge can't bleed in a neighbour's coverage.
const GLYPH_PADDING: u32 = 1;

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

/// A rasterized glyph waiting to be copied into the GPU texture.
pub struct PendingUpload {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// Where the next glyph goes. A shelf packer: glyphs are laid left to
/// right along a row whose height is set by the tallest glyph on it, and
/// a new row starts below when one no longer fits. Crude compared to a
/// real bin packer, but glyphs from one font at one size are all within a
/// few pixels of each other, so the wasted space is negligible.
struct Shelf {
    x: u32,
    y: u32,
    height: u32,
}

struct Cache {
    /// `None` means "no font on this system draws this character" --
    /// cached too, so a missing glyph isn't re-attempted on every frame
    /// it appears in.
    glyphs: HashMap<char, Option<AtlasGlyph>>,
    shelf: Shelf,
    pending: Vec<PendingUpload>,
    /// Set once the texture runs out of room; further misses give up
    /// immediately rather than re-attempting the packing every frame.
    full: bool,
}

pub struct FontAtlas {
    pub width: u32,
    pub height: u32,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Distance from the top of a cell down to the glyph baseline, in px.
    pub baseline: f32,
    pub white_uv: [f32; 2],
    px_size: f32,
    /// The configured monospace font first, then whatever the system has
    /// for the scripts it doesn't cover. Looked up in order per character.
    fonts: Vec<fontdue::Font>,
    /// Rasterizing is memoization, not mutation of what the atlas *is*:
    /// `glyph()` reads as a pure lookup to every caller, and the
    /// alternative -- threading `&mut` through every chrome-drawing
    /// function -- would spread the cache's implementation detail across
    /// the whole renderer. Borrows here are short and never reentrant.
    cache: RefCell<Cache>,
}

impl FontAtlas {
    /// Prepare an atlas for `px_size` physical pixels (the caller has
    /// already multiplied by the window's scale factor, so glyphs come
    /// out crisp on Retina displays).
    ///
    /// Only the reserved white texel and the ASCII range are rasterized
    /// up front; everything else is rasterized the first time it's asked
    /// for. That's what lets the terminal draw text in any script
    /// without the atlas having to hold every character of every one.
    pub fn new(px_size: f32, family: Option<&str>) -> Self {
        let fonts = load_font_chain(family);
        let primary = &fonts[0];

        let line = primary.horizontal_line_metrics(px_size).unwrap_or(fontdue::LineMetrics {
            ascent: px_size * 0.8,
            descent: -px_size * 0.2,
            line_gap: 0.0,
            new_line_size: px_size,
        });
        let cell_height = line.new_line_size.ceil().max(1.0);
        let baseline = line.ascent.ceil();
        let cell_width = primary.metrics('M', px_size).advance_width.ceil().max(1.0);

        // A 2x2 opaque block in the corner, sampled by every flat-color
        // quad (cell backgrounds, bars, cursors) so text and fills can
        // share one pipeline. 2x2 rather than 1x1 so linear sampling at
        // its center can't pick up the transparent pixels beside it.
        let white = PendingUpload { x: 0, y: 0, width: 2, height: 2, coverage: vec![255; 4] };
        let atlas = FontAtlas {
            width: ATLAS_SIDE,
            height: ATLAS_SIDE,
            cell_width,
            cell_height,
            baseline,
            white_uv: [1.0 / ATLAS_SIDE as f32, 1.0 / ATLAS_SIDE as f32],
            px_size,
            fonts,
            cache: RefCell::new(Cache {
                glyphs: HashMap::new(),
                shelf: Shelf { x: 2 + GLYPH_PADDING, y: 0, height: 2 },
                pending: vec![white],
                full: false,
            }),
        };

        // Warm the cache with ASCII: it's what most text is, and doing it
        // here keeps the first frame from uploading a hundred glyphs one
        // at a time.
        for code in 32u8..=126u8 {
            atlas.glyph(code as char);
        }
        atlas
    }

    /// The glyph for `c`, rasterizing and packing it on first use.
    /// `None` when no font in the chain draws this character, or when the
    /// atlas has run out of room.
    pub fn glyph(&self, c: char) -> Option<AtlasGlyph> {
        if let Some(cached) = self.cache.borrow().glyphs.get(&c) {
            return *cached;
        }
        let placed = self.rasterize_and_pack(c);
        self.cache.borrow_mut().glyphs.insert(c, placed);
        placed
    }

    fn rasterize_and_pack(&self, c: char) -> Option<AtlasGlyph> {
        // `lookup_glyph_index` returns 0 for "not in this font", which is
        // the .notdef slot -- rasterizing that would draw a tofu box, so
        // the chain is walked until a font actually has the character.
        let font = self.fonts.iter().find(|f| f.lookup_glyph_index(c) != 0)?;
        let (metrics, coverage) = font.rasterize(c, self.px_size);

        let mut cache = self.cache.borrow_mut();
        if cache.full {
            return None;
        }
        let (width, height) = (metrics.width as u32, metrics.height as u32);
        // A blank glyph (space) still needs an entry so the cache doesn't
        // re-rasterize it, but it occupies no atlas space.
        if width == 0 || height == 0 {
            return Some(AtlasGlyph {
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                width: 0.0,
                height: 0.0,
                xmin: metrics.xmin as f32,
                ymin: metrics.ymin as f32,
            });
        }

        if cache.shelf.x + width > ATLAS_SIDE {
            // Next shelf down, tall enough for whatever lands on it.
            cache.shelf.x = 0;
            cache.shelf.y += cache.shelf.height + GLYPH_PADDING;
            cache.shelf.height = 0;
        }
        if cache.shelf.y + height > ATLAS_SIDE {
            cache.full = true;
            return None;
        }
        let (x, y) = (cache.shelf.x, cache.shelf.y);
        cache.shelf.x += width + GLYPH_PADDING;
        cache.shelf.height = cache.shelf.height.max(height);
        cache.pending.push(PendingUpload { x, y, width, height, coverage });

        Some(AtlasGlyph {
            uv_min: [x as f32 / ATLAS_SIDE as f32, y as f32 / ATLAS_SIDE as f32],
            uv_max: [(x + width) as f32 / ATLAS_SIDE as f32, (y + height) as f32 / ATLAS_SIDE as f32],
            width: width as f32,
            height: height as f32,
            xmin: metrics.xmin as f32,
            ymin: metrics.ymin as f32,
        })
    }

    /// Hand over every glyph rasterized since the last call, for the
    /// caller to copy into the GPU texture. Must be drained each frame
    /// *before* drawing, or newly-seen characters render as blanks for a
    /// frame.
    pub fn take_pending_uploads(&self) -> Vec<PendingUpload> {
        std::mem::take(&mut self.cache.borrow_mut().pending)
    }
}

/// The configured (or default) monospace font, followed by fonts for the
/// scripts monospace faces typically omit.
///
/// SF Mono and Menlo cover Latin, Greek, Cyrillic and the box-drawing
/// range, and nothing else -- a Japanese file name or a log line with CJK
/// in it has no glyphs at all in them. Rather than substituting `?` for
/// what can't be drawn, unknown characters fall through this chain until
/// a font that has them is found.
fn load_font_chain(family: Option<&str>) -> Vec<fontdue::Font> {
    // macOS ships all of these; anything missing is simply skipped, so
    // this list is a preference order rather than a requirement.
    const FALLBACKS: &[&str] = &[
        "Hiragino Sans",        // Japanese
        "PingFang SC",          // Simplified Chinese
        "PingFang TC",          // Traditional Chinese
        "Apple SD Gothic Neo",  // Korean
        "Menlo",                // box drawing, Greek/Cyrillic, many symbols
        "Apple Symbols",        // arrows, math, misc technical
        "Arial Unicode MS",     // broad catch-all when installed
    ];

    let source = SystemSource::new();
    let mut fonts = vec![to_fontdue(select_regular_font(family))];
    for name in FALLBACKS {
        if let Some(font) = regular_face_in_family(&source, name) {
            fonts.push(to_fontdue(font));
        }
    }
    fonts
}

fn to_fontdue(font: FontKitFont) -> fontdue::Font {
    let data = font
        .copy_font_data()
        .expect("system font has no accessible byte data");
    fontdue::Font::from_bytes(data.as_slice(), fontdue::FontSettings::default())
        .unwrap_or_else(|e| panic!("fontdue failed to parse system font: {e}"))
}

/// Resolve `family` (or the SF Mono -> Menlo fallback chain) to a specific,
/// non-italic, non-bold font-kit `Font`.
///
/// Deliberately does NOT use `SystemSource::select_best_match`: on at least
/// one real macOS install it returned "Menlo Italic" for a plain "Menlo"
/// request with `Properties::new()` (i.e. `Style::Normal`) -- font-kit's
/// CoreText backend doesn't reliably filter by style when a family has
/// multiple faces. Instead, each candidate family is resolved with
/// `select_family_by_name` and its faces are inspected directly so the
/// upright, regular-weight member is picked explicitly.
fn select_regular_font(family: Option<&str>) -> FontKitFont {
    let source = SystemSource::new();

    let candidates = family
        .map(str::to_string)
        .into_iter()
        .chain(["SF Mono".to_string(), "Menlo".to_string()]);

    for name in candidates {
        if let Some(font) = regular_face_in_family(&source, &name) {
            return font;
        }
    }

    // Last resort: whatever CoreText considers "the" generic monospace
    // family. select_best_match's style filtering is unreliable (see
    // above), but at this point there's no specific family left to
    // hand-inspect, so it's the best option remaining.
    let handle = source
        .select_best_match(&[FamilyName::Monospace], &Properties::new())
        .expect("no monospace font available on this system");
    handle.load().expect("failed to load system font")
}

fn regular_face_in_family(source: &SystemSource, family_name: &str) -> Option<FontKitFont> {
    let family = source.select_family_by_name(family_name).ok()?;
    let faces: Vec<FontKitFont> = family.fonts().iter().filter_map(|h| h.load().ok()).collect();

    faces
        .iter()
        .find(|f| f.properties().style == Style::Normal && f.properties().weight.0 <= 500.0)
        .or_else(|| faces.iter().find(|f| f.properties().style == Style::Normal))
        .or_else(|| faces.first())
        .cloned()
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

        for c in ['A', 'a', '0', '@', ' '] {
            let glyph = atlas.glyph(c).unwrap_or_else(|| panic!("missing glyph for {c:?}"));
            assert!(glyph.uv_min[0] <= glyph.uv_max[0]);
            assert!(glyph.uv_min[1] <= glyph.uv_max[1]);
        }
    }

    #[test]
    fn the_white_texel_is_uploaded_opaque() {
        // Every flat-color quad samples it; if it weren't opaque, every
        // background and bar in the app would render translucent.
        let atlas = FontAtlas::new(20.0, None);
        let uploads = atlas.take_pending_uploads();
        let white = uploads.first().expect("the white block is queued first");
        assert_eq!((white.x, white.y), (0, 0));
        assert!(white.coverage.iter().all(|&v| v == 255));
        // `white_uv` has to land inside that block.
        assert!(atlas.white_uv[0] * atlas.width as f32 <= white.width as f32);
        assert!(atlas.white_uv[1] * atlas.height as f32 <= white.height as f32);
    }

    #[test]
    fn non_ascii_characters_get_real_glyphs() {
        // The whole point of the fallback chain: monospace faces have no
        // CJK, and these used to be substituted with '?' or drawn as
        // nothing at all.
        let atlas = FontAtlas::new(24.0, None);
        for c in ['日', '本', '語', 'あ', 'ー'] {
            let glyph = atlas.glyph(c).unwrap_or_else(|| panic!("no glyph for {c:?}"));
            assert!(glyph.width > 0.0 && glyph.height > 0.0, "{c:?} rasterized empty");
        }
    }

    #[test]
    fn glyphs_are_rasterized_once_and_then_cached() {
        let atlas = FontAtlas::new(20.0, None);
        atlas.take_pending_uploads(); // drain the warm-up

        let first = atlas.glyph('漢').expect("has a glyph");
        assert_eq!(atlas.take_pending_uploads().len(), 1, "a new glyph queues one upload");

        let second = atlas.glyph('漢').expect("still has a glyph");
        assert_eq!(first.uv_min, second.uv_min, "the same slot is reused");
        assert!(atlas.take_pending_uploads().is_empty(), "a cached glyph queues nothing");
    }

    #[test]
    fn packed_glyphs_stay_inside_the_texture() {
        let atlas = FontAtlas::new(24.0, None);
        // Enough distinct characters to spill onto several shelves.
        for c in ('\u{4e00}'..).take(150) {
            atlas.glyph(c);
        }
        for upload in atlas.take_pending_uploads() {
            assert!(upload.x + upload.width <= atlas.width, "glyph runs past the right edge");
            assert!(upload.y + upload.height <= atlas.height, "glyph runs past the bottom edge");
            assert_eq!(upload.coverage.len(), (upload.width * upload.height) as usize);
        }
    }

    #[test]
    fn a_character_no_font_has_is_reported_missing_not_drawn_as_tofu() {
        let atlas = FontAtlas::new(20.0, None);
        // A private-use codepoint: nothing on the system claims it.
        assert!(atlas.glyph('\u{10FFFD}').is_none());
    }

    #[test]
    fn unknown_family_falls_back_instead_of_panicking() {
        let atlas = FontAtlas::new(20.0, Some("Definitely Not An Installed Font Name"));
        assert!(atlas.cell_width > 0.0);
        assert!(atlas.glyph('A').is_some());
    }

    #[test]
    fn default_font_is_upright_not_italic() {
        // Regression test: font-kit's SystemSource::select_best_match has
        // been observed returning "Menlo Italic" for a plain "Menlo"
        // request even with Properties::new() (Style::Normal) -- this
        // exercises the real CoreText lookup to make sure the regular,
        // upright face is what actually gets picked.
        let font = select_regular_font(None);
        assert_eq!(font.properties().style, Style::Normal, "resolved font is {}", font.full_name());
    }

    #[test]
    fn named_family_is_upright_not_italic() {
        let font = select_regular_font(Some("Menlo"));
        assert_eq!(font.properties().style, Style::Normal, "resolved font is {}", font.full_name());
    }
}
