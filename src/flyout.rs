//! The taskbar flyout: a borderless, non-activating layered popup that shows
//! per-monitor workspace state and handles workspace/direction actions.

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;

use tiny_skia::{Color, FillRule, Mask, Pixmap, Transform};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HGDIOBJ, SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, HHOOK, HTCLIENT, HTTRANSPARENT,
    HWND_TOPMOST, KBDLLHOOKSTRUCT, MA_NOACTIVATE, MOUSEHOOKSTRUCT, PostMessageW, RegisterClassW,
    SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos, SetWindowsHookExW,
    ShowWindow, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEWHEEL, WM_NCHITTEST,
    WNDCLASS_STYLES, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::PCWSTR;

use crate::fonts::Fonts;
use crate::render::{
    draw_shadow, draw_text, draw_text_centered, fill_circle, fill_rounded, fit_font_size,
    measure_text, rgba_to_bgra_premultiplied, rounded_rect, stroke_rounded,
};
use crate::state::{
    AppSnapshot, MonitorId, PendingAction, TilingDirection, UiChangeKind, WorkspaceId,
};
use crate::theme::Palette;
use crate::win32;

// ---------------------------------------------------------------------------
// Messages (posted to the app's hidden window by the flyout / hooks)
// ---------------------------------------------------------------------------

pub const WM_APP_FLYOUT_ACTION: u32 = 0x8000 + 40;
pub const WM_APP_HOOK_OUTSIDE: u32 = 0x8000 + 41;
pub const WM_APP_HOOK_KEY: u32 = 0x8000 + 42;

#[derive(Debug, Clone)]
pub enum FlyoutAction {
    FocusWorkspace {
        workspace_id: WorkspaceId,
        name: String,
    },
    ToggleDirection {
        monitor_id: MonitorId,
    },
    Reconnect,
    Close,
    Scroll {
        delta: f32,
    },
    DpiChanged,
}

// ---------------------------------------------------------------------------
// Layout model (logical pixels; content-relative, pre-scroll)
// ---------------------------------------------------------------------------

