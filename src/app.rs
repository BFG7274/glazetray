//! Application controller: owns the hidden message window, tray icon, flyout,
//! IPC handle and all UI-side state (pending actions, animations, theme).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Instant, SystemTime};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    KillTimer, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW, PostQuitMessage,
    RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEWHEEL,
    WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_THEMECHANGED,
    WM_TIMER, WNDCLASS_STYLES, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

use crate::config::{self, Config};
use crate::flyout::{Flyout, FlyoutAction, LayoutInput, WM_APP_FLYOUT_ACTION, compute_layout};
use crate::fonts::Fonts;
use crate::icon::{TrayConnection, TrayIconKey, create_hicon, render_tray_icon, tray_label};
use crate::ipc::{self, IpcHandle, UiToIpc};
use crate::reducer::can_encode_workspace_name;
use crate::state::{
    AppSnapshot, ConnectionState, MonitorId, PendingAction, TilingDirection, UiChangeKind,
    WorkspaceId,
};
use crate::theme::{Palette, ThemeInput, compute_palette};
use crate::tray::{self, Tray};
use crate::win32;

// ---------------------------------------------------------------------------
// Custom messages
// ---------------------------------------------------------------------------

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_IPC: u32 = WM_APP + 2;
const WM_APP_ACTIVATE: u32 = WM_APP + 3;

/// Registered message broadcast by Explorer when the taskbar is recreated.
static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

fn taskbar_created_msg() -> u32 {
    *TASKBAR_CREATED.get_or_init(|| {
        let name = win32::wide("TaskbarCreated");
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(win32::pcwstr(&name))
        }
    })
}

const TIMER_UI: usize = 1;
/// Slow tick used when idle (config polling, deadline checks).
const TIMER_SLOW_MS: u32 = 250;

/// A focus change is considered mouse-driven if a mouse button went down
/// within this window (covers click → GlazeWM event → UI processing latency).
const MOUSE_DRIVEN_CHANGE_MS: u64 = 250;
/// Fast tick used while the flyout is visible or feedback is animating.
const TIMER_FAST_MS: u32 = 16;

const HIDDEN_CLASS: &str = "GlazeTray.HiddenWindow";

static APP_PTR: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

// ---------------------------------------------------------------------------
// Pending action runtime
// ---------------------------------------------------------------------------

struct PendingRuntime {
    action: PendingAction,
    cmd_id: u64,
    since: Instant,
    calibrated: bool,
    /// For FocusThenToggle: 0 = focusing, 1 = toggling.
    phase: u8,
    baseline: Option<TilingDirection>,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    hinst: HINSTANCE,
    hwnd: HWND,
    tray: Tray,
    flyout: Option<Flyout>,
    ipc: IpcHandle,
    config: Config,
    config_error: Option<String>,
    config_mtime: Option<SystemTime>,
    snapshot: Option<Arc<AppSnapshot>>,
    monitor_names: HashMap<MonitorId, String>,
    icon_cache: HashMap<TrayIconKey, windows::Win32::UI::WindowsAndMessaging::HICON>,
    last_icon_key: Option<TrayIconKey>,
    last_tooltip: Option<String>,
    pending: Option<PendingRuntime>,
    confirm: Option<(WorkspaceId, Instant)>,
    error_ws: Option<(WorkspaceId, Instant)>,
    error_monitor: Option<(MonitorId, Instant)>,
    epoch: Instant,
    fonts: std::sync::Arc<Fonts>,
    palette: Palette,
    dark: bool,
    high_contrast: bool,
    anims: bool,
    cmd_seq: u64,
    closing: bool,
    fast_timer: bool,
    last_config_check: Instant,
    flyout_viewport_h: f32,
    flyout_auto_hide: Option<Instant>,
    transient_change: Option<(UiChangeKind, Instant)>,
}

