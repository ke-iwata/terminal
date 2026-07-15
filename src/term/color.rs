#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn to_rgb(self, default: (u8, u8, u8)) -> (u8, u8, u8) {
        match self {
            Color::Default => default,
            Color::Indexed(i) => palette_rgb(i),
            Color::Rgb(r, g, b) => (r, g, b),
        }
    }
}

pub const DEFAULT_FG: (u8, u8, u8) = (229, 229, 229);
pub const DEFAULT_BG: (u8, u8, u8) = (0, 0, 0);

const ANSI_COLORS: [(u8, u8, u8); 16] = [
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
];

/// Resolve a 256-color palette index (0-15 standard/bright ANSI, 16-231
/// 6x6x6 color cube, 232-255 grayscale ramp) to RGB.
pub fn palette_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => ANSI_COLORS[idx as usize],
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
