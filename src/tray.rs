use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WAIT_ABANDONED, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, MUTEX_ALL_ACCESS,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, KillTimer,
    LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, GWLP_USERDATA,
    HICON, HMENU, IDI_APPLICATION, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
    SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::branding;
use crate::error::SdkError;
use crate::node::NipNode;
use crate::paths::tray_mutex_name;
use crate::pipe::to_wide_null;
use crate::protocol::CAP_TRAY_ACTIONS;
use crate::security::{current_user_sid, PipeSecurityDescriptor};

const WM_TRAY_ICON: u32 = WM_APP + 40;
const WM_TRAY_REEVALUATE: u32 = WM_APP + 41;
const WM_TRAY_YIELD: u32 = WM_APP + 42;
const WM_TRAY_REFRESH: u32 = WM_APP + 43;
const WM_TRAY_SHUTDOWN: u32 = WM_APP + 44;
const WM_CONTEXTMENU_EVENT: u32 = 0x007B;

const TIMER_ELECTION: usize = 1;
const ELECTION_POLL_MS: u32 = 300;
const TRAY_UID: u32 = 1;

static CLASS_INIT: Once = Once::new();

fn default_true() -> bool {
    true
}

/// One menu item contributed by a module to the unified Noren system tray (`tray.actions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayMenuItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub checked: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub separator_before: bool,
    #[serde(default)]
    pub open_path: Option<String>,
}

/// Per-module menu state published under `tray.actions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayModulePayload {
    pub module: String,
    pub title: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub items: Vec<TrayMenuItem>,
}

/// Deterministic snapshot of the unified Noren system tray menu across connected modules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedTrayMenu {
    pub settings_label: String,
    pub standalone_mode: bool,
    pub modules: Vec<TrayModulePayload>,
    pub quit_all_label: Option<String>,
}

/// Configuration for starting `TrayService`.
pub struct TrayConfig {
    pub module: String,
    pub tooltip: String,
    pub initial_payload: TrayModulePayload,
    pub node: Option<Arc<NipNode>>,
    pub on_local_action: Arc<dyn Fn(&str) + Send + Sync + 'static>,
    pub on_local_quit: Arc<dyn Fn() + Send + Sync + 'static>,
    pub on_open_settings: Option<Arc<dyn Fn(Option<&str>) + Send + Sync + 'static>>,
    pub headless: bool,
}

struct TrayShared {
    cfg_module: String,
    cfg_tooltip: String,
    headless: bool,
    node: Option<Arc<NipNode>>,
    local_payload: Mutex<TrayModulePayload>,
    on_local_action: Arc<dyn Fn(&str) + Send + Sync + 'static>,
    on_local_quit: Arc<dyn Fn() + Send + Sync + 'static>,
    on_open_settings: Option<Arc<dyn Fn(Option<&str>) + Send + Sync + 'static>>,
    is_host: AtomicBool,
    stopped: AtomicBool,
    hwnd: AtomicIsize,
    last_yield_req: Mutex<Option<Instant>>,
}

struct ThreadOwnedState {
    shared: Arc<TrayShared>,
    mutex_handle: HANDLE,
    owns_mutex: bool,
    icon_added: bool,
    icon_handle: HICON,
    taskbar_created_msg: u32,
    last_tooltip: String,
}

