use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current NIP wire protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Well-known NIP capability identifiers from `NOREN.md` §6.5.
pub const CAP_WORKSPACE_STATE: &str = "workspace.state";
pub const CAP_WINDOW_CONTROL: &str = "window.control";
pub const CAP_LAYOUT_RESERVED_AREA: &str = "layout.reserved-area";
pub const CAP_OSD_WORKSPACE: &str = "osd.workspace";
pub const CAP_LAUNCHER_ACTIONS: &str = "launcher.actions";
pub const CAP_SETTINGS_SCHEMA: &str = "settings.schema";
pub const CAP_CONTROLCENTER_FOCUS: &str = "controlcenter.focus";
pub const CAP_TRAY_ACTIONS: &str = "tray.actions";

/// Well-known global event names outside capabilities.
pub const EVENT_THEME_CHANGED: &str = "theme.changed";

/// NDJSON wire message exchanged between Noren modules (`NOREN.md` §6.3).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Message {
    Hello {
        v: u32,
        module: String,
        version: String,
        proto: [u32; 2],
        pid: u32,
        #[serde(default)]
        elevated: bool,
        #[serde(default)]
        integrity: String,
        #[serde(default)]
        provides: Vec<String>,
        #[serde(default)]
        wants: Vec<String>,
    },
    Welcome {
        v: u32,
        module: String,
        version: String,
        proto: u32,
        pid: u32,
        #[serde(default)]
        elevated: bool,
        #[serde(default)]
        integrity: String,
        #[serde(default)]
        provides: Vec<String>,
        #[serde(default)]
        wants: Vec<String>,
    },
    Event {
        v: u32,
        #[serde(default)]
        cap: Option<String>,
        name: String,
        #[serde(default)]
        data: Value,
    },
    Cmd {
        v: u32,
        id: String,
        cap: String,
        name: String,
        #[serde(default)]
        data: Value,
    },
    Reply {
        v: u32,
        id: String,
        ok: bool,
        #[serde(default)]
        data: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    },
    State {
        v: u32,
        cap: String,
        seq: u64,
        data: Value,
    },
    Bye {
        v: u32,
        module: String,
        #[serde(default)]
        reason: String,
    },
}

impl Message {
    pub fn to_ndjson_line(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}
