use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use noren_sdk::control_center::{maybe_run_control_center_from_args, open_control_center};
use noren_sdk::paths::module_config_path;
use noren_sdk::protocol::{
    CAP_LAUNCHER_ACTIONS, CAP_LAYOUT_RESERVED_AREA, CAP_OSD_WORKSPACE, CAP_SETTINGS_SCHEMA,
    CAP_WORKSPACE_STATE,
};
use noren_sdk::schema::{ConfigFormat, FieldDef, FieldGroup, FieldKind, FieldValue, ModuleManifest};
use noren_sdk::storage::{
    install_manifest, load_config_values, load_installed_manifests, save_config_values,
};
use noren_sdk::theme::{
    AccentMode, ResolvedTheme, ThemeConfig, ThemeMaterial, ThemeMode, load_theme, save_theme,
};
use noren_sdk::{NipEvent, NipNode, NodeConfig};
use serde_json::{Value, json};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DT_LEFT, DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint,
    FillRect, InvalidateRect, PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
    GetMessageW, IDC_ARROW, LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SW_SHOW, ShowWindow, TranslateMessage, WM_APP, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_PAINT,
    WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

const WM_APP_REFRESH: u32 = WM_APP + 1;
const ID_BTN_OPEN_CC: usize = 1001;
const ID_BTN_PING: usize = 1002;

struct SharedTestbedState {
    module: String,
    theme_cfg: ThemeConfig,
    resolved: ResolvedTheme,
    peers: Vec<String>,
    last_activity: String,
}