/// Manages single-icon Tray Host election (`Local\noren-tray-owner-<user-sid>`),
/// privilege-split handoff, automatic failover, and the dynamic unified Noren tray menu.
pub struct TrayService {
    shared: Arc<TrayShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TrayService {
    pub fn start(cfg: TrayConfig) -> Result<Self, SdkError> {
        let user_sid = current_user_sid()?;
        let mutex_name = tray_mutex_name(&user_sid);
        let headless = cfg.headless || std::env::var("NOREN_TRAY_HEADLESS").as_deref() == Ok("1");

        if let Some(ref node) = cfg.node {
            if let Ok(val) = serde_json::to_value(&cfg.initial_payload) {
                let _ = node.publish_state(CAP_TRAY_ACTIONS, val);
            }
        }

        let shared = Arc::new(TrayShared {
            cfg_module: cfg.module,
            cfg_tooltip: cfg.tooltip,
            headless,
            node: cfg.node,
            local_payload: Mutex::new(cfg.initial_payload),
            on_local_action: cfg.on_local_action,
            on_local_quit: cfg.on_local_quit,
            on_open_settings: cfg.on_open_settings,
            is_host: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            hwnd: AtomicIsize::new(0),
            last_yield_req: Mutex::new(None),
        });

        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), SdkError>>();
        let thread_shared = Arc::clone(&shared);
        let thread = thread::spawn(move || {
            run_tray_thread(thread_shared, user_sid, mutex_name, ready_tx);
        });

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(SdkError::Ui("tray thread exited before initializing".to_string()))
            }
        }
    }

    pub fn is_host(&self) -> bool {
        self.shared.is_host.load(Ordering::Acquire)
    }

    pub fn update_local_payload(&self, payload: TrayModulePayload) {
        *self.shared.local_payload.lock().unwrap() = payload.clone();
        if let Some(ref node) = self.shared.node {
            if let Ok(val) = serde_json::to_value(&payload) {
                let _ = node.publish_state(CAP_TRAY_ACTIONS, val);
            }
        }
        self.post_hwnd(WM_TRAY_REFRESH);
    }

    pub fn notify_topology_changed(&self) {
        self.post_hwnd(WM_TRAY_REEVALUATE);
        self.post_hwnd(WM_TRAY_REFRESH);
    }

    /// Requests the current elevated host to release the tray mutex to a standard-level peer.
    /// Returns `true` if a yield was scheduled.
    pub fn request_yield(&self, requester_is_elevated: bool) -> bool {
        if requester_is_elevated {
            return false;
        }
        let self_elevated = self
            .shared
            .node
            .as_ref()
            .is_some_and(|n| n.integrity_info().elevated);
        if !self_elevated {
            return false;
        }
        self.post_hwnd(WM_TRAY_YIELD);
        true
    }

    pub fn menu_snapshot(&self) -> UnifiedTrayMenu {
        build_menu_snapshot(&self.shared)
    }

    pub fn shutdown(&self) {
        if self.shared.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let raw_hwnd = self.shared.hwnd.load(Ordering::Acquire);
        if raw_hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw_hwnd as *mut c_void)),
                    WM_TRAY_SHUTDOWN,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn post_hwnd(&self, msg: u32) {
        let raw = self.shared.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    msg,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

impl Drop for TrayService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn module_sort_key(module: &str) -> (u8, &str) {
    let rank = match module {
        "wm" => 0,
        "launcher" => 1,
        "notch" => 2,
        "bar" => 3,
        _ => 4,
    };
    (rank, module)
}

fn build_menu_snapshot(shared: &TrayShared) -> UnifiedTrayMenu {
    let local = shared.local_payload.lock().unwrap().clone();
    let mut modules = vec![local];
    let mut seen = HashSet::new();
    seen.insert(shared.cfg_module.clone());

    if let Some(ref node) = shared.node {
        for peer in node.peers() {
            if !peer.provides.iter().any(|c| c == CAP_TRAY_ACTIONS) {
                continue;
            }
            if !seen.insert(peer.module.clone()) {
                continue;
            }
            let payload = node
                .latest_state(&peer.module, CAP_TRAY_ACTIONS)
                .and_then(|v| serde_json::from_value::<TrayModulePayload>(v).ok())
                .unwrap_or_else(|| TrayModulePayload {
                    module: peer.module.clone(),
                    title: peer.module.clone(),
                    paused: false,
                    items: Vec::new(),
                });
            modules.push(payload);
        }
    }

    modules.sort_by(|a, b| module_sort_key(&a.module).cmp(&module_sort_key(&b.module)));
    let standalone_mode = modules.len() <= 1;
    let quit_all_label = (!standalone_mode).then(|| "Quit All".to_string());

    UnifiedTrayMenu {
        settings_label: "Settings (Control Center)".to_string(),
        standalone_mode,
        modules,
        quit_all_label,
    }
}

fn open_tray_mutex(mutex_name: &str, user_sid: &str) -> Result<HANDLE, SdkError> {
    let wide = to_wide_null(mutex_name);
    if let Ok(sd) = PipeSecurityDescriptor::same_user_medium_ml(user_sid) {
        let mut sa = sd.as_security_attributes();
        if let Ok(h) = unsafe { CreateMutexW(Some(&mut sa), false, PCWSTR(wide.as_ptr())) } {
            return Ok(h);
        }
    }
    if let Ok(h) = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, false, PCWSTR(wide.as_ptr())) }
    {
        return Ok(h);
    }
    unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
        .map_err(|e| SdkError::Ui(format!("CreateMutexW failed for {mutex_name}: {e}")))
}

