use std::ffi::c_void;
use std::fs;

use serde::{Deserialize, Serialize};
use windows::core::{PCWSTR, w};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

use crate::error::SdkError;
use crate::paths::general_toml_path;
use crate::schema::{format_hex_color, parse_hex_color};
use crate::storage::atomic_write_text;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMaterial {
    #[default]
    Mica,
    Acrylic,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccentMode {
    #[default]
    Windows,
    Custom,
}

/// Shared Noren theme configuration stored in `%APPDATA%\Noren\general.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default)]
    pub mode: ThemeMode,
    #[serde(default)]
    pub material: ThemeMaterial,
    #[serde(default)]
    pub accent_mode: AccentMode,
    #[serde(default)]
    pub custom_accent: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct GeneralFile {
    #[serde(default)]
    theme: ThemeConfig,
}

/// Resolved concrete theme values after applying Windows defaults when configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedTheme {
    pub is_dark: bool,
    pub material: ThemeMaterial,
    pub transparency_enabled: bool,
    pub accent_rgb: (u8, u8, u8),
}

unsafe fn read_reg_dword(subkey: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = RegGetValueW(
        HKEY_CURRENT_USER,
        subkey,
        value,
        RRF_RT_REG_DWORD,
        None,
        Some((&mut data as *mut u32).cast::<c_void>()),
        Some(&mut size),
    );
    status.is_ok().then_some(data)
}

/// Reads Windows app dark-mode setting (`AppsUseLightTheme == 0`).
pub fn windows_is_dark() -> bool {
    unsafe {
        read_reg_dword(
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
        )
        .map_or(true, |v| v == 0)
    }
}

/// Reads Windows transparency toggle (`EnableTransparency != 0`).
pub fn windows_transparency_enabled() -> bool {
    unsafe {
        read_reg_dword(
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("EnableTransparency"),
        )
        .map_or(true, |v| v != 0)
    }
}

/// Reads Windows DWM accent color as `(r, g, b)`.
pub fn windows_accent_rgb() -> (u8, u8, u8) {
    let abgr = unsafe {
        read_reg_dword(w!(r"Software\Microsoft\Windows\DWM"), w!("AccentColor"))
    };
    match abgr {
        Some(v) => (
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
        ),
        None => (79, 168, 245),
    }
}

impl ThemeConfig {
    /// Resolves this config against current Windows system preferences.
    pub fn resolve(&self) -> ResolvedTheme {
        let is_dark = match self.mode {
            ThemeMode::System => windows_is_dark(),
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
        };
        let accent_rgb = match self.accent_mode {
            AccentMode::Windows => windows_accent_rgb(),
            AccentMode::Custom => self
                .custom_accent
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(windows_accent_rgb),
        };
        ResolvedTheme {
            is_dark,
            material: self.material,
            transparency_enabled: windows_transparency_enabled(),
            accent_rgb,
        }
    }

    pub fn normalized_custom_accent(&self) -> String {
        self.custom_accent
            .as_deref()
            .and_then(parse_hex_color)
            .map_or_else(
                || {
                    let (r, g, b) = windows_accent_rgb();
                    format_hex_color(r, g, b)
                },
                |(r, g, b)| format_hex_color(r, g, b),
            )
    }
}

/// Loads `general.toml`, returning `ThemeConfig::default()` if the file does not exist or fails to parse.
pub fn load_theme() -> ThemeConfig {
    let path = general_toml_path();
    let Ok(text) = fs::read_to_string(path) else {
        return ThemeConfig::default();
    };
    toml::from_str::<GeneralFile>(&text)
        .map(|g| g.theme)
        .unwrap_or_default()
}

/// Persists `ThemeConfig` atomically to `%APPDATA%\Noren\general.toml`.
pub fn save_theme(theme: &ThemeConfig) -> Result<(), SdkError> {
    let file = GeneralFile {
        theme: theme.clone(),
    };
    let text = toml::to_string_pretty(&file)
        .map_err(|e| SdkError::Serialization(format!("TOML serialize error: {e}")))?;
    atomic_write_text(&general_toml_path(), &text)
}