static STATE: Mutex<Option<Arc<Mutex<SharedTestbedState>>>> = Mutex::new(None);
static NODE_REF: Mutex<Option<Arc<NipNode>>> = Mutex::new(None);

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_testbed_manifest(module: &str) -> ModuleManifest {
    ModuleManifest {
        id: module.to_string(),
        name: format!("Testbed ({module})"),
        version: "0.1.0".to_string(),
        executable: std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        config_format: ConfigFormat::Toml,
        config_file: "config.toml".to_string(),
        groups: vec![FieldGroup {
            id: "general".to_string(),
            title: "Testbed Settings".to_string(),
            fields: vec![
                FieldDef {
                    key: "enabled".to_string(),
                    label: "Module Active".to_string(),
                    kind: FieldKind::Boolean,
                    description: Some("Toggle testbed simulation".to_string()),
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
                FieldDef {
                    key: "reserved_px".to_string(),
                    label: "Mock Reserved Area (px)".to_string(),
                    kind: FieldKind::Number,
                    description: None,
                    requires_running: false,
                    default_bool: None,
                    default_number: Some(40.0),
                    default_text: None,
                    min: Some(0.0),
                    max: Some(120.0),
                    step: Some(4.0),
                    choices: Vec::new(),
                    nullable: false,
                    placeholder: None,
                    command: None,
                },
                FieldDef {
                    key: "osd_style".to_string(),
                    label: "Mock OSD Style".to_string(),
                    kind: FieldKind::Choice,
                    description: None,
                    requires_running: false,
                    default_bool: None,
                    default_number: None,
                    default_text: Some("pill".to_string()),
                    min: None,
                    max: None,
                    step: None,
                    choices: vec!["pill".to_string(), "compact".to_string()],
                    nullable: false,
                    placeholder: None,
                    command: None,
                },
                FieldDef {
                    key: "highlight".to_string(),
                    label: "Highlight Color".to_string(),
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
                },
                FieldDef {
                    key: "note".to_string(),
                    label: "Test Note".to_string(),
                    kind: FieldKind::Text,
                    description: None,
                    requires_running: false,
                    default_bool: None,
                    default_number: None,
                    default_text: Some("hello noren".to_string()),
                    min: None,
                    max: None,
                    step: None,
                    choices: Vec::new(),
                    nullable: false,
                    placeholder: Some("text".to_string()),
                    command: None,
                },
                FieldDef {
                    key: "hotkey".to_string(),
                    label: "Sample Hotkey".to_string(),
                    kind: FieldKind::KeyChord,
                    description: None,
                    requires_running: false,
                    default_bool: None,
                    default_number: None,
                    default_text: Some("win+shift+t".to_string()),
                    min: None,
                    max: None,
                    step: None,
                    choices: Vec::new(),
                    nullable: false,
                    placeholder: None,
                    command: None,
                },
                FieldDef {
                    key: "ping_action".to_string(),
                    label: "Live Ping Action".to_string(),
                    kind: FieldKind::Action,
                    description: None,
                    requires_running: true,
                    default_bool: None,
                    default_number: None,
                    default_text: None,
                    min: None,
                    max: None,
                    step: None,
                    choices: Vec::new(),
                    nullable: false,
                    placeholder: None,
                    command: Some("ping".to_string()),
                },
            ],
        }],
        advanced_editors: Vec::new(),
    }
}

fn handle_nip_event(
    ev: NipEvent,
    manifest: &ModuleManifest,
    node: &NipNode,
    state: &Arc<Mutex<SharedTestbedState>>,
    hwnd_raw: isize,
) {
    let updated;
    match ev {
        NipEvent::PeerConnected(p) => {
            let mut st = state.lock().unwrap();
            st.peers = node.peers().into_iter().map(|x| x.module).collect();
            st.last_activity = format!("Peer connected: {} (pid {})", p.module, p.pid);
            updated = true;
        }
        NipEvent::PeerDisconnected { module, graceful } => {
            let mut st = state.lock().unwrap();
            st.peers = node.peers().into_iter().map(|x| x.module).collect();
            st.last_activity = format!("Peer left: {module} (graceful={graceful})");
            updated = true;
        }
        NipEvent::StateReceived { from, cap, seq, .. } => {
            let mut st = state.lock().unwrap();
            st.last_activity = format!("State from {from} [{cap}] seq={seq}");
            updated = true;
        }
        NipEvent::EventReceived {
            from, name, data, ..
        } => {
            let mut st = state.lock().unwrap();
            st.last_activity = format!("Event from {from}: {name} {data}");
            updated = true;
        }
        NipEvent::ThemeChanged(cfg) => {
            let resolved = cfg.resolve();
            let mut st = state.lock().unwrap();
            st.theme_cfg = cfg;
            st.resolved = resolved;
            st.last_activity = format!(
                "theme.changed -> dark={} mat={:?} accent=#{:02X}{:02X}{:02X}",
                resolved.is_dark,
                resolved.material,
                resolved.accent_rgb.0,
                resolved.accent_rgb.1,
                resolved.accent_rgb.2
            );
            updated = true;
        }
        NipEvent::Command {
            from,
            cap,
            name,
            data,
            reply,
            ..
        } => {
            match name.as_str() {
                "ping" => {
                    let _ = reply.ok(json!({
                        "pong_from": manifest.id,
                        "echo": data,
                    }));
                }
                "get_theme" => {
                    let st = state.lock().unwrap();
                    let _ = reply.ok(json!({
                        "config": st.theme_cfg,
                        "is_dark": st.resolved.is_dark,
                        "material": st.resolved.material,
                        "accent_rgb": [st.resolved.accent_rgb.0, st.resolved.accent_rgb.1, st.resolved.accent_rgb.2],
                    }));
                }
                "config.apply" => {
                    if let Ok(vals) = serde_json::from_value::<HashMap<String, FieldValue>>(data) {
                        let path = module_config_path(&manifest.id, &manifest.config_file);
                        match save_config_values(&path, manifest, &vals) {
                            Ok(()) => {
                                let _ = reply.ok(json!({ "saved": true }));
                            }
                            Err(e) => {
                                let _ = reply.err(e.to_string());
                            }
                        }
                    } else {
                        let _ = reply.err("invalid config payload");
                    }
                }
                other => {
                    let _ = reply.ok(json!({
                        "handled_by": manifest.id,
                        "cap": cap,
                        "cmd": other,
                    }));
                }
            }
            let mut st = state.lock().unwrap();
            st.last_activity = format!("Command '{name}' from {from}");
            updated = true;
        }
    }

    if updated && hwnd_raw != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd_raw as *mut c_void)),
                WM_APP_REFRESH,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

