// ABOUTME: TOML configuration: shell, scrollback, font, colors, window/chrome size.
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
    pub session: SessionConfig,
    pub effects: EffectsConfig,
    pub chrome: ChromeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: None,
            scrollback: 10_000,
            font: FontConfig::default(),
            colors: ColorConfig::default(),
            window: WindowConfig::default(),
            session: SessionConfig::default(),
            effects: EffectsConfig::default(),
            chrome: ChromeConfig::default(),
        }
    }
}

/// Window chrome and visible-but-not-content settings.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeConfig {
    /// Flash the screen briefly when the app rings the bell (BEL = 0x07).
    pub visual_bell: bool,
    /// Show the Gator icon in native window chrome where the platform allows.
    pub titlebar_icon: bool,
    /// Padding (logical px) between the window edge and the cell grid.
    pub padding: u32,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            visual_bell: true,
            titlebar_icon: true,
            padding: 0,
        }
    }
}

/// Compiz-style post-process effects. All default off so the terminal feels
/// normal unless the user opts in. Enabling any of these forces the GPU
/// pipeline into its two-pass mode (off-screen render then effect shader).
#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsConfig {
    /// CRT barrel curvature + scanlines (persistent).
    pub crt: bool,
    /// Expanding ring flash centered on each keystroke (animated, ~400ms).
    pub keystroke: bool,
    /// Brief sinusoidal wobble triggered by window resize (~600ms).
    pub wobble: bool,
    /// Page-turn squeeze triggered by alt-screen toggle (~300ms).
    pub cube: bool,
}

impl EffectsConfig {
    pub fn any(&self) -> bool {
        self.crt || self.keystroke || self.wobble || self.cube
    }
}

/// History/session storage. All three are independently opt-in.
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Raw byte-for-byte session log (one file per launch). Empty = off.
    /// Supports `~` and `{ts}` (unix seconds) substitutions.
    pub raw_log: String,
    /// Plain-text scrollback log (append). Empty = off.
    pub text_log: String,
    /// On startup, preload this many lines from text_log into scrollback.
    /// 0 = no restore. Requires text_log to be set.
    pub restore_lines: usize,
}

/// `~` -> $HOME, `{ts}` -> seconds-since-epoch as integer.
pub fn expand_path(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else if s == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        s.to_string()
    };
    if out.contains("{ts}") {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out = out.replace("{ts}", &ts.to_string());
    }
    out
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
    /// Apply the "bold = bright" convention: when an SGR-bold cell uses one
    /// of the 8 basic palette colors, map it to the bright counterpart so
    /// dark navy / dark green prompts and `ls --color` directories don't
    /// disappear on a dark background.
    pub bold_is_bright: bool,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            foreground: HexColor(0xDDE6B8),
            background: HexColor(0x08110B),
            bold_is_bright: true,
        }
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
        let path = explicit.or_else(default_existing_path);
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

fn default_existing_path() -> Option<PathBuf> {
    default_existing_path_from_env(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn default_existing_path_from_env(
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    config_path_for_app_from_env("gator", xdg_config_home, home)
        .filter(|p| p.exists())
        .or_else(|| {
            config_path_for_app_from_env("gaterminal", xdg_config_home, home)
                .filter(|p| p.exists())
        })
}

fn config_path_for_app_from_env(
    app: &str,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join(app).join("config.toml"));
        }
    }
    home.map(|h| PathBuf::from(h).join(".config").join(app).join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.scrollback, 10_000);
        assert_eq!(c.font.size, 18.0);
        assert_eq!(c.colors.foreground.0, 0xDDE6B8);
        assert_eq!(c.colors.background.0, 0x08110B);
        assert_eq!((c.window.width, c.window.height), (960, 600));
        assert!(c.chrome.titlebar_icon);
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
        assert_eq!(c.colors.foreground.0, 0xDDE6B8);
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

    #[test]
    fn session_block_defaults_off() {
        let c = Config::default();
        assert!(c.session.raw_log.is_empty());
        assert!(c.session.text_log.is_empty());
        assert_eq!(c.session.restore_lines, 0);
    }

    #[test]
    fn session_block_parses() {
        let c: Config = toml::from_str(
            r#"
            [session]
            raw_log = "~/raw/{ts}.log"
            text_log = "~/history.log"
            restore_lines = 500
            "#,
        )
        .unwrap();
        assert_eq!(c.session.raw_log, "~/raw/{ts}.log");
        assert_eq!(c.session.text_log, "~/history.log");
        assert_eq!(c.session.restore_lines, 500);
    }

    #[test]
    fn app_config_paths_use_current_and_legacy_names() {
        assert_eq!(
            config_path_for_app_from_env("gator", None, Some("/tmp/home")).unwrap(),
            PathBuf::from("/tmp/home/.config/gator/config.toml")
        );
        assert_eq!(
            config_path_for_app_from_env("gaterminal", None, Some("/tmp/home")).unwrap(),
            PathBuf::from("/tmp/home/.config/gaterminal/config.toml")
        );
        assert_eq!(
            config_path_for_app_from_env("gator", Some("/tmp/xdg"), Some("/tmp/home")).unwrap(),
            PathBuf::from("/tmp/xdg/gator/config.toml")
        );
    }

    #[test]
    fn default_config_path_falls_back_to_existing_legacy_file() {
        let root = std::env::temp_dir().join(format!(
            "gator-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy_dir = root.join(".config").join("gaterminal");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_path = legacy_dir.join("config.toml");
        std::fs::write(&legacy_path, "scrollback = 123\n").unwrap();

        assert_eq!(
            default_existing_path_from_env(None, root.to_str()).unwrap(),
            legacy_path
        );
    }

    #[test]
    fn effects_default_off_and_parse() {
        let c = Config::default();
        assert!(!c.effects.any());
        let c: Config = toml::from_str(
            r#"
            [effects]
            crt = true
            keystroke = true
            "#,
        )
        .unwrap();
        assert!(c.effects.crt && c.effects.keystroke);
        assert!(!c.effects.wobble && !c.effects.cube);
        assert!(c.effects.any());
    }

    #[test]
    fn chrome_block_parses() {
        let c: Config = toml::from_str(
            r#"
            [chrome]
            visual_bell = false
            titlebar_icon = false
            padding = 8
            "#,
        )
        .unwrap();
        assert!(!c.chrome.visual_bell);
        assert!(!c.chrome.titlebar_icon);
        assert_eq!(c.chrome.padding, 8);
    }

    #[test]
    fn expand_path_substitutes_home_and_ts() {
        std::env::set_var("HOME", "/tmp/h");
        let p = expand_path("~/sub/{ts}.log");
        assert!(p.starts_with("/tmp/h/sub/"));
        assert!(p.ends_with(".log"));
        // {ts} replaced by digits.
        let body = &p["/tmp/h/sub/".len()..p.len() - ".log".len()];
        assert!(body.chars().all(|c| c.is_ascii_digit()));
    }
}
