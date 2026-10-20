//! User settings: languages, translation font size, colors, hotkeys. Stored as plain
//! `key = value` lines in the per-user config dir; unknown keys and bad values fall back to
//! defaults field by field so a hand-edited file never blocks startup.

use crate::paint::{Rgb, Theme};
use std::path::PathBuf;

/// One selectable language and its code in every engine we talk to.
pub struct Lang {
    /// Shown in the settings UI, in the language itself.
    pub label: &'static str,
    /// Google Translate code; also the config value.
    pub google: &'static str,
    /// Tesseract traineddata name (Linux OCR).
    pub tesseract: &'static str,
    /// BCP-47 tag (Windows OCR).
    pub bcp47: &'static str,
}

pub const LANGS: &[Lang] = &[
    Lang { label: "한국어", google: "ko", tesseract: "kor", bcp47: "ko" },
    Lang { label: "English", google: "en", tesseract: "eng", bcp47: "en" },
    Lang { label: "日本語", google: "ja", tesseract: "jpn", bcp47: "ja" },
    Lang { label: "中文 (简体)", google: "zh-CN", tesseract: "chi_sim", bcp47: "zh-Hans" },
    Lang { label: "中文 (繁體)", google: "zh-TW", tesseract: "chi_tra", bcp47: "zh-Hant" },
    Lang { label: "Deutsch", google: "de", tesseract: "deu", bcp47: "de" },
    Lang { label: "Français", google: "fr", tesseract: "fra", bcp47: "fr" },
    Lang { label: "Español", google: "es", tesseract: "spa", bcp47: "es" },
    Lang { label: "Italiano", google: "it", tesseract: "ita", bcp47: "it" },
    Lang { label: "Português", google: "pt", tesseract: "por", bcp47: "pt" },
    Lang { label: "Русский", google: "ru", tesseract: "rus", bcp47: "ru" },
    Lang { label: "Tiếng Việt", google: "vi", tesseract: "vie", bcp47: "vi" },
    Lang { label: "ไทย", google: "th", tesseract: "tha", bcp47: "th" },
];

pub fn lang(google: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.google == google)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Google code of the text on screen (drives OCR language).
    pub source: String,
    /// Google code to translate into.
    pub target: String,
    /// Translation font size in px; 0 = match the original text's height.
    pub font_px: u32,
    pub accent: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    /// `global_hotkey` syntax, e.g. "Alt+Shift+T".
    pub hotkey_crop: String,
    pub hotkey_full: String,
}

pub const FONT_PX_MIN: u32 = 12;
pub const FONT_PX_MAX: u32 = 48;

impl Default for Config {
    fn default() -> Config {
        Config {
            source: "en".into(),
            target: "ko".into(),
            font_px: 0,
            accent: (0xF2, 0xB5, 0x44),
            surface: (0x16, 0x17, 0x1A),
            text: (0xEC, 0xED, 0xEF),
            hotkey_crop: "Alt+Shift+T".into(),
            hotkey_full: "Alt+Shift+F".into(),
        }
    }
}

impl Config {
    pub fn theme(&self) -> Theme {
        Theme { accent: self.accent, surface: self.surface, text: self.text }
    }

    pub fn source_lang(&self) -> &'static Lang {
        lang(&self.source).unwrap_or(&LANGS[1])
    }

    pub fn target_lang(&self) -> &'static Lang {
        lang(&self.target).unwrap_or(&LANGS[0])
    }

    /// `$XDG_CONFIG_HOME/snaptr/config` (or `~/.config/...`) on Linux,
    /// `%APPDATA%\snaptr\config` on Windows.
    pub fn path() -> PathBuf {
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        };
        base.unwrap_or_else(|| PathBuf::from(".")).join("snaptr").join("config")
    }

    /// Missing or unreadable file = defaults.
    pub fn load() -> Config {
        std::fs::read_to_string(Config::path()).map(|s| Config::parse(&s)).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Config::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        std::fs::write(&path, self.serialize()).map_err(|e| format!("writing {}: {e}", path.display()))
    }

    pub fn parse(s: &str) -> Config {
        let mut c = Config::default();
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "source" if lang(v).is_some() => c.source = v.into(),
                "target" if lang(v).is_some() => c.target = v.into(),
                "font_px" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if n == 0 || (FONT_PX_MIN..=FONT_PX_MAX).contains(&n) {
                            c.font_px = n;
                        }
                    }
                }
                "accent" => c.accent = parse_hex(v).unwrap_or(c.accent),
                "surface" => c.surface = parse_hex(v).unwrap_or(c.surface),
                "text" => c.text = parse_hex(v).unwrap_or(c.text),
                "hotkey_crop" if !v.is_empty() => c.hotkey_crop = v.into(),
                "hotkey_full" if !v.is_empty() => c.hotkey_full = v.into(),
                _ => {}
            }
        }
        c
    }

    pub fn serialize(&self) -> String {
        format!(
            "source = {}\ntarget = {}\nfont_px = {}\naccent = {}\nsurface = {}\ntext = {}\nhotkey_crop = {}\nhotkey_full = {}\n",
            self.source,
            self.target,
            self.font_px,
            to_hex(self.accent),
            to_hex(self.surface),
            to_hex(self.text),
            self.hotkey_crop,
            self.hotkey_full,
        )
    }
}

pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

pub fn to_hex((r, g, b): Rgb) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_every_field() {
        let c = Config {
            source: "ja".into(),
            target: "de".into(),
            font_px: 20,
            accent: (1, 2, 3),
            surface: (0xAB, 0xCD, 0xEF),
            text: (255, 255, 255),
            hotkey_crop: "Ctrl+Alt+Q".into(),
            hotkey_full: "Super+F9".into(),
        };
        assert_eq!(Config::parse(&c.serialize()), c);
    }

    #[test]
    fn bad_values_fall_back_per_field() {
        let c = Config::parse("source = xx\ntarget = ja\nfont_px = 300\naccent = #12345\ntext=#00ff00\ngarbage\n");
        let d = Config::default();
        assert_eq!(c.source, d.source);
        assert_eq!(c.target, "ja");
        assert_eq!(c.font_px, d.font_px);
        assert_eq!(c.accent, d.accent);
        assert_eq!(c.text, (0, 255, 0));
    }
}
