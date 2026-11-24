use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Storage format for a module's configuration file.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    Kv,
    #[default]
    Toml,
}

/// Supported schema field types (`NOREN.md` §6.7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Boolean,
    Number,
    Choice,
    Color,
    Text,
    KeyChord,
    Action,
}

/// Declarative module manifest (`noren-module.toml`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub config_format: ConfigFormat,
    pub config_file: String,
    #[serde(default)]
    pub groups: Vec<FieldGroup>,
    #[serde(default)]
    pub advanced_editors: Vec<AdvancedEditor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldGroup {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldDef {
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_running: bool,
    #[serde(default)]
    pub default_bool: Option<bool>,
    #[serde(default)]
    pub default_number: Option<f64>,
    #[serde(default)]
    pub default_text: Option<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdvancedEditor {
    pub id: String,
    pub label: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl FieldDef {
    pub fn default_value(&self) -> Option<FieldValue> {
        match self.kind {
            FieldKind::Boolean => Some(FieldValue::Bool(self.default_bool.unwrap_or(false))),
            FieldKind::Number => Some(FieldValue::Number(
                self.default_number.unwrap_or(self.min.unwrap_or(0.0)),
            )),
            FieldKind::Choice => Some(FieldValue::Text(
                self.default_text
                    .clone()
                    .or_else(|| self.choices.first().cloned())
                    .unwrap_or_default(),
            )),
            FieldKind::Color => Some(FieldValue::Text(self.default_text.clone().unwrap_or_else(
                || if self.nullable { "off" } else { "#FFFFFF" }.to_string(),
            ))),
            FieldKind::Text | FieldKind::KeyChord => Some(FieldValue::Text(
                self.default_text.clone().unwrap_or_default(),
            )),
            FieldKind::Action => None,
        }
    }
}

pub fn default_values_for_manifest(manifest: &ModuleManifest) -> HashMap<String, FieldValue> {
    let mut map = HashMap::new();
    for group in &manifest.groups {
        for field in &group.fields {
            if let Some(val) = field.default_value() {
                map.insert(field.key.clone(), val);
            }
        }
    }
    map
}

pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let trimmed = s.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("none")
    {
        return None;
    }
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let val = u32::from_str_radix(hex, 16).ok()?;
    Some((
        ((val >> 16) & 0xFF) as u8,
        ((val >> 8) & 0xFF) as u8,
        (val & 0xFF) as u8,
    ))
}

pub fn format_hex_color(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

pub fn validate_key_chord(s: &str) -> bool {
    let trimmed = s.trim().to_ascii_lowercase();
    if trimmed == "off" || trimmed == "none" {
        return true;
    }
    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    if parts.len() < 2 {
        return false;
    }
    let mods = &parts[..parts.len() - 1];
    let key = parts[parts.len() - 1];
    if key.is_empty() {
        return false;
    }
    mods.iter()
        .all(|m| matches!(*m, "win" | "ctrl" | "alt" | "shift"))
}
