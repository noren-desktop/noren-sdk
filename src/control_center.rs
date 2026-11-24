use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetWindowTextW, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::BOOL;
use windows_reactor::*;

use crate::error::SdkError;
use crate::node::{NipEvent, NipNode, NodeConfig};
use crate::paths::{module_config_path, pipe_prefix};
use crate::pipe::{OverlappedPipe, connect_client_pipe, discover_peers, EventHandle};
use crate::protocol::{CAP_CONTROLCENTER_FOCUS, CAP_SETTINGS_SCHEMA, Message, PROTOCOL_VERSION};
use crate::schema::{
    FieldDef, FieldKind, FieldValue, ModuleManifest, format_hex_color, parse_hex_color,
    validate_key_chord,
};
use crate::security::current_user_sid;
use crate::storage::{load_config_values, load_installed_manifests, save_config_values};
use crate::theme::{AccentMode, ThemeConfig, ThemeMaterial, ThemeMode, load_theme, save_theme};

const CONTROL_CENTER_MODULE: &str = "control-center";
const CONTROL_CENTER_TITLE: &str = "Noren Control Center";

/// Checks CLI args for `--control-center [--tab <id>]`.
/// Returns `Some(result)` if Control Center mode was requested (and executed), or `None` for normal module startup.
pub fn maybe_run_control_center_from_args() -> Option<Result<(), SdkError>> {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--control-center") {
        return None;
    }
    let initial_tab = args
        .windows(2)
        .find(|w| w[0] == "--tab")
        .map(|w| w[1].clone());
    Some(run_control_center(initial_tab))
}

/// Opens the Control Center window (focusing an existing instance if one is open,
/// or spawning the current executable with `--control-center`).
pub fn open_control_center(initial_tab: Option<&str>) -> Result<(), SdkError> {
    if let Ok(sid) = current_user_sid() {
        if let Ok(peers) = discover_peers("__cc_launcher__", &sid) {
            if let Some(cc) = peers.iter().find(|p| p.module == CONTROL_CENTER_MODULE) {
                if focus_existing_control_center(&cc.path, initial_tab).is_ok() {
                    return Ok(());
                }
            }
        }
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--control-center");
    if let Some(tab) = initial_tab {
        cmd.args(["--tab", tab]);
    }
    cmd.spawn()?;
    Ok(())
}

fn focus_existing_control_center(path: &str, tab: Option<&str>) -> Result<(), SdkError> {
    let handle = connect_client_pipe(path)?;
    let pipe = OverlappedPipe::new(handle)?;
    let stop = EventHandle::new_manual(false)?;

    let hello = Message::Hello {
        v: PROTOCOL_VERSION,
        module: format!("cc-focus-{}", std::process::id()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        proto: [PROTOCOL_VERSION, PROTOCOL_VERSION],
        pid: std::process::id(),
        elevated: false,
        integrity: String::new(),
        provides: Vec::new(),
        wants: Vec::new(),
    };
    pipe.write_all(&hello.to_ndjson_line()?)?;

    let mut buf = [0u8; 2048];
    let n = pipe.read_chunk(&mut buf, stop.0)?;
    if n == 0 {
        return Err(SdkError::Protocol("control-center closed before welcome".to_string()));
    }

    let cmd = Message::Cmd {
        v: PROTOCOL_VERSION,
        id: "focus-1".to_string(),
        cap: CAP_CONTROLCENTER_FOCUS.to_string(),
        name: "focus".to_string(),
        data: json!({ "tab": tab }),
    };
    pipe.write_all(&cmd.to_ndjson_line()?)?;
    let _ = pipe.read_chunk(&mut buf, stop.0);

    let bye = Message::Bye {
        v: PROTOCOL_VERSION,
        module: format!("cc-focus-{}", std::process::id()),
        reason: "focus_done".to_string(),
    };
    let _ = pipe.write_all(&bye.to_ndjson_line()?);
    Ok(())
}

fn focus_window_on_ui_thread() {
    struct Search {
        found: Option<HWND>,
    }
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = &mut *(lparam.0 as *mut Search);
        let mut buf = [0u16; 128];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            if title == CONTROL_CENTER_TITLE {
                state.found = Some(hwnd);
                return BOOL(0);
            }
        }
        TRUE
    }
    let mut search = Search { found: None };
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(enum_proc),
            LPARAM(&mut search as *mut _ as isize),
        );
        if let Some(hwnd) = search.found {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Launches and runs the Control Center WinUI 3 window until closed.
pub fn run_control_center(initial_tab: Option<String>) -> Result<(), SdkError> {
    let (node, rx) = NipNode::start(NodeConfig {
        module: CONTROL_CENTER_MODULE.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        provides: vec![CAP_CONTROLCENTER_FOCUS.to_string()],
        wants: vec![CAP_SETTINGS_SCHEMA.to_string()],
        allowlist: None,
        initial_state: HashMap::new(),
    })?;

    let node = Arc::new(node);
    let rx = Arc::new(Mutex::new(rx));

    let res = App::run_component::<ControlCenterApp>(ControlCenterInput {
        node: Arc::clone(&node),
        rx,
        initial_tab,
    })
    .map_err(|e| SdkError::Ui(format!("WinUI 3 Control Center failed: {e}")));

    node.shutdown();
    let _ = pipe_prefix();
    res
}

#[derive(Clone)]
struct ControlCenterInput {
    node: Arc<NipNode>,
    rx: Arc<Mutex<mpsc::Receiver<NipEvent>>>,
    initial_tab: Option<String>,
}

impl PartialEq for ControlCenterInput {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.node, &other.node)
            && Arc::ptr_eq(&self.rx, &other.rx)
            && self.initial_tab == other.initial_tab
    }
}