unsafe fn apply_dwm_theme(hwnd: HWND, resolved: ResolvedTheme) {
    let dark: u32 = if resolved.is_dark { 1 } else { 0 };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        (&dark as *const u32).cast::<c_void>(),
        4,
    );
    let corner: u32 = DWMWCP_ROUND.0 as u32;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        (&corner as *const u32).cast::<c_void>(),
        4,
    );
    let backdrop: u32 = if !resolved.transparency_enabled {
        1 // DWMSBT_DISABLE
    } else {
        match resolved.material {
            ThemeMaterial::Mica => 2,    // DWMSBT_MAINWINDOW
            ThemeMaterial::Acrylic => 3, // DWMSBT_TRANSIENTWINDOW
        }
    };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_SYSTEMBACKDROP_TYPE,
        (&backdrop as *const u32).cast::<c_void>(),
        4,
    );
}

fn colorref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let _ = CreateWindowExW(
                Default::default(),
                w!("BUTTON"),
                w!("Open Control Center"),
                WS_CHILD | WS_VISIBLE | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(BS_PUSHBUTTON as u32),
                16,
                185,
                170,
                32,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(ID_BTN_OPEN_CC as *mut c_void)),
                None,
                None,
            );
            let _ = CreateWindowExW(
                Default::default(),
                w!("BUTTON"),
                w!("Ping Connected Peers"),
                WS_CHILD | WS_VISIBLE | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(BS_PUSHBUTTON as u32),
                198,
                185,
                170,
                32,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(ID_BTN_PING as *mut c_void)),
                None,
                None,
            );
            if let Some(st) = STATE.lock().unwrap().as_ref() {
                let resolved = st.lock().unwrap().resolved;
                apply_dwm_theme(hwnd, resolved);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            if id == ID_BTN_OPEN_CC {
                let module = STATE
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.lock().unwrap().module.clone());
                let _ = open_control_center(module.as_deref());
            } else if id == ID_BTN_PING {
                if let Some(node) = NODE_REF.lock().unwrap().as_ref().cloned() {
                    let _ = node.emit_event(
                        None,
                        Some("testbed.state"),
                        "ping_broadcast",
                        json!({ "from": node.module_name() }),
                    );
                }
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            if let Some(st) = STATE.lock().unwrap().as_ref() {
                let resolved = st.lock().unwrap().resolved;
                apply_dwm_theme(hwnd, resolved);
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);

            let snapshot = STATE
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| {
                    let g = s.lock().unwrap();
                    (
                        g.module.clone(),
                        g.resolved,
                        g.peers.clone(),
                        g.last_activity.clone(),
                    )
                });

            if let Some((module, resolved, peers, last_activity)) = snapshot {
                let bg = if resolved.is_dark {
                    colorref(28, 30, 36)
                } else {
                    colorref(243, 243, 243)
                };
                let fg = if resolved.is_dark {
                    colorref(242, 242, 242)
                } else {
                    colorref(24, 24, 24)
                };
                let bg_brush = CreateSolidBrush(bg);
                FillRect(hdc, &rc, bg_brush);
                let _ = DeleteObject(bg_brush.into());

                let (ar, ag, ab) = resolved.accent_rgb;
                let accent_rect = RECT {
                    left: 16,
                    top: 14,
                    right: (rc.right - 16).max(32),
                    bottom: 24,
                };
                let accent_brush = CreateSolidBrush(colorref(ar, ag, ab));
                FillRect(hdc, &accent_rect, accent_brush);
                let _ = DeleteObject(accent_brush.into());

                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, fg);
                let peers_str = if peers.is_empty() {
                    "(none)".to_string()
                } else {
                    peers.join(", ")
                };
                let body = format!(
                    "Module: {module} (pid {})\nTheme: {} | {:?} | Accent #{ar:02X}{ag:02X}{ab:02X}\nPeers: {peers_str}\nLast: {last_activity}",
                    std::process::id(),
                    if resolved.is_dark { "Dark" } else { "Light" },
                    resolved.material
                );
                let mut text_wide: Vec<u16> = body.encode_utf16().collect();
                let mut text_rc = RECT {
                    left: 16,
                    top: 34,
                    right: (rc.right - 16).max(100),
                    bottom: 175,
                };
                DrawTextW(hdc, &mut text_wide, &mut text_rc, DT_LEFT | DT_WORDBREAK);
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn run_gui(module: &str, provides: Vec<String>, wants: Vec<String>) -> Result<(), String> {
    let manifest = build_testbed_manifest(module);
    install_manifest(&manifest).map_err(|e| e.to_string())?;
    let theme_cfg = load_theme();
    let resolved = theme_cfg.resolve();

    let mut initial_state = HashMap::new();
    initial_state.insert(
        "testbed.state".to_string(),
        json!({ "module": module, "ready": true }),
    );
    initial_state.insert(
        CAP_OSD_WORKSPACE.to_string(),
        json!({ "enabled": true }),
    );
    initial_state.insert(
        CAP_LAYOUT_RESERVED_AREA.to_string(),
        json!({ "top_px": 40 }),
    );

    let (node, rx) = NipNode::start(NodeConfig {
        module: module.to_string(),
        version: "0.1.0".to_string(),
        provides,
        wants,
        allowlist: None,
        initial_state,
    })
    .map_err(|e| e.to_string())?;
    let node = Arc::new(node);
    *NODE_REF.lock().unwrap() = Some(Arc::clone(&node));

    let state = Arc::new(Mutex::new(SharedTestbedState {
        module: module.to_string(),
        theme_cfg,
        resolved,
        peers: Vec::new(),
        last_activity: "Waiting for peers...".to_string(),
    }));
    *STATE.lock().unwrap() = Some(Arc::clone(&state));

    let class_name = to_wide_null("NorenTestbedWindow");
    let title = to_wide_null(&format!("Noren Testbed — {module}"));
    let hwnd_raw: isize;
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            420,
            275,
            None,
            None,
            Some(hinst),
            None,
        )
        .map_err(|e| e.to_string())?;
        hwnd_raw = hwnd.0 as isize;
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);
    let node_bg = Arc::clone(&node);
    let state_bg = Arc::clone(&state);
    let manifest_bg = manifest.clone();
    let listener = std::thread::spawn(move || {
        while !stop_bg.load(Ordering::Acquire) {
            match rx.recv_timeout(Duration::from_millis(120)) {
                Ok(ev) => handle_nip_event(ev, &manifest_bg, &node_bg, &state_bg, hwnd_raw),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    stop.store(true, Ordering::Release);
    node.shutdown();
    let _ = listener.join();
    Ok(())
}

fn run_headless(module: &str, ttl_secs: u64) -> Result<(), String> {
    let manifest = build_testbed_manifest(module);
    install_manifest(&manifest).map_err(|e| e.to_string())?;
    let theme_cfg = load_theme();
    let resolved = theme_cfg.resolve();

    let mut initial_state = HashMap::new();
    initial_state.insert(
        "testbed.state".to_string(),
        json!({ "module": module, "ready": true }),
    );
    let (node, rx) = NipNode::start(NodeConfig {
        module: module.to_string(),
        version: "0.1.0".to_string(),
        provides: vec!["testbed.state".to_string(), CAP_SETTINGS_SCHEMA.to_string()],
        wants: vec!["testbed.state".to_string()],
        allowlist: None,
        initial_state,
    })
    .map_err(|e| e.to_string())?;

    let state = Arc::new(Mutex::new(SharedTestbedState {
        module: module.to_string(),
        theme_cfg,
        resolved,
        peers: Vec::new(),
        last_activity: String::new(),
    }));

    println!("HEADLESS_READY module={module} pid={}", std::process::id());
    let deadline = Instant::now() + Duration::from_secs(ttl_secs);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => handle_nip_event(ev, &manifest, &node, &state, 0),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    node.shutdown();
    Ok(())
}

struct ChildGuard(Option<Child>);
impl ChildGuard {
    fn kill_now(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

fn run_self_test() -> Result<(), String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("noren_phase1_selftest_{nanos}"));
    let prefix = format!("noren_p1_{}_", std::process::id());
    std::env::set_var("NOREN_CONFIG_DIR", &temp_dir);
    std::env::set_var("NOREN_PIPE_PREFIX", &prefix);

    struct TempCleanup(std::path::PathBuf);
    impl Drop for TempCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = TempCleanup(temp_dir.clone());

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    // Register a stopped testbed manifest (Exit Criterion #2).
    let stopped_manifest = build_testbed_manifest("testbed-off");
    install_manifest(&stopped_manifest).map_err(|e| e.to_string())?;

    // Spawn two live headless testbed instances (Exit Criterion #1).
    let child_a = ChildGuard(Some(
        Command::new(&exe)
            .args(["--headless", "--module", "testbed-a", "--ttl", "30"])
            .env("NOREN_CONFIG_DIR", &temp_dir)
            .env("NOREN_PIPE_PREFIX", &prefix)
            .spawn()
            .map_err(|e| format!("failed spawning testbed-a: {e}"))?,
    ));
    let mut child_b = ChildGuard(Some(
        Command::new(&exe)
            .args(["--headless", "--module", "testbed-b", "--ttl", "30"])
            .env("NOREN_CONFIG_DIR", &temp_dir)
            .env("NOREN_PIPE_PREFIX", &prefix)
            .spawn()
            .map_err(|e| format!("failed spawning testbed-b: {e}"))?,
    ));

    // Start an SDK coordinator/observer node (playing Control Center role).
    let (cc_node, cc_rx) = NipNode::start(NodeConfig {
        module: "control-center".to_string(),
        version: "0.1.0".to_string(),
        provides: vec!["controlcenter.focus".to_string()],
        wants: vec!["testbed.state".to_string()],
        allowlist: None,
        initial_state: HashMap::new(),
    })
    .map_err(|e| e.to_string())?;

    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if cc_node.is_peer_connected("testbed-a") && cc_node.is_peer_connected("testbed-b") {
            break;
        }
        let _ = cc_rx.recv_timeout(Duration::from_millis(80));
    }
    if !cc_node.is_peer_connected("testbed-a") || !cc_node.is_peer_connected("testbed-b") {
        return Err(format!(
            "expected testbed-a and testbed-b connected, got: {:?}",
            cc_node.peers()
        ));
    }

    // Verify ping cmd/reply to both running testbeds.
    let pong_a = cc_node
        .send_cmd("testbed-a", "testbed.state", "ping", json!({"n": 1}), Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let pong_b = cc_node
        .send_cmd("testbed-b", "testbed.state", "ping", json!({"n": 2}), Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    assert_eq!(pong_a.get("pong_from").and_then(|v| v.as_str()), Some("testbed-a"));
    assert_eq!(pong_b.get("pong_from").and_then(|v| v.as_str()), Some("testbed-b"));

    // Crash testbed-b and verify testbed-a + cc_node survive & detect crash.
    child_b.kill_now();
    let crash_deadline = Instant::now() + Duration::from_secs(4);
    let mut saw_crash = false;
    while Instant::now() < crash_deadline {
        if let Ok(NipEvent::PeerDisconnected { module, graceful }) =
            cc_rx.recv_timeout(Duration::from_millis(80))
        {
            if module == "testbed-b" && !graceful {
                saw_crash = true;
                break;
            }
        }
        if !cc_node.is_peer_connected("testbed-b") {
            saw_crash = true;
            break;
        }
    }
    if !saw_crash {
        return Err("did not detect testbed-b crash".to_string());
    }
    // testbed-a must still be healthy and responsive.
    cc_node
        .send_cmd("testbed-a", "testbed.state", "ping", json!({"alive": true}), Duration::from_secs(2))
        .map_err(|e| e.to_string())?;

    // Respawn testbed-b so both running instances are present for Exit Criteria #2 and #3.
    let mut child_b2 = ChildGuard(Some(
        Command::new(&exe)
            .args(["--headless", "--module", "testbed-b", "--ttl", "20"])
            .env("NOREN_CONFIG_DIR", &temp_dir)
            .env("NOREN_PIPE_PREFIX", &prefix)
            .spawn()
            .map_err(|e| format!("failed respawning testbed-b: {e}"))?,
    ));
    let recon_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < recon_deadline {
        if cc_node.is_peer_connected("testbed-b") {
            break;
        }
        let _ = cc_rx.recv_timeout(Duration::from_millis(80));
    }
    if !cc_node.is_peer_connected("testbed-b") {
        return Err("testbed-b did not reconnect after restart".to_string());
    }
    println!("EXIT_1_DISCOVERY_EXCHANGE_CRASH_RECOVERY=PASS");

    // Exit Criterion #2: Control Center tab list sees 2 running + 1 stopped (off) + atomic edit on stopped.
    let manifests = load_installed_manifests();
    let tab_statuses: Vec<(String, bool)> = manifests
        .iter()
        .map(|m| (m.id.clone(), cc_node.is_peer_connected(&m.id)))
        .collect();
    let has_a = tab_statuses.iter().any(|(id, on)| id == "testbed-a" && *on);
    let has_b = tab_statuses.iter().any(|(id, on)| id == "testbed-b" && *on);
    let has_off = tab_statuses.iter().any(|(id, on)| id == "testbed-off" && !*on);
    if !(has_a && has_b && has_off) {
        return Err(format!("unexpected tab statuses: {tab_statuses:?}"));
    }
    let off_cfg_path = module_config_path("testbed-off", &stopped_manifest.config_file);
    let mut off_vals = load_config_values(&off_cfg_path, &stopped_manifest).map_err(|e| e.to_string())?;
    off_vals.insert("reserved_px".to_string(), FieldValue::Number(56.0));
    save_config_values(&off_cfg_path, &stopped_manifest, &off_vals).map_err(|e| e.to_string())?;
    let reloaded_off = load_config_values(&off_cfg_path, &stopped_manifest).map_err(|e| e.to_string())?;
    assert_eq!(reloaded_off.get("reserved_px"), Some(&FieldValue::Number(56.0)));
    println!("EXIT_2_RUNNING_AND_STOPPED_TABS=PASS ({tab_statuses:?})");

    // Exit Criterion #3: Theme change saved to general.toml + theme.changed updates BOTH running testbeds.
    let new_theme = ThemeConfig {
        mode: ThemeMode::Dark,
        material: ThemeMaterial::Acrylic,
        accent_mode: AccentMode::Custom,
        custom_accent: Some("#E06C75".to_string()),
    };
    save_theme(&new_theme).map_err(|e| e.to_string())?;
    cc_node.broadcast_theme_changed(&new_theme).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(200));

    for target in ["testbed-a", "testbed-b"] {
        let reply = cc_node
            .send_cmd(target, "testbed.state", "get_theme", Value::Null, Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let accent = reply.get("accent_rgb").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mat = reply.get("material").and_then(|v| v.as_str()).unwrap_or("");
        let is_dark = reply.get("is_dark").and_then(|v| v.as_bool()).unwrap_or(false);
        if !is_dark
            || mat != "acrylic"
            || accent.len() != 3
            || accent[0].as_u64() != Some(0xE0)
            || accent[1].as_u64() != Some(0x6C)
            || accent[2].as_u64() != Some(0x75)
        {
            return Err(format!("theme did not update on {target}: {reply}"));
        }
    }
    println!("EXIT_3_THEME_CHANGED_BROADCAST=PASS");

    cc_node.shutdown();
    drop(child_a);
    child_b2.kill_now();
    println!("PHASE1_SELF_TEST=ALL_PASS");
    Ok(())
}

fn main() {
    if let Some(res) = maybe_run_control_center_from_args() {
        if let Err(e) = res {
            eprintln!("Control Center error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let args: Vec<String> = std::env::args().collect();
    let arg_value = |flag: &str| -> Option<String> {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].clone())
    };

    if args.iter().any(|a| a == "--self-test") {
        if let Err(e) = run_self_test() {
            eprintln!("PHASE1_SELF_TEST_FAILED: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Some(stopped_name) = arg_value("--register-stopped") {
        let manifest = build_testbed_manifest(&stopped_name);
        match install_manifest(&manifest) {
            Ok(p) => println!("Registered stopped testbed manifest at {}", p.display()),
            Err(e) => {
                eprintln!("Failed registering stopped manifest: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let module = arg_value("--module").unwrap_or_else(|| "testbed-a".to_string());
    if args.iter().any(|a| a == "--headless") {
        let ttl = arg_value("--ttl")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        if let Err(e) = run_headless(&module, ttl) {
            eprintln!("Headless testbed error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let parse_list = |s: Option<String>, default: &[&str]| -> Vec<String> {
        s.map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| default.iter().map(|s| (*s).to_string()).collect())
    };

    let provides = parse_list(
        arg_value("--provides"),
        &[
            "testbed.state",
            CAP_OSD_WORKSPACE,
            CAP_LAYOUT_RESERVED_AREA,
            CAP_LAUNCHER_ACTIONS,
            CAP_SETTINGS_SCHEMA,
        ],
    );
    let wants = parse_list(
        arg_value("--wants"),
        &[
            "testbed.state",
            CAP_WORKSPACE_STATE,
            CAP_OSD_WORKSPACE,
            CAP_LAYOUT_RESERVED_AREA,
        ],
    );

    if let Err(e) = run_gui(&module, provides, wants) {
        eprintln!("Testbed GUI error: {e}");
        std::process::exit(1);
    }
}
