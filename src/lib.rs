pub mod branding;
pub mod error;
pub mod node;
pub mod paths;
pub mod pipe;
pub mod protocol;
pub mod schema;
pub mod security;
pub mod storage;
pub mod theme;

#[cfg(feature = "control-center")]
pub mod control_center;

pub use error::SdkError;
pub use node::{NipEvent, NipNode, NodeConfig, PeerInfo, ReplyHandle};
pub use protocol::PROTOCOL_VERSION;
pub use schema::{
    AdvancedEditor, ConfigFormat, FieldDef, FieldGroup, FieldKind, FieldValue, ModuleManifest,
};
pub use security::IntegrityInfo;
pub use theme::{AccentMode, ResolvedTheme, ThemeConfig, ThemeMaterial, ThemeMode};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::protocol::Message;
    use crate::schema::{format_hex_color, parse_hex_color, validate_key_chord};
    use crate::storage::{
        install_manifest, load_installed_manifests, load_kv_file, load_toml_file,
        save_kv_preserving_comments, save_toml_atomic,
    };

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("noren_sdk_test_{tag}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn protocol_roundtrip_and_unknown_fields() {
        let raw = r#"{"v":1,"t":"hello","module":"notch","version":"0.1.0","proto":[1,1],"pid":42,"provides":["osd.workspace"],"wants":["workspace.state"],"future_extra":true}"#;
        let parsed: Message = serde_json::from_str(raw).unwrap();
        match parsed {
            Message::Hello { module, pid, .. } => {
                assert_eq!(module, "notch");
                assert_eq!(pid, 42);
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn theme_resolution_and_hex_helpers() {
        assert_eq!(parse_hex_color("#4FA8F5"), Some((0x4F, 0xA8, 0xF5)));
        assert_eq!(parse_hex_color("off"), None);
        assert_eq!(format_hex_color(0x4F, 0xA8, 0xF5), "#4FA8F5");

        let cfg = ThemeConfig {
            mode: ThemeMode::Dark,
            material: ThemeMaterial::Acrylic,
            accent_mode: AccentMode::Custom,
            custom_accent: Some("#E06C75".to_string()),
        };
        let resolved = cfg.resolve();
        assert!(resolved.is_dark);
        assert_eq!(resolved.material, ThemeMaterial::Acrylic);
        assert_eq!(resolved.accent_rgb, (0xE0, 0x6C, 0x75));
    }

    #[test]
    fn key_chord_validation() {
        assert!(validate_key_chord("win+tab"));
        assert!(validate_key_chord("ctrl+alt+space"));
        assert!(validate_key_chord("off"));
        assert!(!validate_key_chord("tab"));
        assert!(!validate_key_chord("super+tab"));
    }

    #[test]
    fn kv_atomic_save_preserves_comments_and_rules() {
        let dir = unique_temp_dir("kv");
        let conf = dir.join("noren-wm.conf");
        fs::write(
            &conf,
            "# Header comment\ngap = 16\n#bind.snap_up = win+shift+up\nrule = exe:vlc.exe, float\n",
        )
        .unwrap();

        let manifest = ModuleManifest {
            id: "wm".to_string(),
            name: "WM".to_string(),
            version: "0.1.0".to_string(),
            executable: None,
            config_format: ConfigFormat::Kv,
            config_file: "noren-wm.conf".to_string(),
            groups: vec![FieldGroup {
                id: "layout".to_string(),
                title: "Layout".to_string(),
                fields: vec![
                    FieldDef {
                        key: "gap".to_string(),
                        label: "Gap".to_string(),
                        kind: FieldKind::Number,
                        description: None,
                        requires_running: false,
                        default_bool: None,
                        default_number: Some(16.0),
                        default_text: None,
                        min: Some(0.0),
                        max: Some(200.0),
                        step: Some(1.0),
                        choices: Vec::new(),
                        nullable: false,
                        placeholder: None,
                        command: None,
                    },
                    FieldDef {
                        key: "overview_hint".to_string(),
                        label: "Hint".to_string(),
                        kind: FieldKind::Boolean,
                        description: None,
                        requires_running: false,
                        default_bool: Some(true),
                        default_number: None,
                        default_text: None,
                        min: None,
                        max: None,
                        step: None,
                        choices: Vec::new(),
                        nullable: false,
                        placeholder: None,
                        command: None,
                    },
                ],
            }],
            advanced_editors: Vec::new(),
        };

        let mut vals = load_kv_file(&conf, &manifest).unwrap();
        assert_eq!(vals.get("gap"), Some(&FieldValue::Number(16.0)));
        vals.insert("gap".to_string(), FieldValue::Number(24.0));
        vals.insert("overview_hint".to_string(), FieldValue::Bool(false));
        save_kv_preserving_comments(&conf, &manifest, &vals).unwrap();

        let updated = fs::read_to_string(&conf).unwrap();
        assert!(updated.contains("# Header comment"));
        assert!(updated.contains("#bind.snap_up = win+shift+up"));
        assert!(updated.contains("rule = exe:vlc.exe, float"));
        assert!(updated.contains("gap = 24"));
        assert!(updated.contains("overview_hint = off"));

        let reloaded = load_kv_file(&conf, &manifest).unwrap();
        assert_eq!(reloaded.get("gap"), Some(&FieldValue::Number(24.0)));
        assert_eq!(reloaded.get("overview_hint"), Some(&FieldValue::Bool(false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kv_color_field_preserves_and_normalizes_accent() {
        let dir = unique_temp_dir("kv_accent");
        let conf = dir.join("noren-wm.conf");
        fs::write(&conf, "focus_border = windows\n").unwrap();

        let manifest = ModuleManifest {
            id: "wm".to_string(),
            name: "WM".to_string(),
            version: "0.1.0".to_string(),
            executable: None,
            config_format: ConfigFormat::Kv,
            config_file: "noren-wm.conf".to_string(),
            groups: vec![FieldGroup {
                id: "colors".to_string(),
                title: "Colors".to_string(),
                fields: vec![FieldDef {
                    key: "focus_border".to_string(),
                    label: "Focus Border".to_string(),
                    kind: FieldKind::Color,
                    description: None,
                    requires_running: false,
                    default_bool: None,
                    default_number: None,
                    default_text: Some("off".to_string()),
                    min: None,
                    max: None,
                    step: None,
                    choices: Vec::new(),
                    nullable: true,
                    placeholder: None,
                    command: None,
                }],
            }],
            advanced_editors: Vec::new(),
        };

        let vals = load_kv_file(&conf, &manifest).unwrap();
        save_kv_preserving_comments(&conf, &manifest, &vals).unwrap();
        let saved = fs::read_to_string(&conf).unwrap();
        assert!(saved.contains("focus_border = accent"), "{saved}");

        let mut custom_vals = load_kv_file(&conf, &manifest).unwrap();
        assert_eq!(
            custom_vals.get("focus_border"),
            Some(&FieldValue::Text("accent".to_string()))
        );
        custom_vals.insert("focus_border".to_string(), FieldValue::Text("Accent".to_string()));
        save_kv_preserving_comments(&conf, &manifest, &custom_vals).unwrap();
        let saved2 = fs::read_to_string(&conf).unwrap();
        assert!(saved2.contains("focus_border = accent"), "{saved2}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_save_and_manifest_install() {
        let dir = unique_temp_dir("toml");
        std::env::set_var("NOREN_CONFIG_DIR", &dir);

        let manifest = ModuleManifest {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: "0.1.0".to_string(),
            executable: None,
            config_format: ConfigFormat::Toml,
            config_file: "config.toml".to_string(),
            groups: vec![FieldGroup {
                id: "g".to_string(),
                title: "G".to_string(),
                fields: vec![FieldDef {
                    key: "enabled".to_string(),
                    label: "Enabled".to_string(),
                    kind: FieldKind::Boolean,
                    description: None,
                    requires_running: false,
                    default_bool: Some(true),
                    default_number: None,
                    default_text: None,
                    min: None,
                    max: None,
                    step: None,
                    choices: Vec::new(),
                    nullable: false,
                    placeholder: None,
                    command: None,
                }],
            }],
            advanced_editors: Vec::new(),
        };

        install_manifest(&manifest).unwrap();
        let loaded = load_installed_manifests();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "demo");

        let cfg_path = dir.join("demo").join("config.toml");
        let mut vals = HashMap::new();
        vals.insert("enabled".to_string(), FieldValue::Bool(false));
        save_toml_atomic(&cfg_path, &manifest, &vals).unwrap();
        let back = load_toml_file(&cfg_path, &manifest).unwrap();
        assert_eq!(back.get("enabled"), Some(&FieldValue::Bool(false)));

        std::env::remove_var("NOREN_CONFIG_DIR");
        let _ = fs::remove_dir_all(&dir);
    }
}