fn run_tray_thread(
    shared: Arc<TrayShared>,
    user_sid: String,
    mutex_name: String,
    ready_tx: mpsc::Sender<Result<(), SdkError>>,
) {
    let mutex_handle = match open_tray_mutex(&mutex_name, &user_sid) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let hinst: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h.into(),
        Err(e) => {
            unsafe {
                let _ = CloseHandle(mutex_handle);
            }
            let _ = ready_tx.send(Err(SdkError::Ui(format!("GetModuleHandleW failed: {e}"))));
            return;
        }
    };

    let class_name = w!("NorenSdkUnifiedTray");
    CLASS_INIT.call_once(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinst,
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    });

    let icon_handle = if shared.headless {
        HICON::default()
    } else {
        branding::create_tray_icon(branding::system_taskbar_is_dark())
            .unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() })
    };
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    let owned = Box::new(ThreadOwnedState {
        shared: Arc::clone(&shared),
        mutex_handle,
        owns_mutex: false,
        icon_added: false,
        icon_handle,
        taskbar_created_msg,
        last_tooltip: String::new(),
    });

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("NorenTray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            unsafe {
                if !owned.icon_handle.is_invalid() {
                    let _ = DestroyIcon(owned.icon_handle);
                }
                let _ = CloseHandle(mutex_handle);
            }
            let _ = ready_tx.send(Err(SdkError::Ui(format!("CreateWindowExW failed: {e}"))));
            return;
        }
    };

    let raw_ptr = Box::into_raw(owned);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_ptr as isize);
        let _ = SetTimer(Some(hwnd), TIMER_ELECTION, ELECTION_POLL_MS, None);
    }
    shared.hwnd.store(hwnd.0 as isize, Ordering::Release);

    unsafe {
        reevaluate_election(&mut *raw_ptr, hwnd, false);
    }
    let _ = ready_tx.send(Ok(()));

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    shared.hwnd.store(0, Ordering::Release);
    shared.is_host.store(false, Ordering::Release);
    unsafe {
        let mut boxed = Box::from_raw(raw_ptr);
        remove_tray_icon(&mut boxed, hwnd);
        if boxed.owns_mutex {
            let _ = ReleaseMutex(boxed.mutex_handle);
            boxed.owns_mutex = false;
        }
        if !boxed.icon_handle.is_invalid() {
            let _ = DestroyIcon(boxed.icon_handle);
        }
        let _ = CloseHandle(boxed.mutex_handle);
    }
}

fn copy_tip(n: &mut NOTIFYICONDATAW, text: &str) {
    let w: Vec<u16> = text.encode_utf16().take(127).collect();
    n.szTip = [0; 128];
    n.szTip[..w.len()].copy_from_slice(&w);
}

fn make_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n = NOTIFYICONDATAW::default();
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = TRAY_UID;
    n
}

fn effective_tooltip(shared: &TrayShared) -> String {
    let snap = build_menu_snapshot(shared);
    if !snap.standalone_mode {
        "Noren".to_string()
    } else {
        shared.cfg_tooltip.clone()
    }
}

unsafe fn add_or_update_tray_icon(state: &mut ThreadOwnedState, hwnd: HWND) {
    if state.shared.headless || !state.owns_mutex {
        return;
    }
    let tip = effective_tooltip(&state.shared);
    let mut n = make_nid(hwnd);
    if !state.icon_added {
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        n.uCallbackMessage = WM_TRAY_ICON;
        n.hIcon = state.icon_handle;
        copy_tip(&mut n, &tip);
        if Shell_NotifyIconW(NIM_ADD, &n).as_bool() {
            state.icon_added = true;
            state.last_tooltip = tip;
        }
    } else if state.last_tooltip != tip {
        n.uFlags = NIF_TIP;
        copy_tip(&mut n, &tip);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
        state.last_tooltip = tip;
    }
}

unsafe fn remove_tray_icon(state: &mut ThreadOwnedState, hwnd: HWND) {
    if !state.icon_added || state.shared.headless {
        state.icon_added = false;
        return;
    }
    let n = make_nid(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &n);
    state.icon_added = false;
    state.last_tooltip.clear();
}