unsafe extern "system" fn app_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = APP_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        let app = unsafe { &mut *ptr };
        if app.hwnd == hwnd {
            return app.wndproc(msg, wparam, lparam);
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl App {
    pub fn new(config: Config) -> App {
        let hinst = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None).expect("GetModuleHandleW")
        };
        let hinst = HINSTANCE(hinst.0);

        // Register classes.
        let class_wide: Vec<u16> = win32::wide(HIDDEN_CLASS);
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(app_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
            hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR::default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR::from_raw(class_wide.as_ptr()),
        };
        unsafe {
            RegisterClassW(&wc);
        }
        Flyout::register_class(hinst);

        // Hidden message window (invisible, findable for single-instance).
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR::from_raw(class_wide.as_ptr()),
                PCWSTR::null(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .expect("create hidden window");

        let dark = !win32::apps_use_light_theme();
        let high_contrast = win32::high_contrast_enabled();
        let palette = compute_palette(ThemeInput {
            dark,
            accent: win32::system_accent_color(),
            use_system_accent: config.tray.use_system_accent,
            high_contrast,
        });
        let anims = config::animation_enabled(&config, win32::animations_enabled());

        let mut tray = Tray::new(hwnd, WM_APP_TRAY);
        if !tray.add() {
            tracing::warn!("failed to add tray icon (explorer may be restarting)");
        }

        // Sync the configured startup preference into the Run registry key
        // (only enables; never overrides a manual user choice to disable).
        if config.startup.launch_with_windows && !crate::startup::startup_enabled() {
            match crate::startup::set_startup(true) {
                Ok(()) => tracing::info!("startup entry enabled per config"),
                Err(e) => tracing::warn!(error = %e, "failed to enable startup entry"),
            }
        }

        let notify = ipc::IpcNotify::new(hwnd.0 as isize, WM_APP_IPC);
        // Permanent low-level mouse hook: tracks whether a focus change was
        // initiated by the mouse (clicking a window) or by the keyboard, so
        // the transient status popup only appears for keyboard shortcuts.
        crate::flyout::install_permanent_mouse_hook();
        let ipc = ipc::spawn(Arc::new(config.clone()), notify);

        unsafe {
            SetTimer(Some(hwnd), TIMER_UI, TIMER_SLOW_MS, None);
        }

        App {
            hinst,
            hwnd,
            tray,
            flyout: None,
            ipc,
            config,
            config_error: None,
            config_mtime: None,
            snapshot: None,
            monitor_names: HashMap::new(),
            icon_cache: HashMap::new(),
            last_icon_key: None,
            last_tooltip: None,
            pending: None,
            confirm: None,
            error_ws: None,
            error_monitor: None,
            epoch: Instant::now(),
            fonts: std::sync::Arc::new(Fonts::load()),
            palette,
            dark,
            high_contrast,
            anims,
            cmd_seq: 0,
            closing: false,
            fast_timer: false,
            last_config_check: Instant::now(),
            flyout_viewport_h: 300.0,
            flyout_auto_hide: None,
            transient_change: None,
        }
    }

    pub fn run(&mut self) {
        let ptr = self as *mut App;
        APP_PTR.store(ptr, Ordering::SeqCst);
        tracing::info!("GlazeTray {} started", env!("CARGO_PKG_VERSION"));
        unsafe {
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            loop {
                let res = GetMessageW(&mut msg, None, 0, 0);
                if res.0 == 0 {
                    break; // WM_QUIT
                }
                if res.0 == -1 {
                    tracing::error!("GetMessageW failed");
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
        tracing::info!("message loop exited");
    }

    // ------------------------------------------------------------------
    // Message dispatch
    // ------------------------------------------------------------------

    fn wndproc(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_APP_TRAY => self.on_tray_message(wparam, lparam),
            WM_APP_IPC => {
                self.drain_ipc();
                LRESULT(0)
            }
            WM_APP_ACTIVATE => {
                tracing::info!("activated by second instance");
                self.show_flyout();
                LRESULT(0)
            }
            WM_APP_FLYOUT_ACTION => {
                let action = unsafe { Box::from_raw(wparam.0 as *mut FlyoutAction) };
                self.on_flyout_action(*action);
                LRESULT(0)
            }
            WM_TIMER => {
                self.on_tick();
                LRESULT(0)
            }
            WM_SETTINGCHANGE | WM_THEMECHANGED => {
                self.refresh_theme();
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                tracing::info!("display topology changed");
                self.ipc.tx.try_send(UiToIpc::Calibrate).ok();
                self.refresh_tray_dpi();
                self.close_flyout(true);
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                let pbt = wparam.0 as u32;
                if pbt == 0x0012 {
                    // PBT_APMRESUMEAUTOMATIC
                    tracing::info!("system resumed; reconnecting");
                    self.ipc.tx.try_send(UiToIpc::Reconnect).ok();
                }
                LRESULT(1)
            }
            msg if msg == taskbar_created_msg() => {
                tracing::info!("explorer taskbar recreated; re-adding tray icon");
                self.tray.readd();
                // The flyout (if visible) is a child of our process; Explorer
                // restart does not affect it, but the hooks reference the old
                // tray rect — refresh on next open anyway.
                LRESULT(0)
            }
            WM_QUERYENDSESSION => {
                tracing::info!("query end session");
                self.shutdown();
                LRESULT(1)
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    self.shutdown();
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(self.hwnd, msg, wparam, lparam) },
        }
    }

    // ------------------------------------------------------------------
    // Tray events
    // ------------------------------------------------------------------

    fn on_tray_message(&mut self, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // NOTIFYICON_VERSION_4 callback layout (see decode_tray_notification):
        // LOWORD(lParam) = event, HIWORD(lParam) = icon id, wParam = coords.
        let (event, icon_id, _x, _y, wheel_delta) = decode_tray_notification(wparam.0, lparam.0);
        if icon_id != 0 {
            // Not our icon (uID is 0); ignore.
            return LRESULT(0);
        }
        match event {
            WM_LBUTTONUP => {
                self.toggle_flyout();
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                self.on_context_menu();
            }
            WM_MBUTTONUP => {
                self.toggle_focused_direction();
            }
            WM_MOUSEWHEEL if self.config.tray.scroll_switch_workspace => {
                self.scroll_switch_workspace(if wheel_delta > 0 { -1 } else { 1 });
            }
            // TaskbarCreated is delivered to the window itself; tray events
            // arrive here only while the icon exists.
            _ => {}
        }
        LRESULT(0)
    }

    fn on_context_menu(&mut self) {
        let cursor = win32::cursor_pos().unwrap_or(POINT { x: 0, y: 0 });
        let startup_enabled = crate::startup::startup_enabled();
        let reconnect_enabled = !matches!(
            self.snapshot.as_ref().map(|s| s.connection.clone()),
            Some(ConnectionState::Ready)
        ) || true;
        let selected = tray::show_menu(self.hwnd, cursor, startup_enabled, reconnect_enabled);
        match selected {
            tray::MENU_OPEN => {
                self.show_flyout();
            }
            tray::MENU_RECONNECT => {
                self.ipc.tx.try_send(UiToIpc::Reconnect).ok();
            }
            tray::MENU_OPEN_CONFIG => {
                self.open_config_dir();
            }
            tray::MENU_STARTUP => {
                let enabled = !startup_enabled;
                match crate::startup::set_startup(enabled) {
                    Ok(()) => tracing::info!(enabled, "startup toggle"),
                    Err(e) => tracing::error!(error = %e, "failed to set startup"),
                }
            }
            tray::MENU_ABOUT => {
                self.show_about();
            }
            tray::MENU_EXIT => {
                self.shutdown();
                unsafe {
                    DestroyWindow(self.hwnd).ok();
                }
            }
            _ => {}
        }
    }

    fn open_config_dir(&mut self) {
        let dir = config::config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = config::config_path();
        if let Err(e) = config::ensure_default_config(&path) {
            tracing::error!(error = %e, "failed to ensure default config");
        }
        let op = win32::wide("open");
        let dir_w = win32::wide(&dir.to_string_lossy());
        unsafe {
            let ret = ShellExecuteW(
                None,
                win32::pcwstr(&op),
                win32::pcwstr(&dir_w),
                PCWSTR::null(),
                PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
            if (ret.0 as isize) <= 32 {
                tracing::warn!("ShellExecuteW failed with {}", ret.0 as isize);
            }
        }
    }

    fn show_about(&mut self) {
        let version = env!("CARGO_PKG_VERSION");
        let gv = self
            .snapshot
            .as_ref()
            .and_then(|s| s.glazewm_version.clone())
            .unwrap_or_else(|| "未知".into());
        let cfg_path = config::config_path().to_string_lossy().to_string();
        let mut text = format!(
            "GlazeTray {version}\n\nGlazeWM 的轻量系统托盘状态与控制工具\n\nGlazeWM 版本: {gv}\n配置: {cfg_path}\n\n许可证: MIT\n字体: Roboto (Apache-2.0)"
        );
        if let Some(err) = &self.config_error {
            text.push_str(&format!("\n\n配置错误: {err}"));
        }
        let title = win32::wide("关于 GlazeTray");
        let body = win32::wide(&text);
        unsafe {
            let _ = MessageBoxW(
                Some(self.hwnd),
                win32::pcwstr(&body),
                win32::pcwstr(&title),
                MESSAGEBOX_STYLE(MB_OK.0 | MB_ICONINFORMATION.0),
            );
        }
    }

    // ------------------------------------------------------------------
    // IPC
    // ------------------------------------------------------------------

    fn drain_ipc(&mut self) {
        let mut st = match self.ipc.shared.lock() {
            Ok(st) => st,
            Err(e) => {
                tracing::warn!(error = %e, "ipc state lock poisoned");
                return;
            }
        };
        let latest = st.latest.take();
        let results: Vec<(u64, bool, Option<String>)> = st.results.drain(..).collect();
        drop(st);
        for (id, success, message) in results {
            self.on_command_result(id, success, message);
        }
        if let Some(s) = latest {
            self.on_snapshot(s);
        }
    }

    fn on_snapshot(&mut self, snap: Arc<AppSnapshot>) {
        let new_ui_change = snap.last_ui_change.as_ref().and_then(|change| {
            let old_serial = self
                .snapshot
                .as_ref()
                .and_then(|old| old.last_ui_change.as_ref())
                .map(|old| old.serial);
            (old_serial != Some(change.serial)).then(|| change.kind.clone())
        });
        let rev_changed = self
            .snapshot
            .as_ref()
            .map(|s| s.revision != snap.revision || s.connection != snap.connection)
            .unwrap_or(true);
        self.snapshot = Some(snap);
        if rev_changed {
            self.resolve_monitor_names();
            self.reconcile_pending();
            self.update_tray();
            self.render_flyout();
        }
        if matches!(
            self.snapshot.as_ref().map(|s| &s.connection),
            Some(ConnectionState::Ready)
        ) && let Some(change) = new_ui_change
        {
            // Only show the transient popup when the change was NOT initiated
            // by the mouse (clicking a window on another workspace, or another
            // window in the same workspace). Keyboard-shortcut switches have
            // no recent mouse button-down and show the popup.
            if Self::should_show_transient(crate::flyout::last_mouse_down_age_ms()) {
                self.show_transient_flyout(change);
            } else {
                tracing::debug!("workspace change was mouse-driven; suppressing transient popup");
            }
        }
    }

    fn on_command_result(&mut self, id: u64, success: bool, message: Option<String>) {
        if let Some(p) = &self.pending
            && p.cmd_id == id
        {
            if !success {
                tracing::warn!(id, message = ?message, "command failed");
                self.fail_pending();
            }
            return;
        }
        tracing::debug!(id, success, "command result for unknown request");
    }

    fn resolve_monitor_names(&mut self) {
        let Some(snap) = &self.snapshot else { return };
        for mon in &snap.monitors {
            if self.monitor_names.contains_key(&mon.id) {
                continue;
            }
            let name =
                win32::friendly_name_for_glazewm_monitor(mon.device_name.as_deref(), mon.rect);
            let name = name.unwrap_or_else(|| format!("显示器 {}", mon.order + 1));
            self.monitor_names.insert(mon.id.clone(), name);
        }
    }

    fn monitor_display_name(&self, mon: &crate::state::MonitorInfo) -> String {
        self.monitor_names
            .get(&mon.id)
            .cloned()
            .unwrap_or_else(|| mon.display_name.clone())
    }

    // ------------------------------------------------------------------
    // Pending action lifecycle
    // ------------------------------------------------------------------

    fn send_command(&mut self, text: String) -> u64 {
        self.cmd_seq += 1;
        self.ipc
            .tx
            .try_send(UiToIpc::Command {
                id: self.cmd_seq,
                text: text.clone(),
            })
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to queue command");
            });
        tracing::debug!(id = self.cmd_seq, text, "command queued");
        self.cmd_seq
    }

    fn start_pending(
        &mut self,
        action: PendingAction,
        cmd_id: u64,
        baseline: Option<TilingDirection>,
    ) {
        self.pending = Some(PendingRuntime {
            action,
            cmd_id,
            since: Instant::now(),
            calibrated: false,
            phase: 0,
            baseline,
        });
    }

    fn fail_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            let now = Instant::now();
            match &p.action {
                PendingAction::FocusWorkspace { workspace_id, .. }
                | PendingAction::FocusThenToggle { workspace_id, .. } => {
                    self.error_ws = Some((workspace_id.clone(), now));
                }
                PendingAction::ToggleDirection { monitor_id } => {
                    self.error_monitor = Some((monitor_id.clone(), now));
                }
                PendingAction::Reconnect => {}
            }
            tracing::warn!("pending action failed: {:?}", p.action);
        }
    }

    fn confirm_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            let now = Instant::now();
            match &p.action {
                PendingAction::FocusWorkspace { workspace_id, .. } => {
                    self.confirm = Some((workspace_id.clone(), now));
                    if self.config.flyout.close_on_workspace_switch {
                        self.close_flyout(false);
                    }
                }
                PendingAction::ToggleDirection { monitor_id }
                | PendingAction::FocusThenToggle { monitor_id, .. } => {
                    self.confirm = Some((format!("__dir_{monitor_id}"), now));
                    if let Some(f) = &mut self.flyout {
                        f.spin_direction(monitor_id.clone(), now);
                    }
                    if self.config.flyout.close_on_workspace_switch {
                        self.close_flyout(false);
                    }
                }
                PendingAction::Reconnect => {}
            }
            tracing::debug!("pending action confirmed: {:?}", p.action);
        }
    }

    fn reconcile_pending(&mut self) {
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        let mut cmd_to_send: Option<String> = None;
        let mut confirm = false;
        if let Some(p) = &mut self.pending {
            match &p.action {
                PendingAction::FocusWorkspace { workspace_id, .. } => {
                    if snap.focused_workspace_id.as_deref() == Some(workspace_id.as_str()) {
                        confirm = true;
                    }
                }
                PendingAction::ToggleDirection { monitor_id, .. } => {
                    let dir = snap
                        .monitors
                        .iter()
                        .find(|m| &m.id == monitor_id)
                        .and_then(|m| m.direction);
                    if dir.is_some() && dir != p.baseline {
                        confirm = true;
                    }
                }
                PendingAction::FocusThenToggle {
                    workspace_id,
                    monitor_id,
                    ..
                } => {
                    if p.phase == 0 {
                        if snap.focused_workspace_id.as_deref() == Some(workspace_id.as_str()) {
                            // Focus confirmed; now toggle direction on that monitor.
                            p.phase = 1;
                            p.since = Instant::now();
                            p.calibrated = false;
                            p.baseline = snap
                                .monitors
                                .iter()
                                .find(|m| &m.id == monitor_id)
                                .and_then(|m| m.direction);
                            cmd_to_send = Some("command toggle-tiling-direction".into());
                        }
                    } else {
                        let dir = snap
                            .monitors
                            .iter()
                            .find(|m| &m.id == monitor_id)
                            .and_then(|m| m.direction);
                        if dir.is_some() && dir != p.baseline {
                            confirm = true;
                        }
                    }
                }
                PendingAction::Reconnect => {}
            }
        }
        if let Some(text) = cmd_to_send {
            let cmd_id = self.send_command(text);
            if let Some(p) = &mut self.pending {
                p.cmd_id = cmd_id;
            }
        }
        if confirm {
            self.confirm_pending();
        }
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    fn toggle_flyout(&mut self) {
        if self.flyout.as_ref().map(|f| f.visible).unwrap_or(false) {
            if self.flyout.as_ref().map(|f| f.interactive).unwrap_or(false) {
                self.close_flyout(false);
            } else {
                self.show_flyout();
            }
        } else {
            self.show_flyout();
        }
    }

    fn show_flyout(&mut self) {
        self.flyout_auto_hide = None;
        self.transient_change = None;
        self.show_flyout_mode(true);
    }

    /// Whether a transient (auto-hiding) status popup should be shown for a
    /// workspace change: only when the change was NOT initiated by a recent
    /// mouse click (i.e. keyboard shortcuts / GlazeWM bindings).
    fn should_show_transient(last_mouse_down_age_ms: u64) -> bool {
        last_mouse_down_age_ms >= MOUSE_DRIVEN_CHANGE_MS
    }

    fn show_transient_flyout(&mut self, change: UiChangeKind) {
        if self
            .flyout
            .as_ref()
            .map(|f| f.visible && f.interactive)
            .unwrap_or(false)
        {
            self.render_flyout();
            return;
        }
        let now = Instant::now();
        self.flyout_auto_hide = Some(now + std::time::Duration::from_millis(1600));
        self.transient_change = Some((change.clone(), now));
        self.show_flyout_mode(false);
        if let UiChangeKind::Direction { monitor_id } = change
            && let Some(flyout) = &mut self.flyout
        {
            flyout.spin_direction(monitor_id, now);
        }
        self.render_flyout();
    }

    fn show_flyout_mode(&mut self, interactive: bool) {
        let snap = self.snapshot.clone();
        let cursor = if interactive {
            win32::cursor_pos()
        } else {
            self.transient_monitor_point().or_else(win32::cursor_pos)
        }
        .unwrap_or(POINT { x: 0, y: 0 });

        if self.flyout.is_none() {
            self.flyout = Flyout::create(self.hinst, self.hwnd, self.anims);
            if let Some(f) = &mut self.flyout {
                Flyout::bind_global(f);
            }
        }
        let (scale, viewport_h) = match &mut self.flyout {
            Some(f) => f.begin_show(cursor),
            None => {
                tracing::error!("failed to create flyout window");
                return;
            }
        };
        self.flyout_viewport_h = viewport_h;
        let input = self.layout_input(viewport_h);
        let layout = compute_layout(snap.as_deref(), &self.fonts, &input);
        let anchor = self.tray.rect();
        let _ = scale;
        let Some(flyout) = &mut self.flyout else {
            return;
        };
        flyout.show(anchor, cursor, &layout, interactive);
        flyout.set_layout(layout);
        let focused = snap.as_ref().and_then(|s| s.focused_workspace_id.clone());
        flyout.set_kbd_initial(focused.as_ref());
        self.render_flyout();
    }

    /// A point inside the monitor affected by the current transient change.
    /// It is used for per-monitor DPI/work-area lookup, not as a visual anchor.
    fn transient_monitor_point(&self) -> Option<POINT> {
        let snapshot = self.snapshot.as_ref()?;
        let change = &self.transient_change.as_ref()?.0;
        let monitor = match change {
            UiChangeKind::Workspace { workspace_id } => snapshot
                .monitors
                .iter()
                .find(|monitor| monitor.workspaces.iter().any(|ws| &ws.id == workspace_id)),
            UiChangeKind::Direction { monitor_id } => snapshot
                .monitors
                .iter()
                .find(|monitor| &monitor.id == monitor_id),
            UiChangeKind::Pause { .. } => snapshot
                .focused_monitor_id
                .as_ref()
                .and_then(|id| snapshot.monitors.iter().find(|monitor| &monitor.id == id)),
        }?;
        let (x, y, width, height) = monitor.rect;
        Some(POINT {
            x: (x + width / 2.0).round() as i32,
            y: (y + height / 2.0).round() as i32,
        })
    }

    fn close_flyout(&mut self, immediate: bool) {
        self.flyout_auto_hide = None;
        self.transient_change = None;
        if let Some(f) = &mut self.flyout {
            f.hide(immediate);
            if immediate {
                self.render_flyout();
            }
        }
    }

    fn on_flyout_action(&mut self, action: FlyoutAction) {
        match action {
            FlyoutAction::FocusWorkspace { workspace_id, name } => {
                self.do_focus_workspace(workspace_id, name);
            }
            FlyoutAction::ToggleDirection { monitor_id } => {
                self.do_toggle_direction(monitor_id, true);
            }
            FlyoutAction::Reconnect => {
                self.ipc.tx.try_send(UiToIpc::Reconnect).ok();
            }
            FlyoutAction::Close => {
                self.close_flyout(false);
            }
            FlyoutAction::Scroll { delta } => {
                if let Some(f) = &mut self.flyout {
                    f.scroll_by(delta);
                }
                self.render_flyout();
            }
            FlyoutAction::DpiChanged => {
                self.reposition_flyout();
            }
        }
    }

    fn reposition_flyout(&mut self) {
        // Recompute placement (size/DPI may have changed) and re-render.
        if !self.flyout.as_ref().map(|f| f.visible).unwrap_or(false) {
            return;
        }
        let pos = self
            .flyout
            .as_ref()
            .map(|f| f.pos)
            .unwrap_or(POINT { x: 0, y: 0 });
        let snap = self.snapshot.clone();
        let interactive = self
            .flyout
            .as_ref()
            .map(|flyout| flyout.interactive)
            .unwrap_or(false);
        let cursor = if interactive {
            win32::cursor_pos()
        } else {
            self.transient_monitor_point().or_else(win32::cursor_pos)
        }
        .unwrap_or(pos);
        let (scale, viewport_h) = match &mut self.flyout {
            Some(f) => f.begin_show(cursor),
            None => return,
        };
        self.flyout_viewport_h = viewport_h;
        let input = self.layout_input(viewport_h);
        let layout = compute_layout(snap.as_deref(), &self.fonts, &input);
        let (lw, lh) = (
            layout.width + 2.0 * crate::flyout::SHADOW,
            layout.viewport_h + 2.0 * crate::flyout::SHADOW,
        );
        let (pw, ph) = ((lw * scale).ceil() as i32, (lh * scale).ceil() as i32);
        let work = win32::work_area_at(cursor.x, cursor.y);
        let pos = if interactive {
            crate::flyout::compute_position(
                self.tray.rect(),
                cursor,
                (pw, ph),
                work,
                win32::taskbar_edge(),
            )
        } else {
            crate::flyout::compute_transient_position((pw, ph), work)
        };
        let Some(flyout) = &mut self.flyout else {
            return;
        };
        flyout.scale = scale;
        flyout.pos = pos;
        flyout.set_layout(layout);
        self.render_flyout();
    }

    fn do_focus_workspace(&mut self, workspace_id: WorkspaceId, name: String) {
        if !can_encode_workspace_name(&name) {
            tracing::warn!(
                name,
                "workspace name cannot be safely encoded; action disabled"
            );
            self.error_ws = Some((workspace_id, Instant::now()));
            return;
        }
        let text = format!("command focus --workspace {name}");
        let cmd_id = self.send_command(text);
        self.start_pending(
            PendingAction::FocusWorkspace { workspace_id, name },
            cmd_id,
            None,
        );
    }

    fn do_toggle_direction(&mut self, monitor_id: MonitorId, focus_first: bool) {
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        let Some(mon) = snap.monitors.iter().find(|m| m.id == monitor_id) else {
            return;
        };
        if focus_first && !mon.is_focused {
            // Focus the monitor's displayed workspace first, then toggle.
            if let Some(ws) = mon
                .displayed_workspace_id
                .as_ref()
                .and_then(|id| mon.workspaces.iter().find(|w| &w.id == id))
            {
                if !can_encode_workspace_name(&ws.name) {
                    self.error_monitor = Some((monitor_id.clone(), Instant::now()));
                    return;
                }
                let text = format!("command focus --workspace {}", ws.name);
                let cmd_id = self.send_command(text);
                self.start_pending(
                    PendingAction::FocusThenToggle {
                        workspace_id: ws.id.clone(),
                        name: ws.name.clone(),
                        monitor_id,
                    },
                    cmd_id,
                    None,
                );
                return;
            }
        }
        let cmd_id = self.send_command("command toggle-tiling-direction".into());
        self.start_pending(
            PendingAction::ToggleDirection { monitor_id },
            cmd_id,
            mon.direction,
        );
    }

    fn toggle_focused_direction(&mut self) {
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        let Some(mid) = snap.focused_monitor_id.clone() else {
            return;
        };
        self.do_toggle_direction(mid, false);
    }

    fn scroll_switch_workspace(&mut self, direction: i32) {
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        let Some(mon) = snap
            .monitors
            .iter()
            .find(|m| m.is_focused)
            .or_else(|| snap.monitors.first())
        else {
            return;
        };
        if mon.workspaces.len() < 2 {
            return;
        }
        let cur = mon
            .displayed_workspace_id
            .as_ref()
            .and_then(|id| mon.workspaces.iter().position(|w| &w.id == id))
            .unwrap_or(0);
        let n = mon.workspaces.len();
        let next = ((cur as i32 + direction).rem_euclid(n as i32)) as usize;
        if let Some(ws) = mon.workspaces.get(next)
            && can_encode_workspace_name(&ws.name)
        {
            let text = format!("command focus --workspace {}", ws.name);
            let cmd_id = self.send_command(text);
            self.start_pending(
                PendingAction::FocusWorkspace {
                    workspace_id: ws.id.clone(),
                    name: ws.name.clone(),
                },
                cmd_id,
                None,
            );
        }
    }

    // ------------------------------------------------------------------
    // Tray icon + tooltip
    // ------------------------------------------------------------------

    fn tray_dpi(&self) -> u32 {
        win32::dpi_for_window(self.hwnd).max(96)
    }

    fn refresh_tray_dpi(&mut self) {
        self.last_icon_key = None;
        self.update_tray();
    }

    fn tray_icon_key(&self) -> TrayIconKey {
        let size = (16.0 * self.tray_dpi() as f32 / 96.0).round().max(16.0) as u32;
        let (label, direction, paused, conn) = match &self.snapshot {
            Some(s) => {
                let label = s
                    .focused_workspace_id
                    .as_ref()
                    .and_then(|id| {
                        s.monitors
                            .iter()
                            .flat_map(|m| m.workspaces.iter())
                            .find(|w| &w.id == id)
                    })
                    .map(|w| tray_label(&w.name))
                    .unwrap_or_else(|| "–".into());
                let direction = if self.config.tray.show_direction {
                    s.focused_direction
                } else {
                    None
                };
                let conn = match s.connection {
                    ConnectionState::Ready => TrayConnection::Ready,
                    ConnectionState::Disconnected => TrayConnection::Disconnected,
                    ConnectionState::Connecting { .. } | ConnectionState::Synchronizing => {
                        TrayConnection::Connecting
                    }
                    ConnectionState::Degraded { .. } => TrayConnection::Degraded,
                };
                (label, direction, s.is_paused, conn)
            }
            None => ("…".into(), None, false, TrayConnection::Connecting),
        };
        TrayIconKey {
            label,
            direction,
            paused,
            connection: conn,
            dark: self.dark,
            size,
        }
    }

    fn tooltip_text(&self) -> String {
        let mut tip = match &self.snapshot {
            Some(s) => match &s.connection {
                ConnectionState::Ready => {
                    let (mon, ws, dir) = (
                        s.focused_monitor_id
                            .as_ref()
                            .and_then(|id| s.monitors.iter().find(|m| &m.id == id))
                            .map(|m| {
                                self.monitor_names
                                    .get(&m.id)
                                    .cloned()
                                    .unwrap_or_else(|| m.display_name.clone())
                            })
                            .unwrap_or_else(|| "—".into()),
                        s.focused_workspace_id
                            .as_ref()
                            .and_then(|id| {
                                s.monitors
                                    .iter()
                                    .flat_map(|m| m.workspaces.iter())
                                    .find(|w| &w.id == id)
                            })
                            .map(|w| w.name.clone())
                            .unwrap_or_else(|| "—".into()),
                        s.focused_direction.map(|d| d.label()).unwrap_or("未知"),
                    );
                    let pause = if s.is_paused { " · 已暂停" } else { "" };
                    format!("GlazeTray · {mon} · 工作区 {ws} · {dir}布局{pause}")
                }
                ConnectionState::Connecting { .. } | ConnectionState::Synchronizing => {
                    "GlazeTray · 正在连接 GlazeWM".into()
                }
                ConnectionState::Disconnected => "GlazeTray · GlazeWM 未运行".into(),
                ConnectionState::Degraded { reason } => {
                    format!("GlazeTray · 状态同步失败（{reason}）")
                }
            },
            None => "GlazeTray · 正在连接 GlazeWM".into(),
        };
        if let Some(err) = &self.config_error {
            tip.push_str(" · 配置无效");
            let _ = err;
        }
        tip
    }

    fn update_tray(&mut self) {
        let key = self.tray_icon_key();
        if self.last_icon_key.as_ref() != Some(&key) {
            let hicon = self.icon_cache.entry(key.clone()).or_insert_with(|| {
                let buf = render_tray_icon(&key, &self.palette, &self.fonts);
                create_hicon(&buf, key.size, key.size).unwrap_or_default()
            });
            self.tray.set_icon(*hicon);
            self.last_icon_key = Some(key);
        }
        let tip = self.tooltip_text();
        if self.last_tooltip.as_deref() != Some(tip.as_str()) {
            self.last_tooltip = Some(tip.clone());
            self.tray.set_tooltip(&tip);
        }
    }

    // ------------------------------------------------------------------
    // Theme / config
    // ------------------------------------------------------------------

    fn refresh_theme(&mut self) {
        let dark = !win32::apps_use_light_theme();
        let high_contrast = win32::high_contrast_enabled();
        if dark == self.dark && high_contrast == self.high_contrast {
            return;
        }
        self.dark = dark;
        self.high_contrast = high_contrast;
        self.palette = compute_palette(ThemeInput {
            dark,
            accent: win32::system_accent_color(),
            use_system_accent: self.config.tray.use_system_accent,
            high_contrast,
        });
        self.anims = config::animation_enabled(&self.config, win32::animations_enabled());
        self.last_icon_key = None;
        self.update_tray();
        self.render_flyout();
    }

    fn check_config(&mut self) {
        let path = config::config_path();
        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        match config::load_config(&path) {
            Ok(cfg) => {
                self.config = cfg;
                self.config_error = None;
                tracing::info!("config reloaded");
                self.palette = compute_palette(ThemeInput {
                    dark: self.dark,
                    accent: win32::system_accent_color(),
                    use_system_accent: self.config.tray.use_system_accent,
                    high_contrast: self.high_contrast,
                });
                self.anims = config::animation_enabled(&self.config, win32::animations_enabled());
                // Startup preference sync (enable-only, never overrides the
                // user's manual choice to disable).
                if self.config.startup.launch_with_windows && !crate::startup::startup_enabled() {
                    match crate::startup::set_startup(true) {
                        Ok(()) => tracing::info!("startup entry enabled per config"),
                        Err(e) => tracing::warn!(error = %e, "failed to enable startup entry"),
                    }
                }
                // Push the GlazeWM connection settings to the IPC task.
                if let Some(glazewm) = self.ipc.glazewm_cfg() {
                    let mut lock = glazewm.write().unwrap_or_else(|e| e.into_inner());
                    *lock = self.config.glazewm.clone();
                }
                self.update_tray();
                self.render_flyout();
            }
            Err(config::ConfigError::Missing) => {
                self.config_error = None;
            }
            Err(e) => {
                self.config_error = Some(e.to_string());
                tracing::warn!(error = ?e, "config invalid; keeping last good config");
            }
        }
    }

    // ------------------------------------------------------------------
    // Frame / tick
    // ------------------------------------------------------------------

    fn layout_input(&self, viewport_h: f32) -> LayoutInput {
        let now = Instant::now();
        LayoutInput {
            palette: self.palette,
            fonts: self.fonts.clone(),
            epoch: self.epoch,
            now,
            confirm_ws: self.confirm.as_ref().map(|(id, _)| id.clone()),
            confirm_since: self.confirm.as_ref().map(|(_, t)| *t),
            error_ws: self.error_ws.as_ref().map(|(id, _)| id.clone()),
            error_monitor: self.error_monitor.as_ref().map(|(id, _)| id.clone()),
            pending: self.pending.as_ref().map(|p| p.action.clone()),
            kbd_ws: self.flyout.as_ref().and_then(|f| f.kbd_ws()),
            attention: self
                .transient_change
                .as_ref()
                .map(|(change, _)| change.clone()),
            show_empty: self.config.flyout.show_empty_workspaces,
            width: self.config.flyout.width,
            viewport_h,
        }
    }

    fn render_flyout(&mut self) {
        if !self.flyout.as_ref().map(|f| f.visible).unwrap_or(false) {
            return;
        }
        let snap = self.snapshot.clone();
        let input = self.layout_input(self.flyout_viewport_h);
        let mut layout = compute_layout(snap.as_deref(), &self.fonts, &input);
        // Patch monitor display names (system names when resolvable).
        for row in &mut layout.rows {
            if let Some(name) = self
                .snapshot
                .as_ref()
                .and_then(|s| s.monitors.iter().find(|m| m.id == row.monitor_id))
                .map(|m| self.monitor_display_name(m))
            {
                row.title = name;
            }
        }
        let Some(flyout) = &mut self.flyout else {
            return;
        };
        flyout.set_layout(layout.clone());
        flyout.render(&layout, &input);
    }

    fn on_tick(&mut self) {
        let now = Instant::now();

        if self
            .flyout_auto_hide
            .is_some_and(|deadline| now >= deadline)
        {
            self.close_flyout(false);
        }

        // Pending action deadlines.
        if let Some(p) = &self.pending {
            let elapsed = now.duration_since(p.since);
            if elapsed > std::time::Duration::from_secs(2) {
                self.fail_pending();
            } else if elapsed > std::time::Duration::from_millis(500) && !p.calibrated {
                self.ipc.tx.try_send(UiToIpc::Calibrate).ok();
                if let Some(p) = &mut self.pending {
                    p.calibrated = true;
                }
            }
        }

        // Visual state expiry.
        if let Some((_, t)) = &self.confirm
            && now.duration_since(*t) > std::time::Duration::from_millis(400)
        {
            self.confirm = None;
        }
        if let Some((_, t)) = &self.error_ws
            && now.duration_since(*t) > std::time::Duration::from_millis(1500)
        {
            self.error_ws = None;
        }
        if let Some((_, t)) = &self.error_monitor
            && now.duration_since(*t) > std::time::Duration::from_millis(1500)
        {
            self.error_monitor = None;
        }

        // Config watch (every ~2s on the slow tick).
        if now.duration_since(self.last_config_check) > std::time::Duration::from_secs(2) {
            self.last_config_check = now;
            self.check_config();
        }

        // Flyout animation tick.
        let animating = self.pending.is_some()
            || self.confirm.is_some()
            || self.error_ws.is_some()
            || self.error_monitor.is_some();
        let flyout_visible = self.flyout.as_ref().map(|f| f.visible).unwrap_or(false);
        if let Some(f) = &mut self.flyout {
            let changed = f.tick(now);
            if changed || (flyout_visible && animating) {
                self.render_flyout();
            }
        }

        // The fast 16ms tick is only kept alive while it is actually needed
        // (flyout visible or feedback animating); otherwise the UI idles on
        // the 250ms tick — no high-frequency wakeups at rest.
        let need_fast = flyout_visible || animating;
        if need_fast != self.fast_timer {
            self.fast_timer = need_fast;
            unsafe {
                SetTimer(
                    Some(self.hwnd),
                    TIMER_UI,
                    if need_fast {
                        TIMER_FAST_MS
                    } else {
                        TIMER_SLOW_MS
                    },
                    None,
                );
            }
            tracing::debug!(need_fast, "ui timer period adjusted");
        }
    }

    // ------------------------------------------------------------------
    // Shutdown
    // ------------------------------------------------------------------

    fn shutdown(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        tracing::info!("shutting down");
        self.close_flyout(true);
        // NIM_DELETE first, then release every cached HICON (the cache is the
        // sole owner of these handles).
        self.tray.remove();
        for hicon in self.icon_cache.drain().map(|(_, h)| h) {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(hicon);
            }
        }
        self.ipc.tx.try_send(UiToIpc::Shutdown).ok();
        crate::flyout::uninstall_permanent_mouse_hook();
        unsafe {
            KillTimer(Some(self.hwnd), TIMER_UI).ok();
        }
        if let Some(f) = &mut self.flyout {
            f.destroy();
        }
    }
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

