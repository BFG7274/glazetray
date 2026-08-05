//! State reducer: normalizes raw IPC queries/events into `AppSnapshot`s.
//!
//! Invariants maintained:
//! - a monitor has at most one displayed workspace;
//! - at most one workspace is globally focused;
//! - the focused workspace belongs to a known monitor (may be `None` briefly);
//! - every commit bumps `revision`.
//!
//! Shapes verified against GlazeWM 3.10.1: query data is wrapped in objects
//! (`{ "monitors": [...] }`, `{ "focused": ... }`), workspaces reference their
//! monitor through `parentId`, and windows reference their workspace the same
//! way.

use std::collections::HashMap;

use serde_json::Value;

use crate::protocol::{RawContainer, direction_from_str, direction_from_value};
use crate::state::{
    AppSnapshot, ConnectionState, MonitorId, MonitorInfo, TilingDirection, UiChange, UiChangeKind,
    WorkspaceId, WorkspaceInfo,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryKind {
    Monitors,
    Workspaces,
    Focused,
    TilingDirection,
    Paused,
}

#[derive(Debug)]
pub enum ReducerInput {
    Query {
        kind: QueryKind,
        data: Option<Value>,
    },
    Event {
        name: String,
        data: Option<Value>,
    },
    Version {
        version: String,
    },
}

#[derive(Debug, Clone)]
struct MonitorCore {
    id: MonitorId,
    order: usize,
    rect: (f64, f64, f64, f64),
    device_name: Option<String>,
    /// Direction learned from events (workspaces carry their own direction).
    direction: Option<TilingDirection>,
}

#[derive(Debug, Clone)]
struct WorkspaceCore {
    id: WorkspaceId,
    name: String,
    monitor_id: Option<MonitorId>,
    is_displayed: bool,
    is_focused: bool,
    window_count: usize,
    direction: Option<TilingDirection>,
    rect: (f64, f64, f64, f64),
}

#[derive(Default)]
pub struct Reducer {
    monitors: Vec<MonitorCore>,
    workspace_order: Vec<WorkspaceId>,
    workspaces: HashMap<WorkspaceId, WorkspaceCore>,
    focused_container_id: Option<String>,
    focused_workspace_id: Option<WorkspaceId>,
    focused_direction: Option<TilingDirection>,
    is_paused: bool,
    last_ui_change: Option<UiChange>,
    version: Option<String>,
    connection: ConnectionState,
    stale: bool,
    revision: u64,
}

impl Reducer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, input: ReducerInput) -> AppSnapshot {
        match input {
            ReducerInput::Version { version } => {
                self.version = Some(version);
            }
            ReducerInput::Query { kind, data } => match kind {
                QueryKind::Monitors => self.on_monitors(data),
                QueryKind::Workspaces => self.on_workspaces(data),
                QueryKind::Focused => self.on_focused(data),
                QueryKind::TilingDirection => self.on_tiling_direction(data),
                QueryKind::Paused => self.on_paused(data),
            },
            ReducerInput::Event { name, data } => self.on_event(&name, data),
        }
        self.snapshot()
    }

    pub fn set_connection(&mut self, state: ConnectionState) -> AppSnapshot {
        self.connection = state.clone();
        if matches!(state, ConnectionState::Disconnected) {
            self.stale = true;
        }
        self.snapshot()
    }

    /// After a successful sync the state is fresh again.
    pub fn mark_ready(&mut self) -> AppSnapshot {
        self.stale = false;
        self.connection = ConnectionState::Ready;
        self.snapshot()
    }

    pub fn snapshot(&self) -> AppSnapshot {
        let focused_monitor_id = self
            .focused_workspace_id
            .as_ref()
            .and_then(|id| self.workspaces.get(id))
            .and_then(|w| w.monitor_id.clone());

        let monitors = self
            .monitors
            .iter()
            .map(|m| {
                let mut workspaces: Vec<WorkspaceInfo> = self
                    .workspace_order
                    .iter()
                    .filter_map(|id| self.workspaces.get(id))
                    .filter(|w| w.monitor_id.as_deref() == Some(m.id.as_str()))
                    .map(|w| WorkspaceInfo {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        is_displayed: w.is_displayed,
                        is_focused: w.is_focused,
                        window_count: w.window_count,
                        switchable: can_encode_workspace_name(&w.name),
                    })
                    .collect();
                // Order by workspace number, not by GlazeWM's report (creation) order.
                workspaces.sort_by(|a, b| workspace_name_cmp(&a.name, &b.name));
                let displayed = workspaces
                    .iter()
                    .find(|w| w.is_displayed)
                    .map(|w| w.id.clone());
                // Constraint check: at most one displayed workspace per monitor.
                let displayed_count = workspaces.iter().filter(|w| w.is_displayed).count();
                if displayed_count > 1 {
                    tracing::warn!(
                        monitor = %m.id,
                        count = displayed_count,
                        "constraint violated: multiple displayed workspaces"
                    );
                }
                let direction = workspaces
                    .iter()
                    .find(|w| w.is_displayed)
                    .and_then(|w| self.workspaces.get(&w.id))
                    .and_then(|w| w.direction)
                    .or(m.direction);
                MonitorInfo {
                    id: m.id.clone(),
                    order: m.order,
                    display_name: format!("显示器 {}", m.order + 1),
                    is_focused: focused_monitor_id.as_deref() == Some(m.id.as_str()),
                    displayed_workspace_id: displayed,
                    workspaces,
                    direction,
                    device_name: m.device_name.clone(),
                    rect: m.rect,
                }
            })
            .collect();

        AppSnapshot {
            connection: self.connection.clone(),
            glazewm_version: self.version.clone(),
            monitors,
            focused_monitor_id,
            focused_workspace_id: self.focused_workspace_id.clone(),
            focused_direction: self
                .focused_workspace_id
                .as_ref()
                .and_then(|id| self.workspaces.get(id))
                .and_then(|w| w.direction)
                .or(self.focused_direction),
            is_paused: self.is_paused,
            last_ui_change: self.last_ui_change.clone(),
            revision: self.revision,
            stale: self.stale,
        }
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    fn on_monitors(&mut self, data: Option<Value>) {
        let items = data_array_by_key(data, "monitors");
        self.monitors.clear();
        for (i, item) in items.iter().enumerate() {
            let c: RawContainer = serde_json::from_value(item.clone()).unwrap_or_default();
            let id = c.id.clone().unwrap_or_else(|| format!("monitor-{i}"));
            self.monitors.push(MonitorCore {
                id,
                order: i,
                rect: c.rect(),
                device_name: c.device_name.clone(),
                direction: c.tiling_direction.as_deref().and_then(direction_from_str),
            });
            // Monitor children are its displayed workspace(s).
            if let Some(children) = &c.children {
                for ws in children {
                    self.upsert_workspace(ws, Some(c.id.clone().unwrap_or_default()));
                }
            }
            // Older payloads embed the current workspace in `workspace`.
            if let Some(ws) = c.workspace.as_deref() {
                let mid = c.id.clone().unwrap_or_else(|| format!("monitor-{i}"));
                self.upsert_workspace(ws, Some(mid));
            }
        }
        self.revision += 1;
    }

    fn on_workspaces(&mut self, data: Option<Value>) {
        let items = data_array_by_key(data, "workspaces");
        self.workspace_order.clear();
        self.workspaces.clear();
        for item in items.iter() {
            let c: RawContainer = serde_json::from_value(item.clone()).unwrap_or_default();
            let id = c.id.clone().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            // Workspaces are children of monitors in the 3.10 tree.
            let monitor_id = c.parent_id.clone().or_else(|| {
                c.monitor
                    .as_deref()
                    .and_then(|m| m.id.clone())
                    .or_else(|| self.monitor_containing(c.rect()))
            });
            self.ensure_monitor(&monitor_id);
            self.workspace_order.push(id.clone());
            self.workspaces.insert(
                id.clone(),
                WorkspaceCore {
                    id,
                    name: c.workspace_name(),
                    monitor_id,
                    is_displayed: c.is_displayed.unwrap_or(false),
                    is_focused: c.has_focus.unwrap_or(false),
                    window_count: c.children.as_ref().map(|v| v.len()).unwrap_or(0),
                    direction: c.tiling_direction.as_deref().and_then(direction_from_str),
                    rect: c.rect(),
                },
            );
        }
        if self.focused_workspace_id.is_some()
            && !self
                .workspaces
                .contains_key(self.focused_workspace_id.as_ref().unwrap())
        {
            self.focused_workspace_id = None;
        }
        self.revision += 1;
    }

    fn on_focused(&mut self, data: Option<Value>) {
        let Some(v) = data else {
            self.focused_container_id = None;
            self.focused_workspace_id = None;
            self.revision += 1;
            return;
        };
        // 3.10: `{ "focused": <container> }`; legacy: the container itself.
        let container = v.get("focused").cloned().unwrap_or(v);
        if container.is_null() {
            self.focused_container_id = None;
            self.focused_workspace_id = None;
            self.revision += 1;
            return;
        }
        let c: RawContainer = serde_json::from_value(container).unwrap_or_default();
        self.apply_focused_container(&c);
        self.revision += 1;
    }

    fn apply_focused_container(&mut self, c: &RawContainer) {
        self.focused_container_id = c.id.clone();
        let id = match c.typ.as_deref() {
            Some("workspace") => c.id.clone(),
            Some("monitor") => c
                .workspace
                .as_deref()
                .and_then(|w| w.id.clone())
                .or_else(|| {
                    let mid = c.id.clone()?;
                    self.workspaces
                        .values()
                        .find(|w| w.monitor_id.as_deref() == Some(mid.as_str()) && w.is_displayed)
                        .map(|w| w.id.clone())
                }),
            _ => {
                // Window or split: its parent is the workspace.
                c.parent_id.clone().or_else(|| {
                    let (x, y, w, h) = c.rect();
                    self.workspace_containing(x + w / 2.0, y + h / 2.0)
                })
            }
        };
        self.focused_workspace_id = id.clone();
        let focused_monitor_id = id.as_ref().and_then(|workspace_id| {
            self.workspaces
                .get(workspace_id)
                .and_then(|workspace| workspace.monitor_id.clone())
        });
        for ws in self.workspaces.values_mut() {
            let is_focused = id.as_ref() == Some(&ws.id);
            ws.is_focused = is_focused;
            // A focus event is the authoritative signal when switching to an
            // existing workspace. Keep the displayed marker in sync on that
            // monitor even when the event omits the full workspace payload.
            if let Some(monitor_id) = focused_monitor_id.as_deref()
                && ws.monitor_id.as_deref() == Some(monitor_id)
            {
                ws.is_displayed = is_focused;
            }
        }
    }

    fn on_tiling_direction(&mut self, data: Option<Value>) {
        if let Some(v) = data
            && let Some(d) = direction_from_value(&v)
        {
            self.focused_direction = Some(d);
            // 3.10: the direction belongs to `directionContainer`.
            if let Some(dc) = v.get("directionContainer") {
                let c: RawContainer = serde_json::from_value(dc.clone()).unwrap_or_default();
                let ws_id = match c.typ.as_deref() {
                    Some("workspace") => c.id.clone(),
                    _ => c.parent_id.clone(),
                };
                if let Some(ws_id) = ws_id
                    && let Some(ws) = self.workspaces.get_mut(&ws_id)
                {
                    ws.direction = Some(d);
                }
            }
            if let Some(fid) = &self.focused_workspace_id
                && let Some(ws) = self.workspaces.get_mut(fid)
            {
                ws.direction = Some(d);
            }
        }
        self.revision += 1;
    }

    fn on_paused(&mut self, data: Option<Value>) {
        if let Some(is_paused) = data.and_then(|v| v.as_bool()) {
            self.is_paused = is_paused;
        }
        self.revision += 1;
    }

    fn mark_ui_change(&mut self, kind: UiChangeKind) {
        self.last_ui_change = Some(UiChange {
            serial: self.revision,
            kind,
        });
    }

    // ------------------------------------------------------------------
    // Events
    // ------------------------------------------------------------------

    fn on_event(&mut self, name: &str, data: Option<Value>) {
        match name {
            "focus_changed" => {
                if let Some(c) = extract_container(&data, &["focusedContainer", "current"]) {
                    let previous_container_id = self.focused_container_id.clone();
                    let previous_workspace_id = self.focused_workspace_id.clone();
                    let current_container_id = c.id.clone();
                    let focused_workspace_container = c.typ.as_deref() == Some("workspace");
                    self.apply_focused_container(&c);
                    self.revision += 1;
                    let workspace_changed = previous_workspace_id != self.focused_workspace_id;
                    // GlazeWM can emit duplicate focus events while cycling windows.
                    // Only a repeated workspace-container focus is meaningful here;
                    // that is how an empty current workspace reports reactivation.
                    let repeated_workspace_focus = focused_workspace_container
                        && previous_container_id.is_some()
                        && previous_container_id == current_container_id;
                    if (workspace_changed || repeated_workspace_focus)
                        && let Some(workspace_id) = self.focused_workspace_id.clone()
                    {
                        self.mark_ui_change(UiChangeKind::Workspace { workspace_id });
                    }
                }
            }
            "focused_container_moved" => {
                // `move-workspace` emits this event before `workspace_updated`.
                // The focused container is the moved workspace, including its
                // new parent monitor. Keep the state and transient location
                // current as soon as that payload is available.
                if let Some(ws) = extract_container(&data, &["focusedContainer", "container"])
                    && ws.typ.as_deref() == Some("workspace")
                {
                    let workspace_id = ws_id(&ws);
                    let old_monitor_id = self
                        .workspaces
                        .get(&workspace_id)
                        .and_then(|workspace| workspace.monitor_id.clone());
                    self.upsert_workspace(&ws, None);
                    let new_monitor_id = self
                        .workspaces
                        .get(&workspace_id)
                        .and_then(|workspace| workspace.monitor_id.clone());
                    self.revision += 1;
                    if old_monitor_id
                        .zip(new_monitor_id)
                        .is_some_and(|(old, new)| old != new)
                    {
                        self.mark_ui_change(UiChangeKind::Workspace { workspace_id });
                    }
                }
            }
            "workspace_activated" => {
                if let Some(ws) =
                    extract_container(&data, &["activatedWorkspace", "workspace", "container"])
                {
                    let workspace_id = ws_id(&ws);
                    self.upsert_workspace(&ws, None);
                    self.revision += 1;
                    self.mark_ui_change(UiChangeKind::Workspace { workspace_id });
                }
            }
            "workspace_deactivated" => {
                // 3.10 emits only `deactivatedId`/`deactivatedName` because
                // the empty workspace has already been detached from its monitor.
                let workspace_id = data
                    .as_ref()
                    .and_then(|value| value.get("deactivatedId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        extract_container(&data, &["workspace", "container"])
                            .and_then(|workspace| workspace.id)
                    });
                if let Some(workspace_id) = workspace_id {
                    self.remove_workspace(&workspace_id);
                    self.revision += 1;
                }
            }
            "workspace_updated" => {
                if let Some(ws) =
                    extract_container(&data, &["updatedWorkspace", "workspace", "container"])
                {
                    let workspace_id = ws_id(&ws);
                    let old_monitor_id = self
                        .workspaces
                        .get(&workspace_id)
                        .and_then(|workspace| workspace.monitor_id.clone());
                    self.upsert_workspace(&ws, None);
                    let new_monitor_id = self
                        .workspaces
                        .get(&workspace_id)
                        .and_then(|workspace| workspace.monitor_id.clone());
                    self.revision += 1;
                    // A workspace moved between monitors is a visible
                    // workspace change even when focus stays on the same
                    // workspace. `workspace_updated` is also the fallback
                    // for WM versions that omit the moved-container payload.
                    if old_monitor_id
                        .zip(new_monitor_id)
                        .is_some_and(|(old, new)| old != new)
                    {
                        self.mark_ui_change(UiChangeKind::Workspace { workspace_id });
                    }
                }
            }
            "monitor_added" | "monitor_updated" => {
                if let Some(m) = extract_container(&data, &["monitor", "container"]) {
                    self.upsert_monitor(&m);
                    self.revision += 1;
                }
            }
            "monitor_removed" => {
                if let Some(id) =
                    extract_container(&data, &["monitor", "container"]).and_then(|m| m.id)
                {
                    self.monitors.retain(|m| m.id != id);
                    self.workspaces
                        .retain(|_, w| w.monitor_id.as_deref() != Some(id.as_str()));
                    self.workspace_order
                        .retain(|wid| self.workspaces.contains_key(wid));
                    if self
                        .focused_workspace_id
                        .as_ref()
                        .and_then(|f| self.workspaces.get(f))
                        .and_then(|w| w.monitor_id.as_deref())
                        == Some(id.as_str())
                    {
                        self.focused_workspace_id = None;
                    }
                    self.revision += 1;
                }
            }
            "tiling_direction_changed" => {
                if let Some(c) =
                    extract_container(&data, &["directionContainer", "workspace", "monitor"])
                {
                    let direction = data
                        .as_ref()
                        .and_then(|v| v.get("newTilingDirection"))
                        .and_then(|v| v.as_str())
                        .and_then(direction_from_str)
                        .or_else(|| c.tiling_direction.as_deref().and_then(direction_from_str));
                    let workspace_id = match c.typ.as_deref() {
                        Some("workspace") => c.id.clone(),
                        _ => c.parent_id.clone(),
                    };
                    if let Some(direction) = direction {
                        self.focused_direction = Some(direction);
                        let workspace_id = workspace_id
                            .filter(|id| self.workspaces.contains_key(id))
                            .or_else(|| self.focused_workspace_id.clone());
                        let monitor_id = workspace_id.as_ref().and_then(|workspace_id| {
                            self.workspaces.get_mut(workspace_id).and_then(|core| {
                                core.direction = Some(direction);
                                core.monitor_id.clone()
                            })
                        });
                        self.revision += 1;
                        if let Some(monitor_id) = monitor_id {
                            self.mark_ui_change(UiChangeKind::Direction { monitor_id });
                        }
                    }
                }
            }
            "window_managed" => {
                self.adjust_window_count(&data, 1);
            }
            "window_unmanaged" => {
                self.adjust_window_count(&data, -1);
            }
            "pause_changed" => {
                if let Some(is_paused) = data
                    .as_ref()
                    .and_then(|v| v.get("isPaused"))
                    .and_then(|v| v.as_bool())
                {
                    self.is_paused = is_paused;
                    self.revision += 1;
                    self.mark_ui_change(UiChangeKind::Pause { is_paused });
                }
            }
            "user_config_changed" | "application_exiting" | "binding_modes_changed" => {}
            other => {
                tracing::warn!(event = other, "ignoring unknown IPC event");
            }
        }
    }

    fn adjust_window_count(&mut self, data: &Option<Value>, delta: i32) {
        let Some(c) = extract_container(data, &["window", "container"]) else {
            return;
        };
        // The window's parent is its workspace (3.10 tree).
        let wid = c
            .parent_id
            .clone()
            .or_else(|| self.workspace_containing_rect(c.rect()));
        if let Some(wid) = wid
            && let Some(core) = self.workspaces.get_mut(&wid)
        {
            core.window_count = (core.window_count as i64 + delta as i64).max(0) as usize;
            self.revision += 1;
        }
        // Unknown containment: the IPC layer triggers a calibration query.
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn upsert_monitor(&mut self, c: &RawContainer) {
        let id = c.id.clone().unwrap_or_default();
        if id.is_empty() {
            return;
        }
        match self.monitors.iter_mut().find(|m| m.id == id) {
            Some(m) => {
                m.rect = c.rect();
                if let Some(dn) = &c.device_name {
                    m.device_name = Some(dn.clone());
                }
                if let Some(d) = c.tiling_direction.as_deref().and_then(direction_from_str) {
                    m.direction = Some(d);
                }
            }
            None => {
                self.monitors.push(MonitorCore {
                    order: self.monitors.len(),
                    id,
                    rect: c.rect(),
                    device_name: c.device_name.clone(),
                    direction: c.tiling_direction.as_deref().and_then(direction_from_str),
                });
            }
        }
        if let Some(ws) = c.workspace.as_deref() {
            self.upsert_workspace(ws, Some(c.id.clone().unwrap_or_default()));
        }
    }

    fn upsert_workspace(&mut self, c: &RawContainer, forced_monitor: Option<MonitorId>) {
        let id = c.id.clone().unwrap_or_default();
        if id.is_empty() {
            return;
        }
        let monitor_id = forced_monitor
            .or_else(|| c.parent_id.clone())
            .or_else(|| c.monitor.as_deref().and_then(|m| m.id.clone()))
            .or_else(|| self.monitor_containing(c.rect()));
        self.ensure_monitor(&monitor_id);
        let is_focused = c.has_focus.unwrap_or(false);
        let is_displayed = c.is_displayed.unwrap_or(false);
        match self.workspaces.get_mut(&id) {
            Some(core) => {
                core.name = c.workspace_name();
                core.monitor_id = monitor_id.clone();
                core.is_displayed = is_displayed;
                core.is_focused = is_focused;
                core.window_count = c
                    .children
                    .as_ref()
                    .map(|v| v.len())
                    .unwrap_or(core.window_count);
                if let Some(d) = c.tiling_direction.as_deref().and_then(direction_from_str) {
                    core.direction = Some(d);
                }
                core.rect = c.rect();
            }
            None => {
                self.workspace_order.push(id.clone());
                self.workspaces.insert(
                    id.clone(),
                    WorkspaceCore {
                        id: id.clone(),
                        name: c.workspace_name(),
                        monitor_id: monitor_id.clone(),
                        is_displayed,
                        is_focused,
                        window_count: c.children.as_ref().map(|v| v.len()).unwrap_or(0),
                        direction: c.tiling_direction.as_deref().and_then(direction_from_str),
                        rect: c.rect(),
                    },
                );
            }
        }
        if is_focused {
            self.focused_workspace_id = Some(id.clone());
            for ws in self.workspaces.values_mut() {
                ws.is_focused = ws.id == id;
            }
        }
        // Constraint: a monitor displays at most one workspace. When a
        // workspace becomes displayed, clear the flag on every other
        // workspace of the same monitor (handles delayed, duplicated or
        // missing `workspace_deactivated` events idempotently).
        if is_displayed && let Some(mid) = &monitor_id {
            for ws in self.workspaces.values_mut() {
                if ws.id != id && ws.monitor_id.as_deref() == Some(mid.as_str()) {
                    ws.is_displayed = false;
                }
            }
        }
    }

    fn ensure_monitor(&mut self, id: &Option<MonitorId>) {
        if let Some(mid) = id
            && !self.monitors.iter().any(|m| &m.id == mid)
        {
            self.monitors.push(MonitorCore {
                id: mid.clone(),
                order: self.monitors.len(),
                rect: (0.0, 0.0, 0.0, 0.0),
                device_name: None,
                direction: None,
            });
        }
    }

    fn remove_workspace(&mut self, id: &str) {
        self.workspaces.remove(id);
        self.workspace_order
            .retain(|workspace_id| workspace_id != id);
        if self.focused_workspace_id.as_deref() == Some(id) {
            self.focused_workspace_id = None;
        }
        if self.focused_container_id.as_deref() == Some(id) {
            self.focused_container_id = None;
        }
    }

    fn monitor_containing(&self, (x, y, w, h): (f64, f64, f64, f64)) -> Option<MonitorId> {
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        self.monitors
            .iter()
            .find(|m| {
                let (mx, my, mw, mh) = m.rect;
                cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
            })
            .map(|m| m.id.clone())
            .or_else(|| self.monitors.first().map(|m| m.id.clone()))
    }

    fn workspace_containing(&self, cx: f64, cy: f64) -> Option<WorkspaceId> {
        let contains = |w: &WorkspaceCore| {
            let (x, y, ww, hh) = w.rect;
            cx >= x && cx < x + ww && cy >= y && cy < y + hh
        };
        // Prefer the displayed workspace (windows live there); fall back to
        // any workspace (covers duplicate rects across virtual desktops).
        self.workspaces
            .values()
            .find(|w| w.is_displayed && contains(w))
            .map(|w| w.id.clone())
            .or_else(|| {
                self.workspaces
                    .values()
                    .find(|w| contains(w))
                    .map(|w| w.id.clone())
            })
    }

    fn workspace_containing_rect(&self, (x, y, w, h): (f64, f64, f64, f64)) -> Option<WorkspaceId> {
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        self.workspace_containing(x + w / 2.0, y + h / 2.0)
    }
}

/// Extract the data array wrapped under `key` (3.10 wraps query results in
/// objects); falls back to treating the value itself as the array.
fn data_array_by_key(data: Option<Value>, key: &str) -> Vec<Value> {
    let Some(v) = data else {
        return Vec::new();
    };
    if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
        return arr.clone();
    }
    match v {
        Value::Array(items) => items,
        other => vec![other],
    }
}