unsafe fn refresh_tray_icon_theme(state: &mut ThreadOwnedState, hwnd: HWND) {
    if state.shared.headless {
        return;
    }
    let Some(new_icon) = branding::create_tray_icon(branding::system_taskbar_is_dark()) else {
        return;
    };
    let old_icon = state.icon_handle;
    state.icon_handle = new_icon;
    if state.owns_mutex && state.icon_added {
        let mut n = make_nid(hwnd);
        n.uFlags = NIF_ICON;
        n.hIcon = new_icon;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
    if !old_icon.is_invalid() {
        let _ = DestroyIcon(old_icon);
    }
}

unsafe fn reevaluate_election(state: &mut ThreadOwnedState, hwnd: HWND, force_yield: bool) {
    if state.shared.stopped.load(Ordering::Acquire) {
        return;
    }
    let (self_elevated, has_standard_peer) = match &state.shared.node {
        Some(node) => {
            let elev = node.integrity_info().elevated;
            let std_peer = node
                .peers()
                .iter()
                .any(|p| !p.elevated && p.provides.iter().any(|c| c == CAP_TRAY_ACTIONS));
            (elev, std_peer)
        }
        None => (false, false),
    };

    let should_yield = self_elevated && (force_yield || has_standard_peer);
    if state.owns_mutex && should_yield {
        remove_tray_icon(state, hwnd);
        let _ = ReleaseMutex(state.mutex_handle);
        state.owns_mutex = false;
        state.shared.is_host.store(false, Ordering::Release);
        if let Some(ref node) = state.shared.node {
            let node = Arc::clone(node);
            let from = state.shared.cfg_module.clone();
            thread::spawn(move || {
                let _ = node.emit_event(
                    None,
                    Some(CAP_TRAY_ACTIONS),
                    "tray.released",
                    json!({ "from": from }),
                );
            });
        }
        return;
    }

    if !state.owns_mutex && !should_yield {
        let wait = WaitForSingleObject(state.mutex_handle, 0);
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            state.owns_mutex = true;
            state.shared.is_host.store(true, Ordering::Release);
            add_or_update_tray_icon(state, hwnd);
        } else if !self_elevated {
            if let Some(ref node) = state.shared.node {
                let elevated_peers: Vec<String> = node
                    .peers()
                    .into_iter()
                    .filter(|p| p.elevated && p.provides.iter().any(|c| c == CAP_TRAY_ACTIONS))
                    .map(|p| p.module)
                    .collect();
                if !elevated_peers.is_empty() {
                    let should_send = {
                        let mut guard = state.shared.last_yield_req.lock().unwrap();
                        let now = Instant::now();
                        match *guard {
                            Some(prev) if now.duration_since(prev) < Duration::from_millis(500) => {
                                false
                            }
                            _ => {
                                *guard = Some(now);
                                true
                            }
                        }
                    };
                    if should_send {
                        for peer in elevated_peers {
                            let node = Arc::clone(node);
                            thread::spawn(move || {
                                let _ = node.send_cmd(
                                    &peer,
                                    CAP_TRAY_ACTIONS,
                                    "tray.yield",
                                    Value::Null,
                                    Duration::from_millis(500),
                                );
                            });
                        }
                    }
                }
            }
        }
    } else if state.owns_mutex {
        add_or_update_tray_icon(state, hwnd);
    }
}

enum MenuAction {
    OpenSettings { tab: Option<String> },
    OpenPath(String),
    LocalItem(String),
    RemoteItem { peer: String, id: String },
    LocalQuit,
    RemoteQuit(String),
    QuitAll,
}