pub const MARGIN: f32 = 14.0;
pub const HEADER_H: f32 = 34.0;
pub const ROW_PAD_Y: f32 = 10.0;
pub const MONITOR_W: f32 = 126.0;
pub const DIRECTION_W: f32 = 68.0;
pub const SECTION_GAP: f32 = 10.0;
pub const BTN_H: f32 = 30.0;
pub const BTN_MIN_W: f32 = 30.0;
pub const BTN_MAX_W: f32 = 54.0;
pub const CTRL_GAP: f32 = 6.0;
pub const SHADOW: f32 = 10.0;
pub const CORNER: f32 = 10.0;
pub const BTN_CORNER: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FRect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVisual {
    Normal,
    Displayed,
    Focused,
    DisplayedFocused,
    Pending,
    Error,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct ButtonLayout {
    pub id: WorkspaceId,
    pub label: String,
    pub rect: FRect,
    pub visual: ButtonVisual,
    pub has_windows: bool,
    pub switchable: bool,
}

#[derive(Debug, Clone)]
pub struct RowLayout {
    pub monitor_id: MonitorId,
    pub is_focused: bool,
    pub monitor_number: usize,
    pub title: String,
    pub title_rect: FRect,
    pub meta_rect: FRect,
    pub separator_y: f32,
    pub dir_rect: FRect,
    pub direction: Option<TilingDirection>,
    pub dir_pending: bool,
    pub buttons: Vec<ButtonLayout>,
}

#[derive(Debug, Clone)]
pub struct HeaderLayout {
    pub count_text: String,
    pub count_rect: FRect,
    pub status_rect: FRect,
    pub is_paused: bool,
    pub attention: bool,
}

#[derive(Debug, Clone)]
pub struct StatusLayout {
    pub rect: FRect,
    pub text: String,
    pub reconnect_rect: Option<FRect>,
    pub degraded: bool,
}

#[derive(Debug, Clone)]
pub struct FlyoutLayout {
    pub width: f32,
    pub content_h: f32,
    pub viewport_h: f32,
    pub scroll_max: f32,
    pub header: HeaderLayout,
    pub rows: Vec<RowLayout>,
    pub status: Option<StatusLayout>,
    pub buttons: Vec<ButtonLayout>,
}

/// Per-frame rendering/layout parameters.
pub struct LayoutInput {
    pub palette: Palette,
    pub fonts: std::sync::Arc<Fonts>,
    pub epoch: Instant,
    pub now: Instant,
    pub confirm_ws: Option<WorkspaceId>,
    pub confirm_since: Option<Instant>,
    pub error_ws: Option<WorkspaceId>,
    pub error_monitor: Option<MonitorId>,
    pub pending: Option<PendingAction>,
    /// Workspace id currently highlighted for keyboard navigation.
    pub kbd_ws: Option<WorkspaceId>,
    pub attention: Option<UiChangeKind>,
    pub show_empty: bool,
    pub width: f32,
    pub viewport_h: f32,
}

fn button_visual(b: &crate::state::WorkspaceInfo, input: &LayoutInput) -> ButtonVisual {
    if input.error_ws.as_deref() == Some(b.id.as_str()) {
        return ButtonVisual::Error;
    }
    let pending_this = matches!(
        input.pending,
        Some(PendingAction::FocusWorkspace { ref workspace_id, .. })
            | Some(PendingAction::FocusThenToggle { ref workspace_id, .. })
            if workspace_id == &b.id
    );
    if pending_this {
        return ButtonVisual::Pending;
    }
    if !b.switchable {
        return ButtonVisual::Disabled;
    }
    match (b.is_displayed, b.is_focused) {
        (true, true) => ButtonVisual::DisplayedFocused,
        (true, false) => ButtonVisual::Displayed,
        (false, true) => ButtonVisual::Focused,
        (false, false) => ButtonVisual::Normal,
    }
}

fn workspace_button_width(font: &crate::fonts::TextEngine, label: &str) -> f32 {
    (measure_text(font, crate::fonts::REGULAR, label, 12.0) + 16.0).clamp(BTN_MIN_W, BTN_MAX_W)
}

/// Compute the flyout layout. Pure (given fonts + input) and unit-testable.
pub fn compute_layout(
    snapshot: Option<&AppSnapshot>,
    fonts: &Fonts,
    input: &LayoutInput,
) -> FlyoutLayout {
    let font = fonts.get(crate::fonts::REGULAR);
    let mut desired_width = input.width.max(420.0);
    if let Some(snap) = snapshot {
        for mon in &snap.monitors {
            let workspaces: Vec<_> = mon
                .workspaces
                .iter()
                .filter(|w| input.show_empty || w.is_displayed || w.is_focused)
                .collect();
            let rail_width = workspaces
                .iter()
                .map(|ws| workspace_button_width(font, &ws.name))
                .sum::<f32>()
                + CTRL_GAP * workspaces.len().saturating_sub(1) as f32;
            let required =
                2.0 * MARGIN + MONITOR_W + SECTION_GAP + rail_width + SECTION_GAP + DIRECTION_W;
            desired_width = desired_width.max(required);
        }
    }
    let width = desired_width.clamp(420.0, 560.0);
    let content_w = width - 2.0 * MARGIN;
    let mut rows = Vec::new();
    let mut buttons: Vec<ButtonLayout> = Vec::new();
    let is_paused = snapshot.map(|s| s.is_paused).unwrap_or(false);
    let monitor_count = snapshot.map(|s| s.monitors.len()).unwrap_or(0);
    let header = HeaderLayout {
        count_text: format!("{monitor_count} 台显示器"),
        count_rect: FRect {
            x: MARGIN,
            y: 9.0,
            w: 120.0,
            h: 17.0,
        },
        status_rect: FRect {
            x: width - MARGIN - 66.0,
            y: 6.0,
            w: 66.0,
            h: 22.0,
        },
        is_paused,
        attention: matches!(input.attention, Some(UiChangeKind::Pause { .. })),
    };
    let mut content_h = HEADER_H;

    if let Some(snap) = snapshot {
        for mon in &snap.monitors {
            let vis: Vec<&crate::state::WorkspaceInfo> = mon
                .workspaces
                .iter()
                .filter(|w| input.show_empty || w.is_displayed || w.is_focused)
                .collect();
            let row_y = content_h;
            let rail_x = MARGIN + MONITOR_W + SECTION_GAP;
            let rail_right = width - MARGIN - DIRECTION_W - SECTION_GAP;
            let dir_pending = input
                .error_monitor
                .as_deref()
                .map(|m| m == mon.id.as_str())
                .unwrap_or(false)
                || matches!(
                    input.pending,
                    Some(PendingAction::ToggleDirection { ref monitor_id })
                        | Some(PendingAction::FocusThenToggle { ref monitor_id, .. })
                    if monitor_id == &mon.id
                )
                || matches!(
                    input.attention,
                    Some(UiChangeKind::Direction { ref monitor_id }) if monitor_id == &mon.id
                );

            let mut btn_x = rail_x;
            let mut line = 0usize;
            let mut row_buttons = Vec::new();
            for ws in vis {
                let label = ws.name.clone();
                let bw = workspace_button_width(font, &label);
                if btn_x + bw > rail_right && !row_buttons.is_empty() {
                    line += 1;
                    btn_x = rail_x;
                }
                let rect = FRect {
                    x: btn_x,
                    y: row_y + ROW_PAD_Y + line as f32 * (BTN_H + CTRL_GAP),
                    w: bw,
                    h: BTN_H,
                };
                let bl = ButtonLayout {
                    id: ws.id.clone(),
                    label,
                    rect,
                    visual: button_visual(ws, input),
                    has_windows: ws.window_count > 0,
                    switchable: ws.switchable,
                };
                buttons.push(bl.clone());
                row_buttons.push(bl);
                btn_x += bw + CTRL_GAP;
            }
            let line_count = line + 1;
            let rail_h = line_count as f32 * BTN_H + line_count.saturating_sub(1) as f32 * CTRL_GAP;
            let row_h = (rail_h + 2.0 * ROW_PAD_Y).max(50.0);
            let label_y = row_y + (row_h - 30.0) / 2.0;
            rows.push(RowLayout {
                monitor_id: mon.id.clone(),
                is_focused: mon.is_focused,
                monitor_number: mon.order + 1,
                title: mon.display_name.clone(),
                title_rect: FRect {
                    x: MARGIN + 22.0,
                    y: label_y + 13.0,
                    w: MONITOR_W - 22.0,
                    h: 17.0,
                },
                meta_rect: FRect {
                    x: MARGIN + 22.0,
                    y: label_y,
                    w: MONITOR_W - 22.0,
                    h: 13.0,
                },
                separator_y: row_y + row_h,
                dir_rect: FRect {
                    x: width - MARGIN - DIRECTION_W,
                    y: row_y + (row_h - BTN_H) / 2.0,
                    w: DIRECTION_W,
                    h: BTN_H,
                },
                direction: mon.direction,
                dir_pending,
                buttons: row_buttons,
            });
            content_h = row_y + row_h;
        }
    }

    // Status row (connection issues / empty state).
    let mut status = None;
    let conn = snapshot.map(|s| s.connection.clone());
    let degraded = matches!(conn, Some(crate::state::ConnectionState::Degraded { .. }));
    let needs_status = match &conn {
        None => true,
        Some(crate::state::ConnectionState::Ready) => {
            snapshot.map(|s| s.monitors.is_empty()).unwrap_or(true)
        }
        _ => true,
    };
    if needs_status {
        let text = match &conn {
            None | Some(crate::state::ConnectionState::Disconnected) => {
                "GlazeWM 未运行".to_string()
            }
            Some(crate::state::ConnectionState::Connecting { .. }) => {
                "正在连接 GlazeWM…".to_string()
            }
            Some(crate::state::ConnectionState::Synchronizing) => "正在同步状态…".to_string(),
            Some(crate::state::ConnectionState::Ready) => "未检测到显示器".to_string(),
            Some(crate::state::ConnectionState::Degraded { reason }) => reason.clone(),
        };
        let sy = content_h.max(HEADER_H) + 12.0;
        let sh = 56.0;
        let reconnect = if degraded
            || matches!(
                conn,
                None | Some(crate::state::ConnectionState::Disconnected)
            ) {
            Some(FRect {
                x: MARGIN,
                y: sy + 28.0,
                w: 96.0,
                h: 22.0,
            })
        } else {
            None
        };
        status = Some(StatusLayout {
            rect: FRect {
                x: MARGIN,
                y: sy,
                w: content_w,
                h: sh,
            },
            text,
            reconnect_rect: reconnect,
            degraded,
        });
        content_h = sy + sh + MARGIN;
    }

    if !rows.is_empty() {
        content_h += 8.0;
    }
    content_h = content_h.max(0.0);
    let viewport_h = input.viewport_h.min(content_h.max(1.0));
    let scroll_max = (content_h - viewport_h).max(0.0);

    FlyoutLayout {
        width,
        content_h,
        viewport_h,
        scroll_max,
        header,
        rows,
        status,
        buttons,
    }
}

// ---------------------------------------------------------------------------
// Flyout window
// ---------------------------------------------------------------------------

static FLYOUT_PTR: AtomicPtr<Flyout> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "system" fn flyout_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = FLYOUT_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        let flyout = unsafe { &mut *ptr };
        if flyout.hwnd == hwnd
            && let Some(lr) = flyout.handle_message(msg, wparam, lparam)
        {
            return lr;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn post_action(action: FlyoutAction, target: HWND) {
    let boxed = Box::into_raw(Box::new(action));
    unsafe {
        PostMessageW(
            Some(target),
            WM_APP_FLYOUT_ACTION,
            WPARAM(boxed as usize),
            LPARAM(0),
        )
        .ok();
    }
}

pub struct Flyout {
    pub hwnd: HWND,
    app_hwnd: HWND,
    pub visible: bool,
    pub interactive: bool,
    pub scale: f32,
    size_logical: (f32, f32),
    scroll: f32,
    fade: f32,
    fade_target: f32,
    edge: u32,
    anims: bool,
    last_layout: Option<FlyoutLayout>,
    kbd_index: Option<usize>,
    spin: Option<(MonitorId, Instant)>,
    last_tick: Instant,
    pub pos: POINT,
}

static CLASS_WIDE: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();

impl Flyout {
    pub fn register_class(hinst: HINSTANCE) {
        let name = CLASS_WIDE.get_or_init(|| win32::wide("GlazeTray.FlyoutWindow"));
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(flyout_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
            hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR::default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR::from_raw(name.as_ptr()),
        };
        unsafe {
            RegisterClassW(&wc);
        }
    }

    pub fn create(hinst: HINSTANCE, app_hwnd: HWND, anims: bool) -> Option<Flyout> {
        let name = CLASS_WIDE.get_or_init(|| win32::wide("GlazeTray.FlyoutWindow"));
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(
                    WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0,
                ),
                PCWSTR::from_raw(name.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinst),
                None,
            )
        }
        .ok()?;
        Some(Flyout {
            hwnd,
            app_hwnd,
            visible: false,
            interactive: false,
            scale: 1.0,
            size_logical: (0.0, 0.0),
            scroll: 0.0,
            fade: 0.0,
            fade_target: 0.0,
            edge: win32::ABE_BOTTOM,
            anims,
            last_layout: None,
            kbd_index: None,
            spin: None,
            last_tick: Instant::now(),
            pos: POINT { x: 0, y: 0 },
        })
    }

    /// Keep the WndProc's global pointer in sync with the owned instance.
    /// The `Flyout` lives inside `App` on the UI thread; `App` never moves
    /// after startup, so the address is stable for the process lifetime.
    pub fn bind_global(flyout: &mut Flyout) {
        FLYOUT_PTR.store(flyout as *mut Flyout, Ordering::SeqCst);
    }

    pub fn destroy(&mut self) {
        uninstall_hooks();
        unsafe {
            DestroyWindow(self.hwnd).ok();
        }
        FLYOUT_PTR.store(std::ptr::null_mut(), Ordering::SeqCst);
    }

    // ------------------------------------------------------------------
    // Show / hide / tick
    // ------------------------------------------------------------------

    /// Probe the DPI of the monitor under `cursor` by placing the window there
    /// (GetDpiForMonitor is unreliable for PerMonitorV2 processes). Returns
    /// (scale_factor, suggested viewport height).
    pub fn begin_show(&mut self, cursor: POINT) -> (f32, f32) {
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                cursor.x,
                cursor.y,
                1,
                1,
                SWP_NOACTIVATE | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            )
            .ok();
        }
        let dpi = win32::dpi_for_window(self.hwnd);
        self.scale = win32::scale_factor(dpi);
        self.edge = win32::taskbar_edge();
        let work = win32::work_area_at(cursor.x, cursor.y);
        let viewport_h = ((work.bottom - work.top) as f32 / self.scale * 0.6).max(120.0);
        (self.scale, viewport_h)
    }

    pub fn show(
        &mut self,
        anchor: Option<RECT>,
        cursor: POINT,
        layout: &FlyoutLayout,
        interactive: bool,
    ) {
        let (lw, lh) = (
            layout.width + 2.0 * SHADOW,
            layout.viewport_h + 2.0 * SHADOW,
        );
        self.size_logical = (lw, lh);
        let (pw, ph) = (
            (lw * self.scale).ceil() as i32,
            (lh * self.scale).ceil() as i32,
        );
        let work = win32::work_area_at(cursor.x, cursor.y);
        let pos = if interactive {
            compute_position(anchor, cursor, (pw, ph), work, self.edge)
        } else {
            compute_transient_position((pw, ph), work)
        };
        self.pos = pos;
        self.scroll = 0.0;
        self.kbd_index = None;
        self.spin = None;
        self.fade = if self.anims { 0.0 } else { 1.0 };
        self.fade_target = 1.0;
        self.last_tick = Instant::now();
        self.interactive = interactive;

        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                pos.x,
                pos.y,
                pw,
                ph,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .ok();
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
        self.visible = true;
        if interactive {
            install_hooks(self.hwnd, anchor);
        } else {
            uninstall_hooks();
        }
    }

    pub fn hide(&mut self, immediate: bool) {
        if !self.visible {
            return;
        }
        if immediate || !self.anims {
            self.fade = 0.0;
            self.fade_target = 0.0;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
            uninstall_hooks();
        } else {
            self.fade_target = 0.0;
        }
    }

    /// Advance animations. Returns true when a redraw is needed.
    pub fn tick(&mut self, now: Instant) -> bool {
        if !self.visible {
            return false;
        }
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        let mut changed = false;
        if self.fade != self.fade_target {
            let speed = if self.fade_target >= 1.0 {
                1.0 / 0.12
            } else {
                1.0 / 0.08
            };
            if self.fade_target >= 1.0 {
                self.fade = (self.fade + dt * speed).min(1.0);
            } else {
                self.fade = (self.fade - dt * speed).max(0.0);
                if self.fade <= 0.0 {
                    unsafe {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                    self.visible = false;
                    uninstall_hooks();
                }
            }
            changed = true;
        }
        if let Some((_, start)) = &self.spin
            && start.elapsed() > std::time::Duration::from_millis(200)
        {
            self.spin = None;
            changed = true;
        }
        changed
    }

    pub fn spin_direction(&mut self, monitor_id: MonitorId, now: Instant) {
        self.spin = Some((monitor_id, now));
    }

    fn direction_pulse(&self, monitor_id: &MonitorId, now: Instant) -> f32 {
        if let Some((mid, start)) = &self.spin
            && mid == monitor_id
        {
            let t = (now.duration_since(*start).as_secs_f32() / 0.20).clamp(0.0, 1.0);
            return (t * std::f32::consts::PI).sin();
        }
        0.0
    }

    pub fn scroll_by(&mut self, delta: f32) {
        if let Some(layout) = &self.last_layout {
            self.scroll = (self.scroll + delta).clamp(0.0, layout.scroll_max);
        }
    }

    pub fn set_layout(&mut self, layout: FlyoutLayout) {
        self.last_layout = Some(layout);
    }

    #[allow(dead_code)]
    pub fn last_layout(&self) -> Option<&FlyoutLayout> {
        self.last_layout.as_ref()
    }

    pub fn set_kbd_initial(&mut self, focused_id: Option<&WorkspaceId>) {
        if let Some(layout) = &self.last_layout {
            self.kbd_index =
                focused_id.and_then(|fid| layout.buttons.iter().position(|b| &b.id == fid));
        }
    }

    pub fn kbd_ws(&self) -> Option<WorkspaceId> {
        let layout = self.last_layout.as_ref()?;
        let idx = self.kbd_index?;
        layout.buttons.get(idx).map(|b| b.id.clone())
    }

    fn move_kbd(&mut self, delta: i32) {
        let Some(layout) = &self.last_layout else {
            return;
        };
        if layout.buttons.is_empty() {
            return;
        }
        let n = layout.buttons.len();
        let cur = self.kbd_index.unwrap_or(0) as i32;
        self.kbd_index = Some(((cur + delta).rem_euclid(n as i32)) as usize);
    }

    // ------------------------------------------------------------------
    // Frame rendering
    // ------------------------------------------------------------------

    pub fn render(&mut self, layout: &FlyoutLayout, input: &LayoutInput) {
        let scale = self.scale;
        let (pw, ph) = (
            ((layout.width + 2.0 * SHADOW) * scale).ceil() as u32,
            ((layout.viewport_h + 2.0 * SHADOW) * scale).ceil() as u32,
        );
        if pw == 0 || ph == 0 {
            return;
        }
        let mut px = Pixmap::new(pw, ph).expect("flyout pixmap");
        let pal = input.palette;
        let content_x = SHADOW * scale;
        let content_y = SHADOW * scale;

        // Shadow.
        draw_shadow(
            &mut px,
            (
                content_x,
                content_y,
                layout.width * scale,
                layout.viewport_h * scale,
            ),
            CORNER * scale,
            SHADOW * scale,
            pal.shadow,
        );

        // Content clip mask.
        let mut mask = Mask::new(pw, ph).expect("mask");
        if let Some(path) = rounded_rect(
            content_x,
            content_y,
            layout.width * scale,
            layout.viewport_h * scale,
            CORNER * scale,
        ) {
            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        }
        let clip = Some(&mask);

        // Background.
        fill_rounded(
            &mut px,
            content_x,
            content_y,
            layout.width * scale,
            layout.viewport_h * scale,
            CORNER * scale,
            pal.surface,
            clip,
        );
        stroke_rounded(
            &mut px,
            content_x + 0.5 * scale,
            content_y + 0.5 * scale,
            (layout.width - 1.0) * scale,
            (layout.viewport_h - 1.0) * scale,
            CORNER * scale,
            pal.border,
            scale.max(1.0),
            clip,
        );

        let scroll_px = self.scroll.clamp(0.0, layout.scroll_max) * scale;
        let ty = content_y - scroll_px;
        let font = fonts_reg(input);
        let med = fonts_med(input);

        // HUD header: monitor count on the left, global run state on the right.
        let hdr = &layout.header;
        draw_text(
            &mut px,
            font,
            crate::fonts::REGULAR,
            11.0 * scale,
            &hdr.count_text,
            content_x + hdr.count_rect.x * scale,
            ty + hdr.count_rect.y * scale,
            pal.text_secondary,
            clip,
        );
        let status_color = if hdr.is_paused {
            pal.warning
        } else {
            pal.accent
        };
        if hdr.is_paused || hdr.attention {
            fill_rounded(
                &mut px,
                content_x + hdr.status_rect.x * scale,
                ty + hdr.status_rect.y * scale,
                hdr.status_rect.w * scale,
                hdr.status_rect.h * scale,
                5.0 * scale,
                lerp_color(
                    pal.surface,
                    status_color,
                    if hdr.attention { 0.30 } else { 0.14 },
                ),
                clip,
            );
        }
        fill_circle(
            &mut px,
            content_x + (hdr.status_rect.x + 9.0) * scale,
            ty + (hdr.status_rect.y + hdr.status_rect.h / 2.0) * scale,
            2.5 * scale,
            status_color,
            clip,
        );
        draw_text(
            &mut px,
            med,
            crate::fonts::MEDIUM,
            10.5 * scale,
            if hdr.is_paused {
                "已暂停"
            } else {
                "运行中"
            },
            content_x + (hdr.status_rect.x + 17.0) * scale,
            ty + (hdr.status_rect.y + 5.0) * scale,
            if hdr.is_paused {
                pal.warning
            } else {
                pal.text_primary
            },
            clip,
        );
        fill_rounded(
            &mut px,
            content_x,
            ty + (HEADER_H - 1.0) * scale,
            layout.width * scale,
            scale.max(1.0),
            0.0,
            pal.border,
            clip,
        );

        for row in &layout.rows {
            if row.is_focused {
                fill_rounded(
                    &mut px,
                    content_x + 2.0 * scale,
                    ty + (row.meta_rect.y + 1.0) * scale,
                    2.5 * scale,
                    28.0 * scale,
                    1.25 * scale,
                    pal.accent,
                    clip,
                );
            }

            draw_monitor_glyph(
                &mut px,
                content_x + (MARGIN + 7.0) * scale,
                ty + (row.meta_rect.y + 15.0) * scale,
                14.0 * scale,
                if row.is_focused {
                    pal.accent
                } else {
                    pal.text_disabled
                },
                clip,
            );
            draw_text(
                &mut px,
                font,
                crate::fonts::REGULAR,
                9.5 * scale,
                &format!("显示器 {}", row.monitor_number),
                content_x + row.meta_rect.x * scale,
                ty + row.meta_rect.y * scale,
                if row.is_focused {
                    pal.accent
                } else {
                    pal.text_disabled
                },
                clip,
            );
            let title = elide_text(med, &row.title, 12.0, row.title_rect.w);
            draw_text(
                &mut px,
                med,
                crate::fonts::MEDIUM,
                12.0 * scale,
                &title,
                content_x + row.title_rect.x * scale,
                ty + row.title_rect.y * scale,
                pal.text_primary,
                clip,
            );

            // Direction control uses a two-pane layout glyph instead of a bar.
            let d = row.dir_rect;
            if row.direction.is_some() {
                let pulse = self.direction_pulse(&row.monitor_id, input.now);
                fill_rounded(
                    &mut px,
                    content_x + d.x * scale,
                    ty + d.y * scale,
                    d.w * scale,
                    d.h * scale,
                    BTN_CORNER * scale,
                    if row.dir_pending {
                        lerp_color(pal.surface_alt, pal.accent, 0.20 + 0.12 * pulse)
                    } else {
                        pal.surface_alt
                    },
                    clip,
                );
                draw_layout_glyph(
                    &mut px,
                    content_x + (d.x + 14.0) * scale,
                    ty + (d.y + d.h / 2.0) * scale,
                    14.0 * scale,
                    row.direction.unwrap_or(TilingDirection::Horizontal),
                    if row.dir_pending {
                        pal.accent
                    } else {
                        pal.text_secondary
                    },
                    clip,
                );
                draw_text(
                    &mut px,
                    med,
                    crate::fonts::MEDIUM,
                    10.5 * scale,
                    row.direction.map(|v| v.label()).unwrap_or("--"),
                    content_x + (d.x + 27.0) * scale,
                    ty + (d.y + 6.0) * scale,
                    if row.dir_pending {
                        pal.accent
                    } else {
                        pal.text_primary
                    },
                    clip,
                );
            }
            for b in &row.buttons {
                self.draw_button(&mut px, input, b, content_x, ty, scale, clip);
            }
            fill_rounded(
                &mut px,
                content_x + MARGIN * scale,
                ty + (row.separator_y - 1.0) * scale,
                (layout.width - 2.0 * MARGIN) * scale,
                scale.max(1.0),
                0.0,
                pal.border,
                clip,
            );
        }

        // Status row.
        if let Some(st) = &layout.status {
            draw_text(
                &mut px,
                font,
                crate::fonts::REGULAR,
                13.0 * scale,
                &st.text,
                content_x + st.rect.x * scale,
                ty + st.rect.y * scale,
                if st.degraded {
                    pal.error
                } else {
                    pal.text_secondary
                },
                clip,
            );
            if let Some(rr) = &st.reconnect_rect {
                fill_rounded(
                    &mut px,
                    content_x + rr.x * scale,
                    ty + rr.y * scale,
                    rr.w * scale,
                    rr.h * scale,
                    BTN_CORNER * scale,
                    pal.surface_alt,
                    clip,
                );
                stroke_rounded(
                    &mut px,
                    content_x + rr.x * scale + 0.5 * scale,
                    ty + rr.y * scale + 0.5 * scale,
                    rr.w * scale - scale,
                    rr.h * scale - scale,
                    BTN_CORNER * scale,
                    pal.accent,
                    scale.max(1.0),
                    clip,
                );
                draw_text_centered(
                    &mut px,
                    med,
                    crate::fonts::MEDIUM,
                    12.0 * scale,
                    "重新连接",
                    content_x + rr.x * scale,
                    ty + rr.y * scale,
                    rr.w * scale,
                    rr.h * scale,
                    pal.accent,
                    clip,
                );
            }
        }

        // Scrollbar.
        if layout.scroll_max > 0.0 {
            let track_x = content_x + layout.width * scale - 6.0 * scale;
            let track_y = content_y + 8.0 * scale;
            let track_h = layout.viewport_h * scale - 16.0 * scale;
            fill_rounded(
                &mut px,
                track_x,
                track_y,
                2.0 * scale,
                track_h,
                scale,
                pal.border,
                clip,
            );
            let thumb_h = (track_h * (layout.viewport_h / layout.content_h)).max(16.0 * scale);
            let thumb_y = track_y + (track_h - thumb_h) * (self.scroll / layout.scroll_max);
            fill_rounded(
                &mut px,
                track_x,
                thumb_y,
                2.0 * scale,
                thumb_h,
                scale,
                pal.text_disabled,
                clip,
            );
        }

        self.present(&px, pw, ph);
    }

    fn draw_button(
        &self,
        px: &mut Pixmap,
        input: &LayoutInput,
        b: &ButtonLayout,
        tx: f32,
        ty: f32,
        scale: f32,
        clip: Option<&Mask>,
    ) {
        let pal = input.palette;
        let r = b.rect;
        let (x, y, w, h) = (tx + r.x * scale, ty + r.y * scale, r.w * scale, r.h * scale);
        let corner = BTN_CORNER * scale;
        let (mut fill, text) = match b.visual {
            ButtonVisual::Displayed | ButtonVisual::DisplayedFocused => {
                (pal.accent, pal.accent_text)
            }
            ButtonVisual::Focused => (lerp_color(pal.surface, pal.accent, 0.12), pal.accent),
            ButtonVisual::Pending => (pal.accent, pal.accent_text),
            ButtonVisual::Error => (lerp_color(pal.surface, pal.error, 0.12), pal.error),
            ButtonVisual::Disabled => (pal.surface_alt, pal.text_disabled),
            ButtonVisual::Normal => (
                lerp_color(pal.surface, pal.surface_alt, 0.70),
                pal.text_primary,
            ),
        };

        // Confirmation flash: brighten then settle over 220 ms.
        if input.confirm_ws.as_deref() == Some(b.id.as_str())
            && let Some(since) = input.confirm_since
        {
            let t = (input.now.duration_since(since).as_secs_f32() / 0.22).clamp(0.0, 1.0);
            let k = 0.35 * (t * std::f32::consts::PI).sin();
            fill = lerp_color(fill, Color::WHITE, k);
        }

        fill_rounded(px, x, y, w, h, corner, fill, clip);

        match b.visual {
            ButtonVisual::DisplayedFocused => stroke_rounded(
                px,
                x + 0.75 * scale,
                y + 0.75 * scale,
                w - 1.5 * scale,
                h - 1.5 * scale,
                corner - 0.75 * scale,
                lerp_color(pal.accent, pal.accent_text, 0.55),
                scale.max(1.0),
                clip,
            ),
            ButtonVisual::Focused => {
                stroke_rounded(px, x, y, w, h, corner, pal.accent, scale.max(1.0), clip)
            }
            ButtonVisual::Pending => {
                let pulse =
                    0.5 + 0.5 * (input.now.duration_since(input.epoch).as_secs_f32() * 6.0).sin();
                stroke_rounded(
                    px,
                    x,
                    y,
                    w,
                    h,
                    corner,
                    lerp_color(pal.accent, Color::WHITE, 0.5 * pulse),
                    1.5 * scale,
                    clip,
                );
            }
            ButtonVisual::Error => {
                stroke_rounded(px, x, y, w, h, corner, pal.error, scale.max(1.0), clip)
            }
            _ => {}
        }

        let font = input.fonts.get(crate::fonts::MEDIUM);
        let font_size = fit_font_size(
            font,
            crate::fonts::MEDIUM,
            &b.label,
            (r.w - 9.0).max(1.0),
            12.0,
            8.0,
        );
        draw_text_centered(
            px,
            font,
            crate::fonts::MEDIUM,
            font_size * scale,
            &b.label,
            x,
            y,
            w,
            h,
            text,
            clip,
        );

        // A short underline indicates that the workspace contains windows.
        if b.has_windows {
            fill_rounded(
                px,
                x + (w - 6.0 * scale) / 2.0,
                y + h - 3.5 * scale,
                6.0 * scale,
                1.5 * scale,
                0.75 * scale,
                if matches!(
                    b.visual,
                    ButtonVisual::Displayed
                        | ButtonVisual::DisplayedFocused
                        | ButtonVisual::Pending
                ) {
                    lerp_color(pal.accent, pal.accent_text, 0.72)
                } else {
                    pal.text_disabled
                },
                clip,
            );
        }

        // Keyboard focus ring.
        if input.kbd_ws.as_deref() == Some(b.id.as_str()) {
            stroke_rounded(
                px,
                x - 1.5 * scale,
                y - 1.5 * scale,
                w + 3.0 * scale,
                h + 3.0 * scale,
                corner + 1.5 * scale,
                pal.accent,
                1.5 * scale,
                clip,
            );
        }

        if matches!(
            input.attention,
            Some(UiChangeKind::Workspace { ref workspace_id }) if workspace_id == &b.id
        ) {
            stroke_rounded(
                px,
                x - 1.5 * scale,
                y - 1.5 * scale,
                w + 3.0 * scale,
                h + 3.0 * scale,
                corner + 1.5 * scale,
                pal.accent,
                1.5 * scale,
                clip,
            );
        }
    }

    /// Upload the framebuffer to the layered window.
    fn present(&mut self, px: &Pixmap, pw: u32, ph: u32) {
        // Debug: dump the rendered frame (requires the env var).
        if std::env::var("GLAZETRAY_DUMP").is_ok() {
            let path = std::env::temp_dir().join("glazetray_frame.raw");
            let mut raw = Vec::with_capacity((pw * ph * 4) as usize + 8);
            raw.extend_from_slice(&pw.to_le_bytes());
            raw.extend_from_slice(&ph.to_le_bytes());
            raw.extend_from_slice(px.data());
            let _ = std::fs::write(&path, &raw);
        }
        let bgra = rgba_to_bgra_premultiplied(px.data());
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: pw as i32,
                    biHeight: -(ph as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            if let Ok(bmp) = bmp {
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
                let old = SelectObject(dc, HGDIOBJ(bmp.0));
                let alpha = (self.fade.clamp(0.0, 1.0) * 255.0) as u8;
                let blend = windows::Win32::Graphics::Gdi::BLENDFUNCTION {
                    BlendOp: 0, // AC_SRC_OVER
                    BlendFlags: 0,
                    SourceConstantAlpha: alpha,
                    AlphaFormat: 1, // AC_SRC_ALPHA
                };
                let offset_px = if self.visible && self.fade_target >= 1.0 && self.fade < 1.0 {
                    let t = 1.0 - self.fade;
                    match self.edge {
                        win32::ABE_BOTTOM => 4.0 * t * self.scale,
                        win32::ABE_TOP => -4.0 * t * self.scale,
                        win32::ABE_LEFT => -4.0 * t * self.scale,
                        _ => 4.0 * t * self.scale,
                    }
                } else {
                    0.0
                };
                let pos = POINT {
                    x: self.pos.x,
                    y: self.pos.y + offset_px as i32,
                };
                let size = windows::Win32::Foundation::SIZE {
                    cx: pw as i32,
                    cy: ph as i32,
                };
                let src_pt = POINT { x: 0, y: 0 };
                windows::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
                    self.hwnd,
                    None,
                    Some(&pos),
                    Some(&size),
                    Some(dc),
                    Some(&src_pt),
                    windows::Win32::Foundation::COLORREF(0),
                    Some(&blend),
                    windows::Win32::UI::WindowsAndMessaging::UPDATE_LAYERED_WINDOW_FLAGS(0x2),
                )
                .ok();
                let _ = SelectObject(dc, old);
                let _ = DeleteObject(bmp.into());
            }
            let _ = DeleteDC(dc);
        }
    }

    // ------------------------------------------------------------------
    // Messages
    // ------------------------------------------------------------------

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_NCHITTEST => {
                let screen_x = (lparam.0 & 0xFFFF) as i16 as i32;
                let screen_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let local_x = screen_x - self.pos.x;
                let local_y = screen_y - self.pos.y;
                let inset = (SHADOW * self.scale).round() as i32;
                let content_w = (self.size_logical.0 * self.scale).round() as i32 - 2 * inset;
                let content_h = (self.size_logical.1 * self.scale).round() as i32 - 2 * inset;
                let in_content = local_x >= inset
                    && local_x < inset + content_w
                    && local_y >= inset
                    && local_y < inset + content_h;
                Some(LRESULT(if self.interactive && in_content {
                    HTCLIENT as isize
                } else {
                    HTTRANSPARENT as isize
                }))
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(action) = self.hit_test(x, y) {
                    post_action(action, self.app_hwnd);
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let amount = if delta > 0 { -40.0 } else { 40.0 };
                post_action(FlyoutAction::Scroll { delta: amount }, self.app_hwnd);
                Some(LRESULT(0))
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                post_action(FlyoutAction::DpiChanged, self.app_hwnd);
                Some(LRESULT(0))
            }
            WM_APP_HOOK_OUTSIDE => {
                post_action(FlyoutAction::Close, self.app_hwnd);
                Some(LRESULT(0))
            }
            WM_APP_HOOK_KEY => {
                let vk = wparam.0 as u32;
                let _shift = (lparam.0 & 1) != 0;
                self.handle_key(vk);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, vk: u32) {
        if vk == VK_ESCAPE.0 as u32 {
            post_action(FlyoutAction::Close, self.app_hwnd);
        } else if matches!(vk, v if v == VK_TAB.0 as u32 || v == VK_RIGHT.0 as u32 || v == VK_DOWN.0 as u32)
        {
            self.move_kbd(1);
        } else if matches!(vk, v if v == VK_LEFT.0 as u32 || v == VK_UP.0 as u32) {
            self.move_kbd(-1);
        } else if matches!(vk, v if v == VK_RETURN.0 as u32 || v == VK_SPACE.0 as u32)
            && let Some(layout) = &self.last_layout
            && let Some(idx) = self.kbd_index
            && let Some(b) = layout.buttons.get(idx)
            && b.switchable
        {
            post_action(
                FlyoutAction::FocusWorkspace {
                    workspace_id: b.id.clone(),
                    name: b.label.clone(),
                },
                self.app_hwnd,
            );
        }
    }

    /// Hit test in client coordinates (physical pixels).
    pub fn hit_test(&self, x: i32, y: i32) -> Option<FlyoutAction> {
        if !self.interactive {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let scale = self.scale;
        let lx = x as f32 / scale - SHADOW;
        let ly = y as f32 / scale - SHADOW + self.scroll.clamp(0.0, layout.scroll_max);
        if let Some(st) = &layout.status
            && let Some(rr) = &st.reconnect_rect
            && rr.contains(lx, ly)
        {
            return Some(FlyoutAction::Reconnect);
        }
        for row in &layout.rows {
            if row.dir_rect.contains(lx, ly) && row.direction.is_some() {
                return Some(FlyoutAction::ToggleDirection {
                    monitor_id: row.monitor_id.clone(),
                });
            }
            for b in &row.buttons {
                if b.rect.contains(lx, ly) {
                    return if b.switchable {
                        Some(FlyoutAction::FocusWorkspace {
                            workspace_id: b.id.clone(),
                            name: b.label.clone(),
                        })
                    } else {
                        None
                    };
                }
            }
        }
        None
    }
}

fn fonts_reg(input: &LayoutInput) -> &crate::fonts::TextEngine {
    input.fonts.get(crate::fonts::REGULAR)
}

fn fonts_med(input: &LayoutInput) -> &crate::fonts::TextEngine {
    input.fonts.get(crate::fonts::MEDIUM)
}

fn elide_text(font: &crate::fonts::TextEngine, text: &str, size: f32, max_width: f32) -> String {
    if measure_text(font, crate::fonts::REGULAR, text, size) <= max_width {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>());
        if measure_text(font, crate::fonts::REGULAR, &candidate, size) <= max_width {
            return candidate;
        }
    }
    "…".to_string()
}

