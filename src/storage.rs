use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

use crate::error::SdkError;
use crate::paths::manifests_dir;
use crate::schema::{
    ConfigFormat, FieldDef, FieldKind, FieldValue, ModuleManifest, default_values_for_manifest,
    parse_hex_color,
};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn to_wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Atomically writes `content` to `target` using a same-directory temp file,
/// `sync_all()`, and Win32 `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`.
pub fn atomic_write_text(target: &Path, content: &str) -> Result<(), SdkError> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path: PathBuf = parent.join(format!("{file_name}.tmp.{}.{seq}", std::process::id()));

    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
    }

    let src_w = to_wide_null(&tmp_path);
    let dst_w = to_wide_null(target);
    let res = unsafe {
        MoveFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp_path);
        return Err(SdkError::Io(std::io::Error::from_raw_os_error(e.code().0)));
    }

    Ok(())
}

/// Installs/updates a module manifest into `%APPDATA%\Noren\manifests\<id>.toml`.
pub fn install_manifest(manifest: &ModuleManifest) -> Result<PathBuf, SdkError> {
    let target = manifests_dir().join(format!("{}.toml", manifest.id));
    let text = toml::to_string_pretty(manifest)
        .map_err(|e| SdkError::Serialization(format!("Manifest TOML serialize error: {e}")))?;
    atomic_write_text(&target, &text)?;
    Ok(target)
}

/// Loads all installed module manifests from `%APPDATA%\Noren\manifests\*.toml`, sorted by id.
pub fn load_installed_manifests() -> Vec<ModuleManifest> {
    let dir = manifests_dir();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(manifest) = toml::from_str::<ModuleManifest>(&text) {
                manifests.push(manifest);
            }
        }
    }
    manifests.sort_by(|a, b| a.id.cmp(&b.id));
    manifests
}

fn parse_kv_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn field_map(manifest: &ModuleManifest) -> HashMap<&str, &FieldDef> {
    let mut map = HashMap::new();
    for g in &manifest.groups {
        for f in &g.fields {
            map.insert(f.key.as_str(), f);
        }
    }
    map
}

fn format_kv_value(field: &FieldDef, val: &FieldValue) -> String {
    match (field.kind, val) {
        (FieldKind::Boolean, FieldValue::Bool(b)) => {
            if *b { "on" } else { "off" }.to_string()
        }
        (FieldKind::Number, FieldValue::Number(n)) => format!("{}", n.round() as i64),
        (FieldKind::Color, FieldValue::Text(s)) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("accent")
                || t.eq_ignore_ascii_case("windows")
                || t.eq_ignore_ascii_case("system")
            {
                "accent".to_string()
            } else if parse_hex_color(t).is_some() {
                t.to_string()
            } else {
                "off".to_string()
            }
        }
        (_, FieldValue::Text(s)) => s.trim().to_string(),
        (_, FieldValue::Bool(b)) => b.to_string(),
        (_, FieldValue::Number(n)) => format!("{n}"),
    }
}

pub fn load_config_values(
    path: &Path,
    manifest: &ModuleManifest,
) -> Result<HashMap<String, FieldValue>, SdkError> {
    match manifest.config_format {
        ConfigFormat::Kv => load_kv_file(path, manifest),
        ConfigFormat::Toml => load_toml_file(path, manifest),
    }
}

pub fn save_config_values(
    path: &Path,
    manifest: &ModuleManifest,
    values: &HashMap<String, FieldValue>,
) -> Result<(), SdkError> {
    match manifest.config_format {
        ConfigFormat::Kv => save_kv_preserving_comments(path, manifest, values),
        ConfigFormat::Toml => save_toml_atomic(path, manifest, values),
    }
}

pub fn load_kv_file(
    path: &Path,
    manifest: &ModuleManifest,
) -> Result<HashMap<String, FieldValue>, SdkError> {
    let mut values = default_values_for_manifest(manifest);
    if !path.exists() {
        return Ok(values);
    }
    let text = fs::read_to_string(path)?;
    let fields = field_map(manifest);

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let raw_val = v.trim();
        let Some(field) = fields.get(key.as_str()) else {
            continue;
        };
        let parsed = match field.kind {
            FieldKind::Boolean => parse_kv_bool(raw_val).map(FieldValue::Bool),
            FieldKind::Number => raw_val.parse::<f64>().ok().map(|n| {
                let min = field.min.unwrap_or(f64::MIN);
                let max = field.max.unwrap_or(f64::MAX);
                FieldValue::Number(n.clamp(min, max))
            }),
            FieldKind::Choice
            | FieldKind::Color
            | FieldKind::Text
            | FieldKind::KeyChord
            | FieldKind::ShortcutList => Some(FieldValue::Text(raw_val.to_string())),
            FieldKind::Action | FieldKind::Unknown => None,
        };
        if let Some(val) = parsed {
            values.insert(field.key.clone(), val);
        }
    }

    Ok(values)
}

