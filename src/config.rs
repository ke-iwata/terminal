use crate::term::color::Palette;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub colors: ColorConfig,
    pub shell: ShellConfig,
    pub scrollback_lines: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font: FontConfig::default(),
            colors: ColorConfig::default(),
            shell: ShellConfig::default(),
            scrollback_lines: 10_000,
        }
    }
}

impl Config {
    /// Load from `~/.terminal.config.toml`. Falls back to defaults if the
    /// file is missing; falls back with a warning if it exists but fails
    /// to parse, so a typo never prevents the terminal from starting.
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Config::default(),
        };
        match toml::from_str(&contents) {
            Ok(config) => config,
            Err(e) => {
                eprintln!(
                    "terminal: failed to parse {} ({e}); using defaults",
                    path.display()
                );
                Config::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Err(std::io::Error::other("could not determine home directory"));
        };
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("failed to serialize config: {e}")))?;
        std::fs::write(path, text)
    }
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".terminal.config.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// System font family name (e.g. "SF Mono"). When absent, the terminal
    /// falls back through SF Mono -> Menlo -> the system default monospace
    /// font. An unrecognized name here falls back the same way, so a typo
    /// never breaks startup.
    pub family: Option<String>,
    pub size: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig { family: None, size: 14.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    /// Overrides `$SHELL` when set.
    pub command: Option<String>,
    /// Extra arguments appended after the login-shell argv0.
    pub args: Vec<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig { command: None, args: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorConfig {
    pub background: HexColor,
    pub foreground: HexColor,
    pub ansi: [HexColor; 16],
}

impl Default for ColorConfig {
    fn default() -> Self {
        ColorConfig::from(&Palette::default())
    }
}

impl From<&Palette> for ColorConfig {
    fn from(p: &Palette) -> Self {
        ColorConfig {
            background: HexColor::from(p.background),
            foreground: HexColor::from(p.foreground),
            ansi: p.ansi.map(HexColor::from),
        }
    }
}

impl From<&ColorConfig> for Palette {
    fn from(c: &ColorConfig) -> Self {
        Palette {
            background: c.background.into(),
            foreground: c.foreground.into(),
            ansi: c.ansi.map(Into::into),
        }
    }
}

/// An RGB color, serialized as a `"#rrggbb"` string in the TOML file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(pub u8, pub u8, pub u8);

impl From<(u8, u8, u8)> for HexColor {
    fn from((r, g, b): (u8, u8, u8)) -> Self {
        HexColor(r, g, b)
    }
}

impl From<HexColor> for (u8, u8, u8) {
    fn from(c: HexColor) -> Self {
        (c.0, c.1, c.2)
    }
}

impl std::fmt::Display for HexColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

impl HexColor {
    pub fn parse(s: &str) -> Option<HexColor> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(HexColor(r, g, b))
    }
}

impl Serialize for HexColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        HexColor::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid hex color: {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_roundtrip() {
        let c = HexColor(0x1a, 0x2b, 0x3c);
        assert_eq!(c.to_string(), "#1a2b3c");
        assert_eq!(HexColor::parse("#1a2b3c"), Some(c));
        assert_eq!(HexColor::parse("1a2b3c"), Some(c));
        assert_eq!(HexColor::parse("#bad"), None);
        assert_eq!(HexColor::parse("#gggggg"), None);
    }

    #[test]
    fn full_toml_parses() {
        let toml_text = r##"
            scrollback_lines = 5000

            [font]
            family = "Fira Code"
            size = 16.0

            [colors]
            background = "#101010"
            foreground = "#f0f0f0"
            ansi = [
                "#000000", "#ff0000", "#00ff00", "#ffff00",
                "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
                "#111111", "#ff1111", "#11ff11", "#ffff11",
                "#1111ff", "#ff11ff", "#11ffff", "#eeeeee",
            ]

            [shell]
            command = "/bin/bash"
            args = ["-l"]
        "##;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.scrollback_lines, 5000);
        assert_eq!(config.font.family.as_deref(), Some("Fira Code"));
        assert_eq!(config.font.size, 16.0);
        assert_eq!(config.colors.background, HexColor(0x10, 0x10, 0x10));
        assert_eq!(config.shell.command.as_deref(), Some("/bin/bash"));
        assert_eq!(config.shell.args, vec!["-l".to_string()]);
    }

    #[test]
    fn partial_toml_fills_in_defaults() {
        let toml_text = r##"
            [font]
            size = 20.0
        "##;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.font.size, 20.0);
        assert_eq!(config.font.family, None);
        assert_eq!(config.scrollback_lines, 10_000);
        assert_eq!(config.colors.background, HexColor(0, 0, 0));
    }

    #[test]
    fn empty_toml_matches_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.scrollback_lines, Config::default().scrollback_lines);
        assert_eq!(config.font.size, Config::default().font.size);
    }

    #[test]
    fn invalid_hex_color_is_a_parse_error() {
        let toml_text = r##"
            [colors]
            background = "not-a-color"
        "##;
        assert!(toml::from_str::<Config>(toml_text).is_err());
    }

    #[test]
    fn config_round_trips_through_serialization() {
        let mut config = Config::default();
        config.font.family = Some("Iosevka".to_string());
        config.scrollback_lines = 2500;
        let text = toml::to_string_pretty(&config).unwrap();
        let reparsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(reparsed.font.family, config.font.family);
        assert_eq!(reparsed.scrollback_lines, config.scrollback_lines);
        assert_eq!(reparsed.colors.background, config.colors.background);
    }

    #[test]
    fn color_config_default_matches_palette_default() {
        let palette = Palette::default();
        let colors = ColorConfig::default();
        assert_eq!((colors.background.0, colors.background.1, colors.background.2), palette.background);
        assert_eq!(Palette::from(&colors), palette);
    }

    #[test]
    fn shipped_example_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml");
        let text = std::fs::read_to_string(path).expect("config.example.toml should exist");
        let config: Config = toml::from_str(&text).expect("config.example.toml should be valid TOML matching Config");
        assert_eq!(config.scrollback_lines, 10_000);
        assert_eq!(config.colors.ansi.len(), 16);
    }
}