fn draw_monitor_glyph(
    px: &mut Pixmap,
    cx: f32,
    cy: f32,
    size: f32,
    color: Color,
    clip: Option<&Mask>,
) {
    let w = size;
    let h = size * 0.66;
    let x = cx - w / 2.0;
    let y = cy - h * 0.72;
    stroke_rounded(
        px,
        x,
        y,
        w,
        h,
        size * 0.12,
        color,
        (size * 0.10).max(1.0),
        clip,
    );
    fill_rounded(
        px,
        cx - size * 0.08,
        y + h,
        size * 0.16,
        size * 0.17,
        0.0,
        color,
        clip,
    );
    fill_rounded(
        px,
        cx - size * 0.24,
        y + h + size * 0.15,
        size * 0.48,
        (size * 0.08).max(1.0),
        size * 0.04,
        color,
        clip,
    );
}

fn draw_layout_glyph(
    px: &mut Pixmap,
    cx: f32,
    cy: f32,
    size: f32,
    direction: TilingDirection,
    color: Color,
    clip: Option<&Mask>,
) {
    let gap = size * 0.14;
    let extent = size * 0.82;
    let corner = size * 0.10;
    match direction {
        TilingDirection::Horizontal => {
            let pane_w = (extent - gap) / 2.0;
            for x in [cx - extent / 2.0, cx + gap / 2.0] {
                fill_rounded(
                    px,
                    x,
                    cy - extent / 2.0,
                    pane_w,
                    extent,
                    corner,
                    color,
                    clip,
                );
            }
        }
        TilingDirection::Vertical => {
            let pane_h = (extent - gap) / 2.0;
            for y in [cy - extent / 2.0, cy + gap / 2.0] {
                fill_rounded(
                    px,
                    cx - extent / 2.0,
                    y,
                    extent,
                    pane_h,
                    corner,
                    color,
                    clip,
                );
            }
        }
    }
}

pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::from_rgba(
        a.red() + (b.red() - a.red()) * t,
        a.green() + (b.green() - a.green()) * t,
        a.blue() + (b.blue() - a.blue()) * t,
        a.alpha() + (b.alpha() - a.alpha()) * t,
    )
    .unwrap_or(a)
}

// ---------------------------------------------------------------------------
// Low-level hooks (outside-click close + keyboard navigation + input source
// detection for the transient flyout)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HookState {
    /// Permanent WH_MOUSE_LL hook (installed at startup, removed at exit).
    mouse: Option<isize>,
    /// WH_KEYBOARD_LL hook, active only while the flyout is visible.
    keyboard: Option<isize>,
    flyout: isize,
    tray_rect: Option<RECT>,
    nav: bool,
}

static HOOK_STATE: Mutex<HookState> = Mutex::new(HookState {
    mouse: None,
    keyboard: None,
    flyout: 0,
    tray_rect: None,
    nav: false,
});

/// Milliseconds (unix epoch) of the last mouse button-down, 0 = never.
static LAST_MOUSE_DOWN_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Install the permanent low-level mouse hook used to detect whether a focus
/// change was initiated by the mouse (clicking a window) rather than by a
/// keyboard shortcut. Must be called once from the UI thread.
pub fn install_permanent_mouse_hook() {
    let mut st = HOOK_STATE.lock().unwrap();
    if st.mouse.is_some() {
        return;
    }
    st.mouse = unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)
            .ok()
            .map(|h| h.0 as isize)
    };
    if st.mouse.is_none() {
        tracing::warn!("failed to install low-level mouse hook");
    }
}