pub fn save_kv_preserving_comments(
    path: &Path,
    manifest: &ModuleManifest,
    values: &HashMap<String, FieldValue>,
) -> Result<(), SdkError> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    let fields = field_map(manifest);
    let defaults = default_values_for_manifest(manifest);
    let mut seen_keys = HashSet::new();
    let mut out_lines = Vec::new();

    for raw_line in existing.lines() {
        let clean = raw_line.trim_end_matches('\r');
        let trimmed = clean.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out_lines.push(clean.to_string());
            continue;
        }
        if let Some((k, _)) = trimmed.split_once('=') {
            let key_lower = k.trim().to_ascii_lowercase();
            if let Some(field) = fields.get(key_lower.as_str()) {
                if let Some(val) = values.get(&field.key) {
                    let formatted = format_kv_value(field, val);
                    out_lines.push(format!("{} = {}", field.key, formatted));
                    seen_keys.insert(field.key.clone());
                    continue;
                }
            }
        }
        out_lines.push(clean.to_string());
    }

    for group in &manifest.groups {
        for field in &group.fields {
            if matches!(field.kind, FieldKind::Action | FieldKind::Unknown)
                || seen_keys.contains(&field.key)
            {
                continue;
            }
            let Some(cur) = values.get(&field.key) else {
                continue;
            };
            let is_default = defaults.get(&field.key).is_some_and(|d| d == cur);
            if !is_default {
                let formatted = format_kv_value(field, cur);
                if !formatted.is_empty() {
                    out_lines.push(format!("{} = {}", field.key, formatted));
                }
            }
        }
    }

    let mut final_text = out_lines.join("\n");
    if !final_text.ends_with('\n') {
        final_text.push('\n');
    }
    atomic_write_text(path, &final_text)
}

pub fn load_toml_file(
    path: &Path,
    manifest: &ModuleManifest,
) -> Result<HashMap<String, FieldValue>, SdkError> {
    let mut values = default_values_for_manifest(manifest);
    if !path.exists() {
        return Ok(values);
    }
    let text = fs::read_to_string(path)?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| SdkError::Serialization(format!("TOML parse error: {e}")))?;

    for group in &manifest.groups {
        for field in &group.fields {
            let Some(v) = table.get(&field.key) else {
                continue;
            };
            let parsed = match (field.kind, v) {
                (FieldKind::Boolean, toml::Value::Boolean(b)) => Some(FieldValue::Bool(*b)),
                (FieldKind::Number, toml::Value::Integer(i)) => {
                    Some(FieldValue::Number(*i as f64))
                }
                (FieldKind::Number, toml::Value::Float(f)) => Some(FieldValue::Number(*f)),
                (
                    FieldKind::Choice
                    | FieldKind::Color
                    | FieldKind::Text
                    | FieldKind::KeyChord
                    | FieldKind::ShortcutList,
                    toml::Value::String(s),
                ) => Some(FieldValue::Text(s.clone())),
                _ => None,
            };
            if let Some(val) = parsed {
                values.insert(field.key.clone(), val);
            }
        }
    }

    Ok(values)
}

pub fn save_toml_atomic(
    path: &Path,
    manifest: &ModuleManifest,
    values: &HashMap<String, FieldValue>,
) -> Result<(), SdkError> {
    let mut table: toml::Table = if path.exists() {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
            .unwrap_or_default()
    } else {
        toml::Table::new()
    };
    for group in &manifest.groups {
        for field in &group.fields {
            if matches!(field.kind, FieldKind::Action | FieldKind::Unknown) {
                continue;
            }
            if let Some(val) = values.get(&field.key) {
                let tv = match val {
                    FieldValue::Bool(b) => toml::Value::Boolean(*b),
                    FieldValue::Number(n) => {
                        if field.step.unwrap_or(1.0) >= 1.0 {
                            toml::Value::Integer(n.round() as i64)
                        } else {
                            toml::Value::Float(*n)
                        }
                    }
                    FieldValue::Text(s) => toml::Value::String(s.clone()),
                };
                table.insert(field.key.clone(), tv);
            }
        }
    }
    let serialized = toml::to_string_pretty(&table)
        .map_err(|e| SdkError::Serialization(format!("TOML serialize error: {e}")))?;
    atomic_write_text(path, &serialized)
}