unsafe fn append_module_items(
    menu: HMENU,
    module: &TrayModulePayload,
    is_local: bool,
    actions: &mut Vec<MenuAction>,
) {
    for item in &module.items {
        if item.separator_before {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let mut flags = MF_STRING;
        if item.checked {
            flags |= MF_CHECKED;
        }
        if !item.enabled {
            flags |= MF_GRAYED;
        }
        let cmd_id = 100 + actions.len();
        let label_w = to_wide_null(&item.label);
        let _ = AppendMenuW(menu, flags, cmd_id, PCWSTR(label_w.as_ptr()));
        let action = if let Some(ref path) = item.open_path {
            MenuAction::OpenPath(path.clone())
        } else if is_local {
            MenuAction::LocalItem(item.id.clone())
        } else {
            MenuAction::RemoteItem {
                peer: module.module.clone(),
                id: item.id.clone(),
            }
        };
        actions.push(action);
    }
}

unsafe fn show_popup_menu(state: &ThreadOwnedState, hwnd: HWND) {
    let snap = build_menu_snapshot(&state.shared);
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let mut actions: Vec<MenuAction> = Vec::new();

    // Top: Settings (Control Center)
    let settings_tab = if snap.standalone_mode {
        Some(state.shared.cfg_module.clone())
    } else {
        None
    };
    let settings_id = 100 + actions.len();
    let settings_w = to_wide_null(&snap.settings_label);
    let _ = AppendMenuW(menu, MF_STRING, settings_id, PCWSTR(settings_w.as_ptr()));
    actions.push(MenuAction::OpenSettings { tab: settings_tab });
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    if snap.standalone_mode {
        if let Some(m) = snap.modules.first() {
            let is_local = m.module == state.shared.cfg_module;
            append_module_items(menu, m, is_local, &mut actions);
            if !m.items.is_empty() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            let quit_label = if m.module == "wm" {
                "Quit (restore windows)"
            } else {
                "Quit"
            };
            let quit_id = 100 + actions.len();
            let quit_w = to_wide_null(quit_label);
            let _ = AppendMenuW(menu, MF_STRING, quit_id, PCWSTR(quit_w.as_ptr()));
            actions.push(if is_local {
                MenuAction::LocalQuit
            } else {
                MenuAction::RemoteQuit(m.module.clone())
            });
        }
    } else {
        for m in &snap.modules {
            let Ok(sub) = CreatePopupMenu() else {
                continue;
            };
            let is_local = m.module == state.shared.cfg_module;
            append_module_items(sub, m, is_local, &mut actions);
            if !m.items.is_empty() {
                let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
            }
            let quit_label = if m.module == "wm" {
                "Quit Window Manager (restore windows)".to_string()
            } else {
                format!("Quit {}", m.title)
            };
            let quit_id = 100 + actions.len();
            let quit_w = to_wide_null(&quit_label);
            let _ = AppendMenuW(sub, MF_STRING, quit_id, PCWSTR(quit_w.as_ptr()));
            actions.push(if is_local {
                MenuAction::LocalQuit
            } else {
                MenuAction::RemoteQuit(m.module.clone())
            });

            let sub_title = if m.paused {
                format!("{} (paused)", m.title)
            } else {
                m.title.clone()
            };
            let sub_w = to_wide_null(&sub_title);
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(sub_w.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if let Some(ref qa_label) = snap.quit_all_label {
            let qa_id = 100 + actions.len();
            let qa_w = to_wide_null(qa_label);
            let _ = AppendMenuW(menu, MF_STRING, qa_id, PCWSTR(qa_w.as_ptr()));
            actions.push(MenuAction::QuitAll);
        }
    }

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    let chosen = cmd.0 as usize;
    if chosen >= 100 {
        if let Some(action) = actions.get(chosen - 100) {
            dispatch_menu_action(&state.shared, action);
        }
    }
}

fn dispatch_menu_action(shared: &Arc<TrayShared>, action: &MenuAction) {
    match action {
        MenuAction::OpenSettings { tab } => {
            if let Some(ref cb) = shared.on_open_settings {
                cb(tab.as_deref());
            } else {
                #[cfg(feature = "control-center")]
                {
                    let _ = crate::control_center::open_control_center(tab.as_deref());
                }
            }
        }
        MenuAction::OpenPath(path) => {
            let path = path.clone();
            thread::spawn(move || unsafe {
                let wide = to_wide_null(&path);
                let _ = ShellExecuteW(
                    None,
                    w!("open"),
                    PCWSTR(wide.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
            });
        }
        MenuAction::LocalItem(id) => {
            (shared.on_local_action)(id);
        }
        MenuAction::RemoteItem { peer, id } => {
            if let Some(ref node) = shared.node {
                let node = Arc::clone(node);
                let peer = peer.clone();
                let id = id.clone();
                thread::spawn(move || {
                    let _ = node.send_cmd(
                        &peer,
                        CAP_TRAY_ACTIONS,
                        "tray.invoke",
                        json!({ "id": id }),
                        Duration::from_millis(1000),
                    );
                });
            }
        }
        MenuAction::LocalQuit => {
            (shared.on_local_quit)();
        }
        MenuAction::RemoteQuit(peer) => {
            if let Some(ref node) = shared.node {
                let node = Arc::clone(node);
                let peer = peer.clone();
                thread::spawn(move || {
                    let _ = node.send_cmd(
                        &peer,
                        CAP_TRAY_ACTIONS,
                        "tray.quit",
                        Value::Null,
                        Duration::from_millis(800),
                    );
                });
            }
        }
        MenuAction::QuitAll => {
            let shared = Arc::clone(shared);
            thread::spawn(move || {
                if let Some(ref node) = shared.node {
                    let remote_peers: Vec<String> = node
                        .peers()
                        .into_iter()
                        .filter(|p| p.provides.iter().any(|c| c == CAP_TRAY_ACTIONS))
                        .map(|p| p.module)
                        .collect();
                    for peer in remote_peers {
                        let _ = node.send_cmd(
                            &peer,
                            CAP_TRAY_ACTIONS,
                            "tray.quit",
                            Value::Null,
                            Duration::from_millis(500),
                        );
                    }
                }
                (shared.on_local_quit)();
            });
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ThreadOwnedState;
    if !ptr.is_null() {
        let state = &mut *ptr;
        if msg == WM_TRAY_ICON {
            let ev = lparam.0 as u32;
            if ev == WM_LBUTTONUP || ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU_EVENT {
                show_popup_menu(state, hwnd);
            }
            return LRESULT(0);
        }
        if msg == WM_TRAY_REEVALUATE {
            reevaluate_election(state, hwnd, false);
            return LRESULT(0);
        }
        if msg == WM_TRAY_YIELD {
            reevaluate_election(state, hwnd, true);
            return LRESULT(0);
        }
        if msg == WM_TRAY_REFRESH {
            add_or_update_tray_icon(state, hwnd);
            return LRESULT(0);
        }
        if msg == WM_TRAY_SHUTDOWN {
            remove_tray_icon(state, hwnd);
            if state.owns_mutex {
                let _ = ReleaseMutex(state.mutex_handle);
                state.owns_mutex = false;
            }
            state.shared.is_host.store(false, Ordering::Release);
            let _ = KillTimer(Some(hwnd), TIMER_ELECTION);
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        if msg == WM_TIMER && wparam.0 == TIMER_ELECTION {
            reevaluate_election(state, hwnd, false);
            return LRESULT(0);
        }
        if state.taskbar_created_msg != 0 && msg == state.taskbar_created_msg {
            state.icon_added = false;
            add_or_update_tray_icon(state, hwnd);
            return LRESULT(0);
        }
        if msg == WM_SETTINGCHANGE {
            refresh_tray_icon_theme(state, hwnd);
            return LRESULT(0);
        }
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NipEvent, NodeConfig};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct IsolatedEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    impl IsolatedEnvGuard {
        fn new(tag: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let prefix = format!("noren_tray_{tag}_{}_{nanos}.", std::process::id());
            std::env::set_var("NOREN_PIPE_PREFIX", &prefix);
            std::env::set_var("NOREN_TRAY_HEADLESS", "1");
            std::env::remove_var("NOREN_SIMULATE_ELEVATED");
            std::env::remove_var("NOREN_TRAY_MUTEX_NAME");
            Self { _lock: lock }
        }
    }

    impl Drop for IsolatedEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("NOREN_SIMULATE_ELEVATED");
            std::env::remove_var("NOREN_TRAY_HEADLESS");
            std::env::remove_var("NOREN_TRAY_MUTEX_NAME");
            std::env::remove_var("NOREN_PIPE_PREFIX");
        }
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        cond()
    }

    fn sample_payload(module: &str, title: &str) -> TrayModulePayload {
        TrayModulePayload {
            module: module.to_string(),
            title: title.to_string(),
            paused: false,
            items: vec![TrayMenuItem {
                id: format!("{module}_act"),
                label: format!("Action for {title}"),
                checked: false,
                enabled: true,
                separator_before: false,
                open_path: None,
            }],
        }
    }

    #[test]
    fn standalone_election_and_second_peer_failover() {
        let _env = IsolatedEnvGuard::new("election");

        let (node_a, rx_a) = NipNode::start(NodeConfig {
            module: "launcher".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let node_a = Arc::new(node_a);

        let tray_a = TrayService::start(TrayConfig {
            module: "launcher".to_string(),
            tooltip: "noren-launcher".to_string(),
            initial_payload: sample_payload("launcher", "Launcher"),
            node: Some(Arc::clone(&node_a)),
            on_local_action: Arc::new(|_| {}),
            on_local_quit: Arc::new(|| {}),
            on_open_settings: None,
            headless: true,
        })
        .unwrap();

        assert!(tray_a.is_host());
        let snap1 = tray_a.menu_snapshot();
        assert!(snap1.standalone_mode);
        assert_eq!(snap1.quit_all_label, None);
        assert_eq!(snap1.modules.len(), 1);

        let (node_b, rx_b) = NipNode::start(NodeConfig {
            module: "notch".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let node_b = Arc::new(node_b);

        let tray_b = TrayService::start(TrayConfig {
            module: "notch".to_string(),
            tooltip: "noren-notch".to_string(),
            initial_payload: sample_payload("notch", "Notch"),
            node: Some(Arc::clone(&node_b)),
            on_local_action: Arc::new(|_| {}),
            on_local_quit: Arc::new(|| {}),
            on_open_settings: None,
            headless: true,
        })
        .unwrap();

        assert!(wait_until(Duration::from_secs(4), || {
            while let Ok(ev) = rx_a.try_recv() {
                if matches!(
                    ev,
                    NipEvent::PeerConnected(_)
                        | NipEvent::PeerDisconnected { .. }
                        | NipEvent::StateReceived { .. }
                ) {
                    tray_a.notify_topology_changed();
                }
            }
            while let Ok(ev) = rx_b.try_recv() {
                if matches!(
                    ev,
                    NipEvent::PeerConnected(_)
                        | NipEvent::PeerDisconnected { .. }
                        | NipEvent::StateReceived { .. }
                ) {
                    tray_b.notify_topology_changed();
                }
            }
            let s = tray_a.menu_snapshot();
            !s.standalone_mode && s.modules.len() == 2
        }));

        assert!(tray_a.is_host());
        assert!(!tray_b.is_host());
        let snap2 = tray_a.menu_snapshot();
        assert_eq!(snap2.quit_all_label.as_deref(), Some("Quit All"));
        assert_eq!(snap2.modules[0].module, "launcher");
        assert_eq!(snap2.modules[1].module, "notch");

        // Host exits -> second peer automatically acquires the mutex and becomes Tray Host.
        tray_a.shutdown();
        node_a.shutdown();
        assert!(wait_until(Duration::from_secs(4), || {
            tray_b.notify_topology_changed();
            tray_b.is_host()
        }));

        tray_b.shutdown();
        node_b.shutdown();
    }

    #[test]
    fn privilege_split_yields_from_elevated_to_standard_and_recovers() {
        let _env = IsolatedEnvGuard::new("privsplit");

        std::env::set_var("NOREN_SIMULATE_ELEVATED", "1");
        let (wm_node, wm_rx) = NipNode::start(NodeConfig {
            module: "wm".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let wm_node = Arc::new(wm_node);
        assert!(wm_node.integrity_info().elevated);
        std::env::remove_var("NOREN_SIMULATE_ELEVATED");

        let wm_tray = TrayService::start(TrayConfig {
            module: "wm".to_string(),
            tooltip: "noren-wm".to_string(),
            initial_payload: sample_payload("wm", "Window Manager"),
            node: Some(Arc::clone(&wm_node)),
            on_local_action: Arc::new(|_| {}),
            on_local_quit: Arc::new(|| {}),
            on_open_settings: None,
            headless: true,
        })
        .unwrap();
        assert!(wm_tray.is_host());

        // Standard-level launcher starts and connects.
        let (launcher_node, launcher_rx) = NipNode::start(NodeConfig {
            module: "launcher".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let launcher_node = Arc::new(launcher_node);
        assert!(!launcher_node.integrity_info().elevated);

        let launcher_tray = TrayService::start(TrayConfig {
            module: "launcher".to_string(),
            tooltip: "noren-launcher".to_string(),
            initial_payload: sample_payload("launcher", "Launcher"),
            node: Some(Arc::clone(&launcher_node)),
            on_local_action: Arc::new(|_| {}),
            on_local_quit: Arc::new(|| {}),
            on_open_settings: None,
            headless: true,
        })
        .unwrap();

        let handed_off = wait_until(Duration::from_secs(5), || {
            while let Ok(ev) = wm_rx.try_recv() {
                match ev {
                    NipEvent::PeerConnected(_) | NipEvent::PeerDisconnected { .. } => {
                        wm_tray.notify_topology_changed();
                    }
                    NipEvent::Command {
                        from, name, reply, ..
                    } if name == "tray.yield" => {
                        let elev = wm_node
                            .peers()
                            .into_iter()
                            .find(|p| p.module == from)
                            .is_some_and(|p| p.elevated);
                        let yielded = wm_tray.request_yield(elev);
                        let _ = reply.ok(json!({ "yielded": yielded }));
                    }
                    _ => {}
                }
            }
            while let Ok(ev) = launcher_rx.try_recv() {
                if matches!(
                    ev,
                    NipEvent::PeerConnected(_)
                        | NipEvent::EventReceived { .. }
                        | NipEvent::StateReceived { .. }
                ) {
                    launcher_tray.notify_topology_changed();
                }
            }
            !wm_tray.is_host() && launcher_tray.is_host()
        });
        assert!(handed_off, "elevated wm did not yield tray host to standard launcher");

        // Launcher exits -> elevated wm automatically takes over tray host again.
        launcher_tray.shutdown();
        launcher_node.shutdown();

        let recovered = wait_until(Duration::from_secs(4), || {
            while let Ok(ev) = wm_rx.try_recv() {
                if matches!(ev, NipEvent::PeerDisconnected { .. }) {
                    wm_tray.notify_topology_changed();
                }
            }
            wm_tray.is_host()
        });
        assert!(recovered, "elevated wm did not recover tray host after standard peer exited");

        wm_tray.shutdown();
        wm_node.shutdown();
    }

    #[test]
    fn remote_invoke_and_quit_all_dispatch() {
        let _env = IsolatedEnvGuard::new("dispatch");
        let invoked = Arc::new(Mutex::new(Vec::<String>::new()));
        let remote_quits = Arc::new(AtomicUsize::new(0));
        let local_quits = Arc::new(AtomicUsize::new(0));

        let (node_a, _rx_a) = NipNode::start(NodeConfig {
            module: "launcher".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let node_a = Arc::new(node_a);

        let local_quits_cb = Arc::clone(&local_quits);
        let tray_a = TrayService::start(TrayConfig {
            module: "launcher".to_string(),
            tooltip: "noren-launcher".to_string(),
            initial_payload: sample_payload("launcher", "Launcher"),
            node: Some(Arc::clone(&node_a)),
            on_local_action: Arc::new(|_| {}),
            on_local_quit: Arc::new(move || {
                local_quits_cb.fetch_add(1, Ordering::SeqCst);
            }),
            on_open_settings: None,
            headless: true,
        })
        .unwrap();

        let (node_b, rx_b) = NipNode::start(NodeConfig {
            module: "wm".to_string(),
            version: "0.1.0".to_string(),
            provides: vec![CAP_TRAY_ACTIONS.to_string()],
            wants: vec![CAP_TRAY_ACTIONS.to_string()],
            allowlist: None,
            initial_state: HashMap::new(),
        })
        .unwrap();
        let node_b = Arc::new(node_b);

        let stop_b = Arc::new(AtomicBool::new(false));
        let stop_b_bg = Arc::clone(&stop_b);
        let invoked_bg = Arc::clone(&invoked);
        let remote_quits_bg = Arc::clone(&remote_quits);
        let responder_b = thread::spawn(move || {
            while !stop_b_bg.load(Ordering::Acquire) {
                if let Ok(NipEvent::Command {
                    name, data, reply, ..
                }) = rx_b.recv_timeout(Duration::from_millis(30))
                {
                    match name.as_str() {
                        "tray.invoke" => {
                            let id = data
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            invoked_bg.lock().unwrap().push(id);
                            let _ = reply.ok(json!({ "ok": true }));
                        }
                        "tray.quit" => {
                            remote_quits_bg.fetch_add(1, Ordering::SeqCst);
                            let _ = reply.ok(json!({ "quitting": true }));
                        }
                        _ => {
                            let _ = reply.err("unsupported");
                        }
                    }
                }
            }
        });

        assert!(wait_until(Duration::from_secs(4), || {
            node_a.is_peer_connected("wm")
        }));

        dispatch_menu_action(
            &tray_a.shared,
            &MenuAction::RemoteItem {
                peer: "wm".to_string(),
                id: "pause".to_string(),
            },
        );
        assert!(wait_until(Duration::from_secs(2), || {
            invoked.lock().unwrap().as_slice() == ["pause"]
        }));

        dispatch_menu_action(&tray_a.shared, &MenuAction::QuitAll);
        assert!(wait_until(Duration::from_secs(2), || {
            remote_quits.load(Ordering::SeqCst) == 1 && local_quits.load(Ordering::SeqCst) == 1
        }));

        stop_b.store(true, Ordering::Release);
        let _ = responder_b.join();
        tray_a.shutdown();
        node_a.shutdown();
        node_b.shutdown();
    }
}
