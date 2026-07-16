#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn to_rgb(self, default: (u8, u8, u8), palette: &Palette) -> (u8, u8, u8) {
        match self {
            Color::Default => default,
            Color::Indexed(i) => palette.resolve_indexed(i),
            Color::Rgb(r, g, b) => (r, g, b),
        }
    }
}

/// The user-configurable part of a terminal's color scheme: the default
/// foreground/background and the 16 standard/bright ANSI slots. The
/// 256-color cube and grayscale ramp (indices 16-255) are always derived
/// algorithmically, matching every other terminal emulator's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub foreground: (u8, u8, u8),
    pub background: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            foreground: (229, 229, 229),
            background: (0, 0, 0),
            ansi: [
                (0, 0, 0),
                (205, 0, 0),
                (0, 205, 0),
                (205, 205, 0),
                (0, 0, 238),
                (205, 0, 205),
                (0, 205, 205),
                (229, 229, 229),
                (127, 127, 127),
                (255, 0, 0),
                (0, 255, 0),
                (255, 255, 0),
                (92, 92, 255),
                (255, 0, 255),
                (0, 255, 255),
                (255, 255, 255),
            ],
        }
    }
}

impl Palette {
    /// Resolve a 256-color palette index (0-15 from this palette's `ansi`
    /// slots, 16-231 a fixed 6x6x6 color cube, 232-255 a fixed grayscale
    /// ramp) to RGB.
    pub fn resolve_indexed(&self, idx: u8) -> (u8, u8, u8) {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let i = idx - 16;
                let r = i / 36;
                let g = (i % 36) / 6;
                let b = i % 6;
                let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                (scale(r), scale(g), scale(b))
            }
            232..=255 => {
                let v = 8 + (idx - 232) * 10;
                (v, v, v)
            }
        }
    }
}