pub fn uninstall_permanent_mouse_hook() {
    let mut st = HOOK_STATE.lock().unwrap();
    if let Some(h) = st.mouse.take() {
        unsafe {
            UnhookWindowsHookEx(HHOOK(h as *mut core::ffi::c_void)).ok();
        }
    }
}

/// Milliseconds since the last mouse button-down; `u64::MAX` if the hook has
/// never seen one. Used to distinguish mouse-driven focus changes from
/// keyboard-driven ones.
pub fn last_mouse_down_age_ms() -> u64 {
    let last = LAST_MOUSE_DOWN_MS.load(Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    now_ms().saturating_sub(last)
}

/// Install the flyout-scoped hooks (keyboard navigation + outside-click close
/// state). The permanent mouse hook stays installed.
pub fn install_hooks(flyout: HWND, tray_rect: Option<RECT>) {
    let mut st = HOOK_STATE.lock().unwrap();
    st.flyout = flyout.0 as isize;
    st.tray_rect = tray_rect;
    st.nav = true;
    if st.keyboard.is_none() {
        st.keyboard = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
                .ok()
                .map(|h| h.0 as isize)
        };
    }
}

pub fn uninstall_hooks() {
    let mut st = HOOK_STATE.lock().unwrap();
    st.nav = false;
    if let Some(h) = st.keyboard.take() {
        unsafe {
            UnhookWindowsHookEx(HHOOK(h as *mut core::ffi::c_void)).ok();
        }
    }
    st.flyout = 0;
    st.tray_rect = None;
}