static MUTEX_HANDLE: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

/// Returns true when this is the first instance. Otherwise pings the existing
/// instance (asking it to show the flyout) and returns false.
pub fn ensure_single_instance() -> bool {
    let (handle, first) = win32::create_single_instance_mutex("Local\\GlazeTray.SingleInstance");
    if !first {
        win32::close_handle(handle);
        let class_wide = win32::wide(HIDDEN_CLASS);
        unsafe {
            let hwnd = FindWindowW(win32::pcwstr(&class_wide), PCWSTR::null()).unwrap_or_default();
            if !hwnd.0.is_null() {
                windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    hwnd,
                    WM_APP_ACTIVATE,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );
            }
        }
        return false;
    }
    let _ = MUTEX_HANDLE.set(handle.0 as isize);
    true
}

/// Decode a NOTIFYICON_VERSION_4 tray notification callback message.
///
/// Layout (Windows notification area, version 4):
/// - `LOWORD(lParam)` = the notification event (e.g. `WM_LBUTTONUP`);
/// - `HIWORD(lParam)` = the tray icon id (`uID`);
/// - `wParam` = packed cursor coordinates (LOWORD = x, HIWORD = y);
/// - for `WM_MOUSEWHEEL`, `HIWORD(wParam)` carries the wheel delta instead.
///
/// Returns `(event, icon_id, x, y, wheel_delta)`.
pub fn decode_tray_notification(wparam: usize, lparam: isize) -> (u32, u16, i32, i32, i32) {
    let raw_lparam = lparam as u32;
    let raw_wparam = wparam as u32;
    let event = raw_lparam & 0xFFFF;
    let icon_id = (raw_lparam >> 16) as u16;
    let x = (raw_wparam & 0xFFFF) as u16 as i16 as i32;
    let y = (raw_wparam >> 16) as u16 as i16 as i32;
    let wheel_delta = ((raw_wparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    (event, icon_id, x, y, wheel_delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wparam_from_xy(x: i32, y: i32) -> usize {
        ((x as u16) as usize) | (((y as u16) as usize) << 16)
    }

    fn lparam_from_event(event: u32, id: u16) -> isize {
        (event | ((id as u32) << 16)) as isize
    }

    #[test]
    fn tray_click_event_is_in_lparam() {
        let (event, id, x, y, _) =
            decode_tray_notification(wparam_from_xy(1200, 800), lparam_from_event(0x0202, 0));
        assert_eq!(event, 0x0202); // WM_LBUTTONUP
        assert_eq!(id, 0);
        assert_eq!(x, 1200);
        assert_eq!(y, 800);
    }

    #[test]
    fn tray_context_menu_and_other_events() {
        let (event, _id, _, _, _) = decode_tray_notification(0, lparam_from_event(0x007B, 0)); // WM_CONTEXTMENU
        assert_eq!(event, 0x007B);
        let (event, _id, _, _, _) = decode_tray_notification(0, lparam_from_event(0x0208, 0)); // WM_MBUTTONUP
        assert_eq!(event, 0x0208);
    }

    #[test]
    fn tray_icon_id_is_in_high_word_of_lparam() {
        let (_, id, _, _, _) = decode_tray_notification(0, lparam_from_event(0x0202, 7));
        assert_eq!(id, 7);
    }

    #[test]
    fn tray_wheel_delta_from_high_word_of_wparam() {
        // Wheel: wParam high word = delta; coordinates in lParam (like a real
        // WM_MOUSEWHEEL). Our decoder returns delta from wParam regardless.
        let wparam = (120i32 as u16 as usize) << 16;
        let (event, _, _, _, delta) =
            decode_tray_notification(wparam, lparam_from_event(0x020A, 0));
        assert_eq!(event, 0x020A); // WM_MOUSEWHEEL
        assert_eq!(delta, 120);
    }

    #[test]
    fn tray_negative_coordinates_roundtrip() {
        let (_, _, x, y, _) =
            decode_tray_notification(wparam_from_xy(-320, -100), lparam_from_event(0x0202, 0));
        assert_eq!(x, -320);
        assert_eq!(y, -100);
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;

    #[test]
    fn transient_popup_shown_for_keyboard_driven_changes() {
        // No mouse click in the window: keyboard shortcut switch → show.
        assert!(App::should_show_transient(u64::MAX));
        assert!(App::should_show_transient(5_000));
        assert!(App::should_show_transient(250));
    }

    #[test]
    fn transient_popup_suppressed_for_mouse_driven_changes() {
        // Recent mouse button-down: clicking a window → suppress.
        assert!(!App::should_show_transient(0));
        assert!(!App::should_show_transient(50));
        assert!(!App::should_show_transient(249));
    }
}