enum Msg {
    NipTick(Vec<NipEvent>),
    SelectTab(TabSelection),
    SetThemeMode(ThemeMode),
    SetThemeMaterial(ThemeMaterial),
    SetAccentMode(AccentMode),
    SetCustomAccentHex(String),
    ToggleThemeColorPicker,
    PickThemeColor(Color),
    SaveAndBroadcastTheme,
    SetBool(String, bool),
    SetNumber(String, f64),
    SetChoice(String, Option<usize>),
    SetText(String, String),
    ToggleFieldColorPicker(String),
    PickFieldColor(String, Color),
    SetFieldColorOff(String),
    InvokeAction(String, String),
    OpenAdvancedEditor(String),
    SaveModule,
    ReloadModule,
}

impl Clone for Msg {
    fn clone(&self) -> Self {
        match self {
            Self::NipTick(_) => Self::NipTick(Vec::new()),
            Self::SelectTab(t) => Self::SelectTab(t.clone()),
            Self::SetThemeMode(v) => Self::SetThemeMode(*v),
            Self::SetThemeMaterial(v) => Self::SetThemeMaterial(*v),
            Self::SetAccentMode(v) => Self::SetAccentMode(*v),
            Self::SetCustomAccentHex(v) => Self::SetCustomAccentHex(v.clone()),
            Self::ToggleThemeColorPicker => Self::ToggleThemeColorPicker,
            Self::PickThemeColor(v) => Self::PickThemeColor(*v),
            Self::SaveAndBroadcastTheme => Self::SaveAndBroadcastTheme,
            Self::SetBool(k, v) => Self::SetBool(k.clone(), *v),
            Self::SetNumber(k, v) => Self::SetNumber(k.clone(), *v),
            Self::SetChoice(k, v) => Self::SetChoice(k.clone(), *v),
            Self::SetText(k, v) => Self::SetText(k.clone(), v.clone()),
            Self::ToggleFieldColorPicker(k) => Self::ToggleFieldColorPicker(k.clone()),
            Self::PickFieldColor(k, c) => Self::PickFieldColor(k.clone(), *c),
            Self::SetFieldColorOff(k) => Self::SetFieldColorOff(k.clone()),
            Self::InvokeAction(l, c) => Self::InvokeAction(l.clone(), c.clone()),
            Self::OpenAdvancedEditor(c) => Self::OpenAdvancedEditor(c.clone()),
            Self::SaveModule => Self::SaveModule,
            Self::ReloadModule => Self::ReloadModule,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TabSelection {
    General,
    Module(String),
}

struct ModuleTab {
    manifest: ModuleManifest,
    config_path: PathBuf,
    running: bool,
    values: HashMap<String, FieldValue>,
}

struct ControlCenterApp {
    node: Arc<NipNode>,
    rx: Arc<Mutex<mpsc::Receiver<NipEvent>>>,
    theme: ThemeConfig,
    theme_picker_open: bool,
    tabs: Vec<ModuleTab>,
    active_tab: TabSelection,
    expanded_color_picker: Option<String>,
    status: String,
}

fn poll_nip_events(rx: &Arc<Mutex<mpsc::Receiver<NipEvent>>>, cancel: &CancellationToken) -> Msg {
    let mut batch = Vec::new();
    if !cancel.is_cancelled() {
        if let Ok(guard) = rx.lock() {
            if let Ok(first) = guard.recv_timeout(Duration::from_millis(120)) {
                batch.push(first);
                while let Ok(more) = guard.try_recv() {
                    batch.push(more);
                }
            }
        }
    }
    Msg::NipTick(batch)
}

impl ControlCenterApp {
    fn schedule_poll(&self, ctx: &ComponentContext<Self>) {
        let rx = Arc::clone(&self.rx);
        ctx.spawn_background(move |cancel| poll_nip_events(&rx, &cancel));
    }

    fn current_module_mut(&mut self) -> Option<&mut ModuleTab> {
        let TabSelection::Module(ref id) = self.active_tab else {
            return None;
        };
        self.tabs.iter_mut().find(|t| &t.manifest.id == id)
    }

    fn current_module(&self) -> Option<&ModuleTab> {
        let TabSelection::Module(ref id) = self.active_tab else {
            return None;
        };
        self.tabs.iter().find(|t| &t.manifest.id == id)
    }

    fn sync_peer_statuses(&mut self) {
        let connected: Vec<String> = self
            .node
            .peers()
            .into_iter()
            .map(|p| p.module)
            .collect();
        for tab in &mut self.tabs {
            tab.running = connected.iter().any(|m| m == &tab.manifest.id);
        }
    }

    fn refresh_manifests(&mut self) {
        let installed = load_installed_manifests();
        for manifest in installed {
            if self.tabs.iter().any(|t| t.manifest.id == manifest.id) {
                continue;
            }
            let config_path = module_config_path(&manifest.id, &manifest.config_file);
            let values = load_config_values(&config_path, &manifest).unwrap_or_default();
            self.tabs.push(ModuleTab {
                manifest,
                config_path,
                running: false,
                values,
            });
        }
        self.sync_peer_statuses();
    }

    fn render_general_tab(&self, ctx: &ViewContext<Self>) -> View {
        let resolved = self.theme.resolve();
        let accent_hex = self.theme.normalized_custom_accent();
        let (ar, ag, ab) = resolved.accent_rgb;

        let mode_row = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                mode_btn("Follow Windows", ThemeMode::System, self.theme.mode, ctx),
                mode_btn("Light", ThemeMode::Light, self.theme.mode, ctx),
                mode_btn("Dark", ThemeMode::Dark, self.theme.mode, ctx),
            ));

        let material_row = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                material_btn("Mica", ThemeMaterial::Mica, self.theme.material, ctx),
                material_btn("Acrylic", ThemeMaterial::Acrylic, self.theme.material, ctx),
            ));