unsafe extern "system" fn mouse_hook_proc(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let msg = wparam.0 as u32;
        if matches!(msg, 0x0201 | 0x0204 | 0x0207 | 0x020A | 0x020B) {
            // WM_LBUTTONDOWN / WM_RBUTTONDOWN / WM_MBUTTONDOWN /
            // WM_XBUTTONDOWN / WM_XBUTTONDBLCLK: a click is a mouse-driven
            // interaction (movement alone does not count).
            LAST_MOUSE_DOWN_MS.store(now_ms(), Ordering::Relaxed);
        }
        if matches!(msg, 0x0201 | 0x0204 | 0x0207) {
            // Outside-click close while the flyout is visible.
            let st = HOOK_STATE.lock().unwrap();
            let flyout_hwnd = HWND(st.flyout as *mut core::ffi::c_void);
            if st.flyout != 0 {
                let info = unsafe { &*(lparam.0 as *const MOUSEHOOKSTRUCT) };
                let in_flyout =
                    crate::win32::point_in_rect(info.pt, crate::win32::window_rect(flyout_hwnd));
                let in_tray = st
                    .tray_rect
                    .map(|r| crate::win32::point_in_rect(info.pt, r))
                    .unwrap_or(false);
                if !in_flyout && !in_tray {
                    unsafe {
                        PostMessageW(Some(flyout_hwnd), WM_APP_HOOK_OUTSIDE, WPARAM(0), LPARAM(0))
                            .ok();
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let st = HOOK_STATE.lock().unwrap();
        let flyout_hwnd = HWND(st.flyout as *mut core::ffi::c_void);
        if st.nav && st.flyout != 0 {
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let vk = info.vkCode;
            let alt_down = (info.flags.0 & 0x20) != 0; // LLKHF_ALTDOWN
            let is_down = matches!(wparam.0 as u32, 0x0100 | 0x0104); // WM_KEYDOWN / WM_SYSKEYDOWN
            if is_down {
                let shift = unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 };
                let escape = vk == VK_ESCAPE.0 as u32;
                let nav_key = matches!(
                    vk,
                    v if v == VK_TAB.0 as u32
                        || v == VK_UP.0 as u32
                        || v == VK_DOWN.0 as u32
                        || v == VK_LEFT.0 as u32
                        || v == VK_RIGHT.0 as u32
                        || v == VK_RETURN.0 as u32
                        || v == VK_SPACE.0 as u32
                );
                if escape {
                    unsafe {
                        PostMessageW(Some(flyout_hwnd), WM_APP_HOOK_OUTSIDE, WPARAM(0), LPARAM(0))
                            .ok();
                    }
                    return LRESULT(1);
                }
                if nav_key {
                    if vk == VK_TAB.0 as u32 && alt_down {
                        // Alt+Tab: close the flyout but do not swallow the key.
                        unsafe {
                            PostMessageW(
                                Some(flyout_hwnd),
                                WM_APP_HOOK_OUTSIDE,
                                WPARAM(0),
                                LPARAM(0),
                            )
                            .ok();
                        }
                        return unsafe { CallNextHookEx(None, n_code, wparam, lparam) };
                    }
                    unsafe {
                        PostMessageW(
                            Some(flyout_hwnd),
                            WM_APP_HOOK_KEY,
                            WPARAM(vk as usize),
                            LPARAM(if shift { 1 } else { 0 }),
                        )
                        .ok();
                    }
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, n_code, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Compute the flyout position (physical pixels). Pure function, unit-tested.
pub fn compute_position(
    anchor: Option<RECT>,
    cursor: POINT,
    size: (i32, i32),
    work: RECT,
    edge: u32,
) -> POINT {
    let (sw, sh) = size;
    let (ax, ay) = match anchor {
        Some(r) => (
            r.left + (r.right - r.left) / 2,
            r.top + (r.bottom - r.top) / 2,
        ),
        None => (cursor.x, cursor.y),
    };
    let (mut x, mut y) = match edge {
        win32::ABE_BOTTOM => {
            let anchor_right = anchor.map(|r| r.right).unwrap_or(ax + sw / 2);
            (anchor_right - sw, ay - sh)
        }
        win32::ABE_TOP => {
            let anchor_right = anchor.map(|r| r.right).unwrap_or(ax + sw / 2);
            (anchor_right - sw, ay)
        }
        win32::ABE_LEFT => (ax, ay + sh / 2),
        _ => (ax - sw, ay + sh / 2),
    };
    let margin = 4;
    if x < work.left + margin {
        x = work.left + margin;
    }
    if y < work.top + margin {
        y = work.top + margin;
    }
    if x + sw > work.right - margin {
        x = (work.right - margin - sw).max(work.left + margin);
    }
    if y + sh > work.bottom - margin {
        y = (work.bottom - margin - sh).max(work.top + margin);
    }
    POINT { x, y }
}

/// Place a keyboard-triggered status popup at the affected monitor's
/// bottom-right work-area corner, independent of the tray icon monitor.
pub fn compute_transient_position(size: (i32, i32), work: RECT) -> POINT {
    let margin = 4;
    POINT {
        x: (work.right - margin - size.0).max(work.left + margin),
        y: (work.bottom - margin - size.1).max(work.top + margin),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ConnectionState, MonitorInfo, WorkspaceInfo};

    fn workspace(
        id: &str,
        name: &str,
        displayed: bool,
        focused: bool,
        windows: usize,
    ) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.into(),
            name: name.into(),
            is_displayed: displayed,
            is_focused: focused,
            window_count: windows,
            switchable: true,
        }
    }

    fn monitor(
        id: &str,
        name: &str,
        ws: Vec<WorkspaceInfo>,
        dir: Option<TilingDirection>,
    ) -> MonitorInfo {
        MonitorInfo {
            id: id.into(),
            order: 0,
            display_name: name.into(),
            is_focused: false,
            displayed_workspace_id: ws.iter().find(|w| w.is_displayed).map(|w| w.id.clone()),
            workspaces: ws,
            direction: dir,
            device_name: None,
            rect: (0.0, 0.0, 1920.0, 1080.0),
        }
    }

    fn snap() -> AppSnapshot {
        AppSnapshot {
            connection: ConnectionState::Ready,
            glazewm_version: Some("3.9.0".into()),
            monitors: vec![
                monitor(
                    "m0",
                    "显示器 1",
                    vec![
                        workspace("1", "1", true, true, 2),
                        workspace("2", "2", false, false, 0),
                        workspace("3", "3", false, false, 1),
                    ],
                    Some(TilingDirection::Horizontal),
                ),
                monitor(
                    "m1",
                    "显示器 2",
                    vec![workspace("4", "4", true, false, 0)],
                    Some(TilingDirection::Vertical),
                ),
            ],
            focused_monitor_id: Some("m0".into()),
            focused_workspace_id: Some("1".into()),
            focused_direction: Some(TilingDirection::Horizontal),
            is_paused: false,
            last_ui_change: None,
            revision: 1,
            stale: false,
        }
    }

    fn input(_fonts: &Fonts, pal: &Palette, show_empty: bool) -> LayoutInput {
        LayoutInput {
            palette: *pal,
            fonts: std::sync::Arc::new(Fonts::load()),
            epoch: Instant::now(),
            now: Instant::now(),
            confirm_ws: None,
            confirm_since: None,
            error_ws: None,
            error_monitor: None,
            pending: None,
            kbd_ws: None,
            attention: None,
            show_empty,
            width: 320.0,
            viewport_h: 300.0,
        }
    }

    #[test]
    fn layout_rows_and_buttons() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let inp = input(&fonts, &pal, true);
        let l = compute_layout(Some(&snap()), &fonts, &inp);
        assert_eq!(l.rows.len(), 2);
        assert_eq!(l.rows[0].buttons.len(), 3);
        assert_eq!(l.rows[1].buttons.len(), 1);
        assert_eq!(l.buttons.len(), 4);
        assert_eq!(l.rows[0].direction, Some(TilingDirection::Horizontal));
        assert_eq!(l.rows[1].direction, Some(TilingDirection::Vertical));
        assert_eq!(l.rows[1].monitor_id, "m1");
        assert!(!l.header.is_paused);
        assert!(l.width >= 420.0);
        assert_eq!(l.rows[0].monitor_number, 1);
        assert!(l.rows[0].separator_y - HEADER_H >= 50.0);
        let last_separator = l.rows.last().unwrap().separator_y;
        assert!(l.content_h - last_separator >= 8.0);
        assert!(l.scroll_max >= 0.0);
    }

    #[test]
    fn paused_snapshot_is_visible_in_header() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let mut s = snap();
        s.is_paused = true;
        let mut inp = input(&fonts, &pal, true);
        inp.attention = Some(UiChangeKind::Pause { is_paused: true });
        let layout = compute_layout(Some(&s), &fonts, &inp);
        assert!(layout.header.is_paused);
        assert!(layout.header.attention);
    }

    #[test]
    fn layout_hides_empty_workspaces_when_configured() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let inp = input(&fonts, &pal, false);
        let l = compute_layout(Some(&snap()), &fonts, &inp);
        assert_eq!(l.rows[0].buttons.len(), 1); // only displayed ws "1"
    }

    #[test]
    fn layout_wraps_long_workspace_lists() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let ws: Vec<WorkspaceInfo> = (0..12)
            .map(|i| workspace(&i.to_string(), &i.to_string(), i == 0, false, 0))
            .collect();
        let s = AppSnapshot {
            monitors: vec![monitor(
                "m0",
                "显示器 1",
                ws,
                Some(TilingDirection::Horizontal),
            )],
            ..snap()
        };
        let inp = input(&fonts, &pal, true);
        let l = compute_layout(Some(&s), &fonts, &inp);
        let rows_of_buttons: usize = {
            let r = &l.rows[0];
            let mut count = 1;
            let mut prev_y = r.buttons[0].rect.y;
            for b in &r.buttons[1..] {
                if (b.rect.y - prev_y).abs() > 1.0 {
                    count += 1;
                    prev_y = b.rect.y;
                }
            }
            count
        };
        assert!(
            rows_of_buttons >= 2,
            "buttons should wrap: {rows_of_buttons}"
        );
    }

    #[test]
    fn status_shown_when_disconnected() {
        let fonts = Fonts::load();
        let pal = crate::theme::compute_palette(crate::theme::ThemeInput {
            dark: true,
            accent: None,
            use_system_accent: false,
            high_contrast: false,
        });
        let mut s = snap();
        s.connection = ConnectionState::Disconnected;
        let inp = input(&fonts, &pal, true);
        let l = compute_layout(Some(&s), &fonts, &inp);
        assert!(l.status.is_some());
        assert!(l.status.as_ref().unwrap().reconnect_rect.is_some());
    }

    #[test]
    fn placement_bottom_taskbar() {
        let anchor = RECT {
            left: 1700,
            top: 1040,
            right: 1724,
            bottom: 1080,
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let p = compute_position(
            Some(anchor),
            POINT { x: 1712, y: 1060 },
            (352, 200),
            work,
            win32::ABE_BOTTOM,
        );
        // Above the taskbar, right-aligned with the icon, clamped into work
        // (the flyout cannot fit entirely above the icon within the work area).
        assert_eq!(p.x, 1724 - 352);
        assert_eq!(p.y, work.bottom - 4 - 200);
    }

    #[test]
    fn placement_clamps_to_work_area() {
        let anchor = RECT {
            left: 0,
            top: 1040,
            right: 40,
            bottom: 1080,
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let p = compute_position(
            Some(anchor),
            POINT { x: 20, y: 1060 },
            (500, 300),
            work,
            win32::ABE_BOTTOM,
        );
        assert!(p.x >= work.left + 4);
        assert!(p.x + 500 <= work.right - 4 + 1);
        assert!(p.y >= work.top + 4);
    }

    #[test]
    fn fallback_to_cursor_when_no_anchor() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let p = compute_position(
            None,
            POINT { x: 960, y: 500 },
            (300, 150),
            work,
            win32::ABE_BOTTOM,
        );
        assert!(p.x >= work.left && p.x + 300 <= work.right);
    }

    #[test]
    fn transient_position_uses_each_monitors_bottom_right_corner() {
        let primary = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let secondary = RECT {
            left: -2560,
            top: 0,
            right: 0,
            bottom: 1400,
        };
        assert_eq!(
            compute_transient_position((460, 180), primary),
            POINT { x: 1456, y: 856 }
        );
        assert_eq!(
            compute_transient_position((460, 180), secondary),
            POINT { x: -464, y: 1216 }
        );
    }
}