fn ws_id(c: &RawContainer) -> WorkspaceId {
    c.id.clone().unwrap_or_default()
}

/// Extract a container from an event payload that may wrap it under any of
/// the given keys or carry it directly.
fn extract_container(data: &Option<Value>, keys: &[&str]) -> Option<RawContainer> {
    let v = data.as_ref()?;
    let mut candidate = v.clone();
    for key in keys {
        if let Some(x) = v.get(*key) {
            candidate = x.clone();
            break;
        }
    }
    let c: RawContainer = serde_json::from_value(candidate).ok()?;
    if c.id.is_none() && c.typ.is_none() {
        return None;
    }
    Some(c)
}

/// Whether a workspace name can be safely encoded into a GlazeWM CLI command.
///
/// GlazeWM splits command arguments on whitespace and does not honor shell
/// quoting, so names containing whitespace or quote characters cannot be
/// passed reliably. These are shown in the flyout but their switch action is
/// disabled. Commands are never executed through a shell.
pub fn can_encode_workspace_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|c| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '$' | ';' | '|' | '&')
        })
}

/// Compare workspace names the way users expect: pure numeric names by value
/// ("2" before "10"), numeric names before non-numeric ones, and everything
/// else lexicographically. This keeps workspaces ordered by their number
/// (e.g. 1, 2, 3) regardless of the order GlazeWM reports them in (which
/// follows creation order).
fn workspace_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(na), Ok(nb)) => na.cmp(&nb),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(r: &Reducer) -> AppSnapshot {
        r.snapshot()
    }

    fn monitors_json() -> Value {
        serde_json::json!({
            "monitors": [
                {"type":"monitor","id":"m0","x":-3840,"y":0,"width":3840,"height":2160,
                 "deviceName":"\\\\.\\DISPLAY2","scaleFactor":1.5,
                 "children":[{"type":"workspace","id":"ws1","name":"1","isDisplayed":true,"hasFocus":true,
                              "tilingDirection":"horizontal","x":-3828,"y":42,"width":3816,"height":2034,
                              "parentId":"m0","children":[{"type":"window","id":"w1","parentId":"ws1"}]}]},
                {"type":"monitor","id":"m1","x":0,"y":0,"width":3840,"height":2160,
                 "deviceName":"\\\\.\\DISPLAY1","scaleFactor":1.5,
                 "children":[{"type":"workspace","id":"ws3","name":"3","isDisplayed":true,"hasFocus":false,
                              "tilingDirection":"horizontal","x":12,"y":42,"width":3816,"height":2034,
                              "parentId":"m1","children":[]}]}
            ]
        })
    }

    fn workspaces_json() -> Value {
        serde_json::json!({
            "workspaces": [
                {"type":"workspace","id":"ws1","name":"1","parentId":"m0",
                 "isDisplayed":true,"hasFocus":true,"tilingDirection":"horizontal",
                 "x":-3828,"y":42,"width":3816,"height":2034,
                 "children":[{"type":"window","id":"w1","parentId":"ws1"}]},
                {"type":"workspace","id":"ws2","name":"2","parentId":"m0",
                 "isDisplayed":false,"hasFocus":false,"tilingDirection":"vertical",
                 "x":-3828,"y":42,"width":3816,"height":2034,"children":[]},
                {"type":"workspace","id":"ws3","name":"3","parentId":"m1",
                 "isDisplayed":true,"hasFocus":false,"tilingDirection":"horizontal",
                 "x":12,"y":42,"width":3816,"height":2034,"children":[]}
            ]
        })
    }

    fn r() -> Reducer {
        let mut r = Reducer::new();
        r.apply(ReducerInput::Query {
            kind: QueryKind::Monitors,
            data: Some(monitors_json()),
        });
        r.apply(ReducerInput::Query {
            kind: QueryKind::Workspaces,
            data: Some(workspaces_json()),
        });
        r.apply(ReducerInput::Query {
            kind: QueryKind::Focused,
            data: Some(serde_json::json!({"focused": {"type":"workspace","id":"ws1"}})),
        });
        r
    }

    #[test]
    fn distinguishes_displayed_from_focused() {
        let s = snap(&r());
        assert_eq!(s.monitors.len(), 2);
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws1"));
        assert!(s.monitors[0].is_focused);
        assert_eq!(s.monitors[1].displayed_workspace_id.as_deref(), Some("ws3"));
        assert!(!s.monitors[1].is_focused);
        assert_eq!(s.focused_workspace_id.as_deref(), Some("ws1"));
        assert_eq!(s.focused_monitor_id.as_deref(), Some("m0"));
        assert_eq!(s.focused_direction, Some(TilingDirection::Horizontal));
        assert_eq!(s.monitors[0].direction, Some(TilingDirection::Horizontal));
        assert_eq!(s.monitors[0].device_name.as_deref(), Some(r"\\.\DISPLAY2"));
    }

    #[test]
    fn window_count_per_workspace() {
        let s = snap(&r());
        assert_eq!(s.monitors[0].workspaces[0].window_count, 1);
        assert_eq!(s.monitors[0].workspaces[1].window_count, 0);
        assert_eq!(s.monitors[0].workspaces[0].id, "ws1");
    }

    #[test]
    fn focus_change_event_updates_focus() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "eventType": "focus_changed",
                "focusedContainer": {"type":"workspace","id":"ws2"}
            })),
        });
        assert_eq!(s.focused_workspace_id.as_deref(), Some("ws2"));
        assert_eq!(s.focused_monitor_id.as_deref(), Some("m0"));
        assert_eq!(s.focused_direction, Some(TilingDirection::Vertical));
        assert!(matches!(
            s.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Workspace { workspace_id }) if workspace_id == "ws2"
        ));
    }

    #[test]
    fn focus_change_event_updates_displayed_workspace() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"workspace","id":"ws2"}
            })),
        });

        let ws1 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws1")
            .unwrap();
        let ws2 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws2")
            .unwrap();
        assert!(!ws1.is_displayed);
        assert!(ws2.is_displayed);
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn focused_window_promotes_parent_workspace_to_displayed() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"window","id":"w2","parentId":"ws2"}
            })),
        });

        assert_eq!(s.focused_workspace_id.as_deref(), Some("ws2"));
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn repeated_workspace_focus_surfaces_workspace_without_window_noise() {
        let mut repeated = r();
        let s = repeated.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"workspace","id":"ws1"}
            })),
        });
        assert!(matches!(
            s.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Workspace { workspace_id }) if workspace_id == "ws1"
        ));

        let mut ordinary = r();
        ordinary.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"window","id":"new-window","parentId":"ws1"}
            })),
        });
        let s = ordinary.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"window","id":"new-window","parentId":"ws1"}
            })),
        });
        assert!(s.last_ui_change.is_none());
    }

    #[test]
    fn focus_window_resolves_via_parent_id() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {"type":"window","id":"w1","parentId":"ws1","title":"t"}
            })),
        });
        assert_eq!(s.focused_workspace_id.as_deref(), Some("ws1"));
    }

    #[test]
    fn focus_on_second_monitor_switches_focused_monitor() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "focus_changed".into(),
            data: Some(serde_json::json!({"focusedContainer": {"type":"workspace","id":"ws3"}})),
        });
        assert_eq!(s.focused_monitor_id.as_deref(), Some("m1"));
        assert!(s.monitors[1].is_focused);
        assert!(!s.monitors[0].is_focused);
    }

    #[test]
    fn workspace_move_between_monitors_surfaces_ui_change() {
        let moved_workspace = || ReducerInput::Event {
            name: "focused_container_moved".into(),
            data: Some(serde_json::json!({
                "focusedContainer": {
                    "type": "workspace",
                    "id": "ws1",
                    "name": "1",
                    "parentId": "m1",
                    "isDisplayed": true,
                    "hasFocus": true,
                    "tilingDirection": "horizontal",
                    "x": 12,
                    "y": 42,
                    "width": 3816,
                    "height": 2034,
                    "children": [{"type": "window", "id": "w1", "parentId": "ws1"}]
                }
            })),
        };
        let updated_workspace = || ReducerInput::Event {
            name: "workspace_updated".into(),
            data: Some(serde_json::json!({
                "updatedWorkspace": {
                    "type": "workspace",
                    "id": "ws1",
                    "name": "1",
                    "parentId": "m1",
                    "isDisplayed": true,
                    "hasFocus": true,
                    "tilingDirection": "horizontal",
                    "x": 12,
                    "y": 42,
                    "width": 3816,
                    "height": 2034,
                    "children": [{"type": "window", "id": "w1", "parentId": "ws1"}]
                }
            })),
        };

        let mut r = r();
        let first = r.apply(moved_workspace());
        assert!(matches!(
            first.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Workspace { workspace_id }) if workspace_id == "ws1"
        ));
        assert_eq!(first.focused_monitor_id.as_deref(), Some("m1"));
        assert!(!first.monitors[0].workspaces.iter().any(|ws| ws.id == "ws1"));
        assert!(first.monitors[1].workspaces.iter().any(|ws| ws.id == "ws1"));

        // GlazeWM emits workspace_updated after focused_container_moved for
        // the same move. It updates state but must not flash a second time.
        let first_serial = first.last_ui_change.as_ref().unwrap().serial;
        let second = r.apply(updated_workspace());
        assert_eq!(second.last_ui_change.as_ref().unwrap().serial, first_serial);
    }

    #[test]
    fn workspace_updated_detects_monitor_migration_without_move_event() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "workspace_updated".into(),
            data: Some(serde_json::json!({
                "updatedWorkspace": {
                    "type": "workspace",
                    "id": "ws1",
                    "name": "1",
                    "parentId": "m1",
                    "isDisplayed": true,
                    "hasFocus": true,
                    "x": 12,
                    "y": 42,
                    "width": 3816,
                    "height": 2034
                }
            })),
        });
        assert!(matches!(
            s.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Workspace { workspace_id }) if workspace_id == "ws1"
        ));
        assert_eq!(s.focused_monitor_id.as_deref(), Some("m1"));
    }

    #[test]
    fn tiling_direction_query_updates_direction_container() {
        let mut r = r();
        let s = r.apply(ReducerInput::Query {
            kind: QueryKind::TilingDirection,
            data: Some(serde_json::json!({
                "tilingDirection": "vertical",
                "directionContainer": {"type":"workspace","id":"ws1"}
            })),
        });
        assert_eq!(s.focused_direction, Some(TilingDirection::Vertical));
        let _ws1 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws1")
            .unwrap();
        assert_eq!(s.monitors[0].direction, Some(TilingDirection::Vertical));
    }

    #[test]
    fn paused_query_and_event_update_snapshot() {
        let mut r = r();
        let queried = r.apply(ReducerInput::Query {
            kind: QueryKind::Paused,
            data: Some(serde_json::json!(true)),
        });
        assert!(queried.is_paused);
        assert!(queried.last_ui_change.is_none());

        let resumed = r.apply(ReducerInput::Event {
            name: "pause_changed".into(),
            data: Some(serde_json::json!({
                "eventType": "pause_changed",
                "isPaused": false
            })),
        });
        assert!(!resumed.is_paused);
        assert!(matches!(
            resumed.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Pause { is_paused: false })
        ));
    }

    #[test]
    fn repeated_workspace_activation_gets_a_new_ui_change_serial() {
        let mut r = r();
        let event = || ReducerInput::Event {
            name: "workspace_activated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_activated",
                "activatedWorkspace": {"type":"workspace","id":"ws1","isDisplayed":true,
                                       "hasFocus":true,"parentId":"m0"}
            })),
        };
        let first = r.apply(event());
        let second = r.apply(event());
        assert!(
            second.last_ui_change.as_ref().unwrap().serial
                > first.last_ui_change.as_ref().unwrap().serial
        );
    }

    #[test]
    fn workspace_activated_sets_displayed() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "workspace_activated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_activated",
                "workspace": {"type":"workspace","id":"ws2","isDisplayed":true,"hasFocus":true,
                              "parentId":"m0"}
            })),
        });
        let ws2 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws2")
            .unwrap();
        assert!(ws2.is_displayed);
        assert!(ws2.is_focused);
        assert_eq!(s.focused_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn activating_workspace_clears_previous_displayed() {
        // ws1 is displayed on m0; activating ws2 must clear ws1's flag even
        // without a deactivated event for ws1.
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "workspace_activated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_activated",
                "workspace": {"type":"workspace","id":"ws2","isDisplayed":true,"hasFocus":true,
                              "parentId":"m0"}
            })),
        });
        let ws1 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws1")
            .unwrap();
        let ws2 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws2")
            .unwrap();
        assert!(!ws1.is_displayed, "ws1 must stop being displayed");
        assert!(!ws1.is_focused);
        assert!(ws2.is_displayed);
        assert_eq!(
            s.monitors[0]
                .workspaces
                .iter()
                .filter(|w| w.is_displayed)
                .count(),
            1,
            "exactly one displayed workspace on the monitor"
        );
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn duplicate_activation_is_idempotent() {
        let mut r = r();
        for _ in 0..3 {
            r.apply(ReducerInput::Event {
                name: "workspace_activated".into(),
                data: Some(serde_json::json!({
                    "eventType": "workspace_activated",
                    "workspace": {"type":"workspace","id":"ws2","isDisplayed":true,"hasFocus":true,
                                  "parentId":"m0"}
                })),
            });
        }
        let s = r.snapshot();
        assert_eq!(
            s.monitors[0]
                .workspaces
                .iter()
                .filter(|w| w.is_displayed)
                .count(),
            1
        );
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn official_deactivated_event_removes_detached_workspace() {
        // Order: activate ws2, THEN the old deactivated event for ws1 arrives
        // late. The final state must still show ws2 as displayed.
        let mut r = r();
        r.apply(ReducerInput::Event {
            name: "workspace_activated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_activated",
                "workspace": {"type":"workspace","id":"ws2","isDisplayed":true,"hasFocus":true,
                              "parentId":"m0"}
            })),
        });
        r.apply(ReducerInput::Event {
            name: "workspace_deactivated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_deactivated",
                "deactivatedId": "ws1",
                "deactivatedName": "1"
            })),
        });
        let s = r.snapshot();
        assert!(!s.monitors[0].workspaces.iter().any(|w| w.id == "ws1"));
        assert!(
            s.monitors[0]
                .workspaces
                .iter()
                .find(|w| w.id == "ws2")
                .unwrap()
                .is_displayed
        );
        assert_eq!(s.monitors[0].displayed_workspace_id.as_deref(), Some("ws2"));
    }

    #[test]
    fn recreating_empty_workspace_does_not_duplicate_its_name() {
        let mut r = r();
        let activate = |id: &str| ReducerInput::Event {
            name: "workspace_activated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_activated",
                "activatedWorkspace": {
                    "type": "workspace", "id": id, "name": "4",
                    "isDisplayed": true, "hasFocus": true, "parentId": "m1"
                }
            })),
        };

        r.apply(activate("empty-workspace-a"));
        r.apply(ReducerInput::Event {
            name: "workspace_deactivated".into(),
            data: Some(serde_json::json!({
                "eventType": "workspace_deactivated",
                "deactivatedId": "empty-workspace-a",
                "deactivatedName": "4"
            })),
        });
        let s = r.apply(activate("empty-workspace-b"));

        let named_four: Vec<_> = s.monitors[1]
            .workspaces
            .iter()
            .filter(|workspace| workspace.name == "4")
            .collect();
        assert_eq!(named_four.len(), 1);
        assert_eq!(named_four[0].id, "empty-workspace-b");
    }

    #[test]
    fn tiling_direction_changed_event_updates_workspace() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "tiling_direction_changed".into(),
            data: Some(serde_json::json!({
                "eventType": "tiling_direction_changed",
                "directionContainer": {"type":"split","id":"s1","parentId":"ws1"},
                "newTilingDirection": "vertical"
            })),
        });
        let _ws1 = s.monitors[0]
            .workspaces
            .iter()
            .find(|w| w.id == "ws1")
            .unwrap();
        assert_eq!(s.monitors[0].direction, Some(TilingDirection::Vertical));
        assert!(matches!(
            s.last_ui_change.as_ref().map(|change| &change.kind),
            Some(UiChangeKind::Direction { monitor_id }) if monitor_id == "m0"
        ));
    }

    #[test]
    fn monitor_removed_cleans_workspaces() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "monitor_removed".into(),
            data: Some(serde_json::json!({"monitor": {"type":"monitor","id":"m1"}})),
        });
        assert_eq!(s.monitors.len(), 1);
        assert!(s.monitors[0].workspaces.iter().all(|w| w.id != "ws3"));
    }

    #[test]
    fn unknown_events_do_not_crash() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "something_new".into(),
            data: Some(serde_json::json!({"weird": [1,2,3]})),
        });
        assert_eq!(s.monitors.len(), 2);
    }

    #[test]
    fn window_managed_adjusts_count() {
        let mut r = r();
        let s = r.apply(ReducerInput::Event {
            name: "window_managed".into(),
            data: Some(serde_json::json!({
                "eventType": "window_managed",
                "window": {"type":"window","id":"w2","parentId":"ws1"}
            })),
        });
        assert_eq!(s.monitors[0].workspaces[0].window_count, 2);
    }

    #[test]
    fn empty_focus_clears_focus() {
        let mut r = r();
        let s = r.apply(ReducerInput::Query {
            kind: QueryKind::Focused,
            data: Some(serde_json::json!({"focused": null})),
        });
        assert_eq!(s.focused_workspace_id, None);
    }

    #[test]
    fn revision_monotonic() {
        let mut r = Reducer::new();
        let mut prev = 0u64;
        for _ in 0..5 {
            let s = r.apply(ReducerInput::Event {
                name: "workspace_updated".into(),
                data: Some(serde_json::json!({"workspace": {"type":"workspace","id":"x"}})),
            });
            assert!(s.revision > prev);
            prev = s.revision;
        }
    }

    #[test]
    fn command_name_encoding() {
        assert!(can_encode_workspace_name("1"));
        assert!(can_encode_workspace_name("a-b_c"));
        assert!(can_encode_workspace_name("工作区1"));
        assert!(!can_encode_workspace_name(""));
        assert!(!can_encode_workspace_name("1 2"));
        assert!(!can_encode_workspace_name("a\"b"));
        assert!(!can_encode_workspace_name("a;b"));
        assert!(!can_encode_workspace_name("a\tb"));
    }

    #[test]
    fn workspaces_are_sorted_by_number_not_creation_order() {
        // GlazeWM reports workspaces in creation order: workspace "5" was
        // created before "1", "2" and "10". The snapshot must still order
        // them numerically: 1, 2, 5, 10.
        let mut r = Reducer::new();
        r.apply(ReducerInput::Query {
            kind: QueryKind::Monitors,
            data: Some(serde_json::json!({
                "monitors": [{"type":"monitor","id":"m0","x":0,"y":0,
                               "width":3840,"height":2160}]
            })),
        });
        r.apply(ReducerInput::Query {
            kind: QueryKind::Workspaces,
            data: Some(serde_json::json!({
                "workspaces": [
                    {"type":"workspace","id":"ws5","name":"5","parentId":"m0"},
                    {"type":"workspace","id":"ws10","name":"10","parentId":"m0"},
                    {"type":"workspace","id":"ws1","name":"1","parentId":"m0"},
                    {"type":"workspace","id":"ws2","name":"2","parentId":"m0"}
                ]
            })),
        });
        let s = snap(&r);
        let names: Vec<_> = s.monitors[0]
            .workspaces
            .iter()
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(names, ["1", "2", "5", "10"]);
    }

    #[test]
    fn workspaces_created_by_events_are_sorted_too() {
        // New workspaces arriving via events (upsert path) must also land in
        // numeric order instead of being appended in creation order.
        let mut r = Reducer::new();
        r.apply(ReducerInput::Query {
            kind: QueryKind::Monitors,
            data: Some(serde_json::json!({
                "monitors": [{"type":"monitor","id":"m0","x":0,"y":0,
                               "width":3840,"height":2160}]
            })),
        });
        let upsert = |r: &mut Reducer, id: &str, name: &str| {
            r.apply(ReducerInput::Event {
                name: "workspace_updated".into(),
                data: Some(serde_json::json!({
                    "workspace": {"type":"workspace","id":id,"name":name,"parentId":"m0"}
                })),
            });
        };
        upsert(&mut r, "ws3", "3");
        upsert(&mut r, "ws1", "1");
        upsert(&mut r, "ws2", "2");
        let s = snap(&r);
        let names: Vec<_> = s.monitors[0]
            .workspaces
            .iter()
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(names, ["1", "2", "3"]);
    }

    #[test]
    fn workspace_name_cmp_numeric_before_lexicographic() {
        assert_eq!(workspace_name_cmp("2", "10"), std::cmp::Ordering::Less);
        assert_eq!(workspace_name_cmp("10", "2"), std::cmp::Ordering::Greater);
        assert_eq!(workspace_name_cmp("1", "a"), std::cmp::Ordering::Less);
        assert_eq!(workspace_name_cmp("a", "1"), std::cmp::Ordering::Greater);
        assert_eq!(workspace_name_cmp("a", "b"), std::cmp::Ordering::Less);
        assert_eq!(workspace_name_cmp("1", "1"), std::cmp::Ordering::Equal);
    }
}