        let accent_mode_row = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                accent_mode_btn(
                    "Follow Windows",
                    AccentMode::Windows,
                    self.theme.accent_mode,
                    ctx,
                ),
                accent_mode_btn("Custom Color", AccentMode::Custom, self.theme.accent_mode, ctx),
            ));

        let mut accent_controls: Vec<KeyedView> = vec![
            KeyedView::new("mode_row", accent_mode_row),
            KeyedView::new(
                "preview_row",
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(10.0)
                    .children((
                        Border::new()
                            .width(32.0)
                            .height(32.0)
                            .corner_radius(CornerRadius::uniform(6.0))
                            .background(Color::rgb(ar, ag, ab))
                            .border_brush(ThemeBrush::CardStroke)
                            .border_thickness(Thickness::uniform(1.0)),
                        TextBlock::new()
                            .text(format!(
                                "Effective Accent: #{ar:02X}{ag:02X}{ab:02X} ({})",
                                if resolved.is_dark { "Dark" } else { "Light" }
                            ))
                            .vertical_alignment(VerticalAlignment::Center),
                    )),
            ),
        ];

        if self.theme.accent_mode == AccentMode::Custom {
            accent_controls.push(KeyedView::new(
                "custom_hex",
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(8.0)
                    .children((
                        TextBox::new()
                            .width(140.0)
                            .text(accent_hex.clone())
                            .placeholder_text("#4FA8F5")
                            .on_text_changed(ctx.callback(Msg::SetCustomAccentHex)),
                        Button::new()
                            .on_click(ctx.message(Msg::ToggleThemeColorPicker))
                            .content(if self.theme_picker_open {
                                "Hide Picker"
                            } else {
                                "Pick Color"
                            }),
                    )),
            ));
            if self.theme_picker_open {
                let (cr, cg, cb) = parse_hex_color(&accent_hex).unwrap_or((ar, ag, ab));
                accent_controls.push(KeyedView::new(
                    "picker",
                    ColorPicker::new()
                        .color(Color::rgb(cr, cg, cb))
                        .is_alpha_enabled(false)
                        .is_hex_input_visible(true)
                        .on_color_changed(ctx.callback(Msg::PickThemeColor)),
                ));
            }
        }

        StackPanel::new()
            .spacing(12.0)
            .children((
                group_card(
                    "Mode",
                    "Applies to Control Center and broadcasts theme.changed to running modules.",
                    mode_row,
                ),
                group_card(
                    "Material",
                    "Mica or Acrylic backdrop (falls back to solid if Windows transparency is off).",
                    material_row,
                ),
                group_card(
                    "Colors & Accent",
                    "Follow Windows personalization or pick a custom accent color.",
                    StackPanel::new().spacing(8.0).keyed_children(accent_controls),
                ),
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(ctx.message(Msg::SaveAndBroadcastTheme))
                    .content("Save general.toml & Broadcast theme.changed"),
            ))
    }

    fn render_module_tab(&self, tab: &ModuleTab, ctx: &ViewContext<Self>) -> View {
        let toolbar = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(ctx.message(Msg::SaveModule))
                    .content(if tab.running {
                        "Apply & Save via NIP"
                    } else {
                        "Save Atomically (Module Off)"
                    }),
                Button::new()
                    .on_click(ctx.message(Msg::ReloadModule))
                    .content("Reload from Disk"),
            ));

        let mut items: Vec<KeyedView> = Vec::new();
        for group in &tab.manifest.groups {
            let fields: Vec<KeyedView> = group
                .fields
                .iter()
                .map(|field| {
                    KeyedView::new(
                        format!("{}-{}-{}", tab.manifest.id, group.id, field.key),
                        self.render_schema_field(tab, field, ctx),
                    )
                })
                .collect();

            items.push(KeyedView::new(
                format!("{}-group-{}", tab.manifest.id, group.id),
                StackPanel::new().spacing(8.0).children((
                    TextBlock::new()
                        .text(group.title.clone())
                        .font_size(16.0)
                        .font_weight(FontWeight::SEMI_BOLD),
                    StackPanel::new().spacing(8.0).keyed_children(fields),
                )),
            ));
        }

        if !tab.manifest.advanced_editors.is_empty() {
            let editors: Vec<KeyedView> = tab
                .manifest
                .advanced_editors
                .iter()
                .map(|ed| {
                    KeyedView::new(
                        format!("{}-adv-{}", tab.manifest.id, ed.id),
                        Button::new()
                            .on_click(ctx.message(Msg::OpenAdvancedEditor(ed.command.clone())))
                            .content(ed.label.clone()),
                    )
                })
                .collect();
            items.push(KeyedView::new(
                format!("{}-advanced", tab.manifest.id),
                StackPanel::new().spacing(8.0).children((
                    TextBlock::new()
                        .text("Advanced Editors")
                        .font_size(16.0)
                        .font_weight(FontWeight::SEMI_BOLD),
                    StackPanel::new().spacing(6.0).keyed_children(editors),
                )),
            ));
        }

        StackPanel::new().spacing(12.0).children((
            toolbar,
            TextBlock::new()
                .text(format!(
                    "v{}  |  Status: {}  |  Config: {}",
                    tab.manifest.version,
                    if tab.running { "Running" } else { "Off" },
                    tab.config_path.display()
                ))
                .font_size(12.0)
                .text_wrapping(TextWrapping::Wrap),
            StackPanel::new().spacing(14.0).keyed_children(items),
        ))
    }

    fn render_schema_field(
        &self,
        tab: &ModuleTab,
        field: &FieldDef,
        ctx: &ViewContext<Self>,
    ) -> View {
        let enabled = tab.running || !field.requires_running;
        let key = field.key.clone();

        let control: View = match field.kind {
            FieldKind::Boolean => {
                let cur = matches!(tab.values.get(&field.key), Some(FieldValue::Bool(true)));
                let k = key.clone();
                ToggleSwitch::new()
                    .is_on(cur)
                    .is_enabled(enabled)
                    .on_toggled(ctx.callback(move |v| Msg::SetBool(k.clone(), v)))
                    .into()
            }
            FieldKind::Number => {
                let min = field.min.unwrap_or(0.0);
                let max = field.max.unwrap_or(100.0);
                let step = field.step.unwrap_or(1.0);
                let cur = match tab.values.get(&field.key) {
                    Some(FieldValue::Number(n)) => *n,
                    _ => min,
                };
                let k = key.clone();
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(10.0)
                    .children((
                        Slider::new()
                            .width(220.0)
                            .minimum(min)
                            .maximum(max)
                            .step_frequency(step)
                            .value(cur)
                            .is_enabled(enabled)
                            .on_value_changed(ctx.callback(move |v| Msg::SetNumber(k.clone(), v))),
                        TextBlock::new()
                            .text(format!("{}", cur.round() as i64))
                            .vertical_alignment(VerticalAlignment::Center),
                    ))
            }
            FieldKind::Choice => {
                let cur_text = match tab.values.get(&field.key) {
                    Some(FieldValue::Text(s)) => s.as_str(),
                    _ => "",
                };
                let idx = field.choices.iter().position(|c| c == cur_text);
                let k = key.clone();
                ComboBox::new()
                    .width(180.0)
                    .items_source(field.choices.iter().cloned())
                    .selected_index(idx)
                    .is_enabled(enabled)
                    .on_selection_changed(ctx.callback(move |sel| Msg::SetChoice(k.clone(), sel)))
                    .into()
            }
            FieldKind::Color => {
                let cur_text = match tab.values.get(&field.key) {
                    Some(FieldValue::Text(s)) => s.clone(),
                    _ => "off".to_string(),
                };
                let parsed_rgb = parse_hex_color(&cur_text);
                let swatch_color = parsed_rgb
                    .map(|(r, g, b)| Color::rgb(r, g, b))
                    .unwrap_or_else(|| Color::argb(40, 128, 128, 128));
                let picker_open = self.expanded_color_picker.as_deref() == Some(field.key.as_str());

                let k_txt = key.clone();
                let k_pick = key.clone();
                let k_off = key.clone();
                let k_cp = key.clone();

                let mut row_items: Vec<KeyedView> = vec![
                    KeyedView::new(
                        "swatch",
                        Border::new()
                            .width(28.0)
                            .height(28.0)
                            .corner_radius(CornerRadius::uniform(6.0))
                            .background(swatch_color)
                            .border_brush(ThemeBrush::CardStroke)
                            .border_thickness(Thickness::uniform(1.0)),
                    ),
                    KeyedView::new(
                        "hex",
                        TextBox::new()
                            .width(120.0)
                            .text(cur_text)
                            .placeholder_text("#RRGGBB")
                            .is_enabled(enabled)
                            .on_text_changed(ctx.callback(move |t| Msg::SetText(k_txt.clone(), t))),
                    ),
                    KeyedView::new(
                        "toggle_picker",
                        Button::new()
                            .is_enabled(enabled)
                            .on_click(ctx.message(Msg::ToggleFieldColorPicker(k_pick)))
                            .content(if picker_open { "Hide Picker" } else { "Pick" }),
                    ),
                ];
                if field.nullable {
                    row_items.push(KeyedView::new(
                        "off_btn",
                        Button::new()
                            .is_enabled(enabled)
                            .on_click(ctx.message(Msg::SetFieldColorOff(k_off)))
                            .content("Off"),
                    ));
                }

                let row = StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(8.0)
                    .keyed_children(row_items);

                if picker_open {
                    let picker_color = parsed_rgb
                        .map(|(r, g, b)| Color::rgb(r, g, b))
                        .unwrap_or_else(|| Color::rgb(79, 168, 245));
                    StackPanel::new().spacing(8.0).children((
                        row,
                        ColorPicker::new()
                            .color(picker_color)
                            .is_alpha_enabled(false)
                            .is_hex_input_visible(true)
                            .is_enabled(enabled)
                            .on_color_changed(ctx.callback(move |c| Msg::PickFieldColor(k_cp.clone(), c))),
                    ))
                } else {
                    row
                }
            }
            FieldKind::Text => {
                let cur_text = match tab.values.get(&field.key) {
                    Some(FieldValue::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let k = key.clone();
                TextBox::new()
                    .width(220.0)
                    .text(cur_text)
                    .placeholder_text(field.placeholder.clone().unwrap_or_default())
                    .is_enabled(enabled)
                    .on_text_changed(ctx.callback(move |t| Msg::SetText(k.clone(), t)))
                    .into()
            }
            FieldKind::KeyChord => {
                let cur_text = match tab.values.get(&field.key) {
                    Some(FieldValue::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let valid = validate_key_chord(&cur_text);
                let k = key.clone();
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(8.0)
                    .children((
                        TextBox::new()
                            .width(180.0)
                            .text(cur_text)
                            .placeholder_text("win+tab")
                            .is_enabled(enabled)
                            .on_text_changed(ctx.callback(move |t| Msg::SetText(k.clone(), t))),
                        TextBlock::new()
                            .text(if valid { "Valid" } else { "Invalid" })
                            .vertical_alignment(VerticalAlignment::Center),
                    ))
            }
            FieldKind::Action => {
                let cmd = field.command.clone().unwrap_or_else(|| field.key.clone());
                let label = field.label.clone();
                Button::new()
                    .is_enabled(enabled)
                    .on_click(ctx.message(Msg::InvokeAction(label.clone(), cmd)))
                    .content(if enabled {
                        label
                    } else {
                        format!("{label} (off)")
                    })
                    .into()
            }
        };

        let mut header = vec![KeyedView::new(
            "lbl",
            TextBlock::new()
                .text(format!(
                    "{}{}",
                    field.label,
                    if field.requires_running {
                        " [live only]"
                    } else {
                        ""
                    }
                ))
                .font_weight(FontWeight::MEDIUM),
        )];
        if let Some(desc) = &field.description {
            header.push(KeyedView::new(
                "desc",
                TextBlock::new().text(desc.clone()).font_size(12.0),
            ));
        }

        Border::new()
            .corner_radius(CornerRadius::uniform(8.0))
            .padding(Thickness::xy(12.0, 10.0))
            .background(ThemeBrush::CardBackground)
            .border_brush(ThemeBrush::CardStroke)
            .border_thickness(Thickness::uniform(1.0))
            .content(
                StackPanel::new().spacing(6.0).children((
                    StackPanel::new().spacing(2.0).keyed_children(header),
                    control,
                )),
            )
            .into()
    }
}

impl Component for ControlCenterApp {
    type Input = ControlCenterInput;
    type Message = Msg;

    fn create(input: &ControlCenterInput, ctx: &ComponentContext<Self>) -> Self {
        let theme = load_theme();
        let manifests = load_installed_manifests();
        let mut tabs = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            let config_path = module_config_path(&manifest.id, &manifest.config_file);
            let values = load_config_values(&config_path, &manifest).unwrap_or_default();
            tabs.push(ModuleTab {
                manifest,
                config_path,
                running: false,
                values,
            });
        }

        let active_tab = input
            .initial_tab
            .as_ref()
            .filter(|id| tabs.iter().any(|t| &t.manifest.id == *id))
            .map(|id| TabSelection::Module(id.clone()))
            .unwrap_or(TabSelection::General);

        let mut app = Self {
            node: Arc::clone(&input.node),
            rx: Arc::clone(&input.rx),
            theme,
            theme_picker_open: false,
            tabs,
            active_tab,
            expanded_color_picker: None,
            status: "Control Center ready.".to_string(),
        };
        app.sync_peer_statuses();
        app.schedule_poll(ctx);
        app
    }

    fn update(&mut self, msg: Msg, ctx: &ComponentContext<Self>) {
        match msg {
            Msg::NipTick(events) => {
                for ev in events {
                    match ev {
                        NipEvent::PeerConnected(_) | NipEvent::PeerDisconnected { .. } => {
                            self.refresh_manifests();
                        }
                        NipEvent::ThemeChanged(theme) => {
                            self.theme = theme;
                            self.status = "Theme updated via theme.changed.".to_string();
                        }
                        NipEvent::Command {
                            cap,
                            name,
                            data,
                            reply,
                            ..
                        } if cap == CAP_CONTROLCENTER_FOCUS && name == "focus" => {
                            if let Some(tab_id) = data.get("tab").and_then(|v| v.as_str()) {
                                self.refresh_manifests();
                                if self.tabs.iter().any(|t| t.manifest.id == tab_id) {
                                    self.active_tab = TabSelection::Module(tab_id.to_string());
                                }
                            }
                            focus_window_on_ui_thread();
                            let _ = reply.ok(json!({ "focused": true }));
                        }
                        NipEvent::Command { reply, name, .. } => {
                            let _ = reply.err(format!("unsupported control-center command '{name}'"));
                        }
                        _ => {}
                    }
                }
                self.schedule_poll(ctx);
            }
            Msg::SelectTab(tab) => {
                self.refresh_manifests();
                self.active_tab = tab;
                self.expanded_color_picker = None;
            }
            Msg::SetThemeMode(mode) => self.theme.mode = mode,
            Msg::SetThemeMaterial(material) => self.theme.material = material,
            Msg::SetAccentMode(accent_mode) => self.theme.accent_mode = accent_mode,
            Msg::SetCustomAccentHex(hex) => self.theme.custom_accent = Some(hex),
            Msg::ToggleThemeColorPicker => self.theme_picker_open = !self.theme_picker_open,
            Msg::PickThemeColor(c) => {
                self.theme.custom_accent = Some(format_hex_color(c.r, c.g, c.b));
            }
            Msg::SaveAndBroadcastTheme => match save_theme(&self.theme) {
                Ok(()) => {
                    let _ = self.node.broadcast_theme_changed(&self.theme);
                    self.status = "Saved general.toml and broadcasted theme.changed.".to_string();
                }
                Err(e) => self.status = format!("Failed saving theme: {e}"),
            },
            Msg::SetBool(k, v) => {
                if let Some(tab) = self.current_module_mut() {
                    tab.values.insert(k, FieldValue::Bool(v));
                }
            }
            Msg::SetNumber(k, v) => {
                if let Some(tab) = self.current_module_mut() {
                    tab.values.insert(k, FieldValue::Number(v));
                }
            }
            Msg::SetChoice(k, sel) => {
                if let Some(idx) = sel {
                    if let Some(tab) = self.current_module_mut() {
                        let choice = tab
                            .manifest
                            .groups
                            .iter()
                            .flat_map(|g| g.fields.iter())
                            .find(|f| f.key == k)
                            .and_then(|f| f.choices.get(idx).cloned());
                        if let Some(val) = choice {
                            tab.values.insert(k, FieldValue::Text(val));
                        }
                    }
                }
            }
            Msg::SetText(k, text) => {
                if let Some(tab) = self.current_module_mut() {
                    tab.values.insert(k, FieldValue::Text(text));
                }
            }
            Msg::ToggleFieldColorPicker(k) => {
                if self.expanded_color_picker.as_deref() == Some(k.as_str()) {
                    self.expanded_color_picker = None;
                } else {
                    self.expanded_color_picker = Some(k);
                }
            }
            Msg::PickFieldColor(k, c) => {
                if let Some(tab) = self.current_module_mut() {
                    tab.values
                        .insert(k, FieldValue::Text(format_hex_color(c.r, c.g, c.b)));
                }
            }
            Msg::SetFieldColorOff(k) => {
                if self.expanded_color_picker.as_deref() == Some(k.as_str()) {
                    self.expanded_color_picker = None;
                }
                if let Some(tab) = self.current_module_mut() {
                    tab.values.insert(k, FieldValue::Text("off".to_string()));
                }
            }
            Msg::InvokeAction(label, cmd) => {
                if let Some(tab) = self.current_module() {
                    let target = tab.manifest.id.clone();
                    let res = self.node.send_cmd(
                        &target,
                        CAP_SETTINGS_SCHEMA,
                        &cmd,
                        Value::Null,
                        Duration::from_secs(3),
                    );
                    self.status = match res {
                        Ok(reply) => format!("Action '{label}' ({cmd}) -> {reply}"),
                        Err(e) => format!("Action '{label}' failed: {e}"),
                    };
                }
            }
            Msg::OpenAdvancedEditor(cmd) => {
                if let Some(tab) = self.current_module() {
                    if tab.running {
                        let res = self.node.send_cmd(
                            &tab.manifest.id,
                            CAP_SETTINGS_SCHEMA,
                            &cmd,
                            Value::Null,
                            Duration::from_secs(3),
                        );
                        self.status = match res {
                            Ok(reply) => format!("Advanced editor '{cmd}' -> {reply}"),
                            Err(e) => format!("Advanced editor '{cmd}' failed: {e}"),
                        };
                    } else {
                        self.status = format!(
                            "Module '{}' is off; config file is at {}",
                            tab.manifest.id,
                            tab.config_path.display()
                        );
                    }
                }
            }
            Msg::SaveModule => {
                if let Some(tab) = self.current_module() {
                    if tab.running {
                        let payload = serde_json::to_value(&tab.values).unwrap_or(Value::Null);
                        let res = self.node.send_cmd(
                            &tab.manifest.id,
                            CAP_SETTINGS_SCHEMA,
                            "config.apply",
                            payload,
                            Duration::from_secs(3),
                        );
                        self.status = match res {
                            Ok(_) => format!("Applied & saved '{}' via NIP.", tab.manifest.id),
                            Err(e) => format!("NIP save failed: {e}"),
                        };
                    } else {
                        self.status = match save_config_values(&tab.config_path, &tab.manifest, &tab.values) {
                            Ok(()) => format!(
                                "Saved stopped module '{}' atomically -> {}",
                                tab.manifest.id,
                                tab.config_path.display()
                            ),
                            Err(e) => format!("Atomic save failed: {e}"),
                        };
                    }
                }
            }
            Msg::ReloadModule => {
                if let Some(tab) = self.current_module_mut() {
                    self.status = match load_config_values(&tab.config_path, &tab.manifest) {
                        Ok(vals) => {
                            tab.values = vals;
                            format!("Reloaded '{}'.", tab.manifest.id)
                        }
                        Err(e) => format!("Reload failed: {e}"),
                    };
                }
            }
        }
    }

    fn view(&self, _input: &ControlCenterInput, ctx: &mut ViewContext<Self>) -> View {
        let backdrop = match self.theme.material {
            ThemeMaterial::Mica => WindowBackdrop::Mica,
            ThemeMaterial::Acrylic => WindowBackdrop::Acrylic,
        };
        let win_theme = match self.theme.mode {
            ThemeMode::System => WindowTheme::System,
            ThemeMode::Light => WindowTheme::Light,
            ThemeMode::Dark => WindowTheme::Dark,
        };
        ctx.window_title(CONTROL_CENTER_TITLE);
        ctx.window_visuals(
            WindowVisuals::new()
                .backdrop(backdrop)
                .theme(win_theme)
                .client_size(760.0, 720.0),
        );

        let mut tab_buttons = vec![KeyedView::new("tab-general", {
            let mut btn = Button::new().on_click(ctx.message(Msg::SelectTab(TabSelection::General)));
            if self.active_tab == TabSelection::General {
                btn = btn.style(ButtonStyle::Accent);
            }
            btn.content("General")
        })];

        for tab in &self.tabs {
            let id = tab.manifest.id.clone();
            let label = if tab.running {
                tab.manifest.name.clone()
            } else {
                format!("{} (off)", tab.manifest.name)
            };
            let is_active = self.active_tab == TabSelection::Module(id.clone());
            let mut btn =
                Button::new().on_click(ctx.message(Msg::SelectTab(TabSelection::Module(id.clone()))));
            if is_active {
                btn = btn.style(ButtonStyle::Accent);
            }
            tab_buttons.push(KeyedView::new(format!("tab-{id}"), btn.content(label)));
        }

        let body = match &self.active_tab {
            TabSelection::General => self.render_general_tab(ctx),
            TabSelection::Module(_) => {
                if let Some(tab) = self.current_module() {
                    self.render_module_tab(tab, ctx)
                } else {
                    TextBlock::new().text("Module not found.").into()
                }
            }
        };

        ScrollViewer::new()
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .content(
                StackPanel::new()
                    .margin(Thickness::uniform(16.0))
                    .spacing(12.0)
                    .children((
                        StackPanel::new()
                            .orientation(Orientation::Horizontal)
                            .spacing(8.0)
                            .keyed_children(tab_buttons),
                        body,
                        Border::new()
                            .corner_radius(CornerRadius::uniform(8.0))
                            .padding(Thickness::uniform(10.0))
                            .background(ThemeBrush::SolidBackground)
                            .border_brush(ThemeBrush::CardStroke)
                            .border_thickness(Thickness::uniform(1.0))
                            .content(
                                TextBlock::new()
                                    .text(format!("Status: {}", self.status))
                                    .text_wrapping(TextWrapping::Wrap),
                            ),
                    )),
            )
    }
}

fn group_card(title: &str, subtitle: &str, inner: View) -> View {
    Border::new()
        .corner_radius(CornerRadius::uniform(10.0))
        .padding(Thickness::uniform(12.0))
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .content(
            StackPanel::new().spacing(8.0).children((
                TextBlock::new()
                    .text(title)
                    .font_size(15.0)
                    .font_weight(FontWeight::SEMI_BOLD),
                TextBlock::new()
                    .text(subtitle)
                    .font_size(12.0)
                    .text_wrapping(TextWrapping::Wrap),
                inner,
            )),
        )
        .into()
}

fn mode_btn(
    label: &str,
    mode: ThemeMode,
    current: ThemeMode,
    ctx: &ViewContext<ControlCenterApp>,
) -> View {
    let mut b = Button::new().on_click(ctx.message(Msg::SetThemeMode(mode)));
    if mode == current {
        b = b.style(ButtonStyle::Accent);
    }
    b.content(label)
}

fn material_btn(
    label: &str,
    mat: ThemeMaterial,
    current: ThemeMaterial,
    ctx: &ViewContext<ControlCenterApp>,
) -> View {
    let mut b = Button::new().on_click(ctx.message(Msg::SetThemeMaterial(mat)));
    if mat == current {
        b = b.style(ButtonStyle::Accent);
    }
    b.content(label)
}

fn accent_mode_btn(
    label: &str,
    mode: AccentMode,
    current: AccentMode,
    ctx: &ViewContext<ControlCenterApp>,
) -> View {
    let mut b = Button::new().on_click(ctx.message(Msg::SetAccentMode(mode)));
    if mode == current {
        b = b.style(ButtonStyle::Accent);
    }
    b.content(label)
}
