// ABOUTME: TOML configuration: shell, scrollback, font, colors, window size.
// ABOUTME: Missing file = defaults; present-but-invalid = hard error.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Overrides $SHELL when set.
    pub shell: Option<String>,
    pub scrollback: usize,
    pub font: FontConfig,
    pub colors: ColorConfig,
    pub window: WindowConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: None,
            scrollback: 10_000,
            font: FontConfig::default(),
            colors: ColorConfig::default(),
            window: WindowConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    pub size: f32,
    /// Multiplied by `size` to get line height.
    pub line_height: f32,
    /// Explicit font file; falls back to system monospace when absent.
    pub path: Option<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self { size: 18.0, line_height: 1.3, path: None }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorConfig {
    pub foreground: HexColor,
    pub background: HexColor,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self { foreground: HexColor(0xFFFFFF), background: HexColor(0x000000) }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { width: 960, height: 600 }
    }
}

/// `#rgb` or `#rrggbb`, stored as 0x00RRGGBB.
#[derive(Debug, Clone, Copy)]
pub struct HexColor(pub u32);

pub fn parse_hex(s: &str) -> Option<u32> {
    let h = s.strip_prefix('#')?;
    match h.len() {
        6 => u32::from_str_radix(h, 16).ok(),
        3 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            let r = (v >> 8) & 0xF;
            let g = (v >> 4) & 0xF;
            let b = v & 0xF;
            Some((r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b)
        }
        _ => None,
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        parse_hex(&s)
            .map(HexColor)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid hex color: {s:?}")))
    }
}

impl Config {
    /// Resolve and load. Explicit path wins; then XDG/HOME defaults.
    /// A missing file yields defaults; an unparseable file is an error.
    pub fn load(explicit: Option<PathBuf>) -> anyhow::Result<Self> {
        let path = explicit.or_else(default_path);
        match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p)?;
                toml::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("config {}: {}", p.display(), e))
            }
            _ => Ok(Self::default()),
        }
    }
}

fn default_path() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("gaterminal/config.toml"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/gaterminal/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.scrollback, 10_000);
        assert_eq!(c.font.size, 18.0);
        assert_eq!(c.colors.foreground.0, 0xFFFFFF);
        assert_eq!(c.colors.background.0, 0x000000);
        assert_eq!((c.window.width, c.window.height), (960, 600));
        assert!(c.shell.is_none());
    }

    #[test]
    fn parses_partial_toml_and_keeps_defaults() {
        let c: Config = toml::from_str(
            r##"
            scrollback = 500
            [colors]
            background = "#102030"
            [window]
            width = 1280
            "##,
        )
        .unwrap();
        assert_eq!(c.scrollback, 500);
        assert_eq!(c.colors.background.0, 0x102030);
        // Untouched fields fall back to defaults.
        assert_eq!(c.colors.foreground.0, 0xFFFFFF);
        assert_eq!(c.window.width, 1280);
        assert_eq!(c.window.height, 600);
    }

    #[test]
    fn short_hex_expands() {
        assert_eq!(parse_hex("#abc"), Some(0xAABBCC));
        assert_eq!(parse_hex("#ff8800"), Some(0xFF8800));
        assert_eq!(parse_hex("bad"), None);
        assert_eq!(parse_hex("#12"), None);
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("definitely_not_a_key = 1").is_err());
    }
}
