//! Application-level state types shared between the IPC reducer, the tray and
//! the flyout. The UI never touches raw IPC DTOs.

pub type WorkspaceId = String;
pub type MonitorId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TilingDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiChangeKind {
    Workspace { workspace_id: WorkspaceId },
    Direction { monitor_id: MonitorId },
    Pause { is_paused: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiChange {
    pub serial: u64,
    pub kind: UiChangeKind,
}

impl TilingDirection {
    pub fn label(&self) -> &'static str {
        match self {
            TilingDirection::Horizontal => "水平",
            TilingDirection::Vertical => "垂直",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting {
        attempt: u32,
    },
    Synchronizing,
    Ready,
    Degraded {
        reason: String,
    },
}

/// Immutable snapshot produced by the reducer. `revision` increments on every
/// commit so the UI can cheaply detect changes.
#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub connection: ConnectionState,
    pub glazewm_version: Option<String>,
    pub monitors: Vec<MonitorInfo>,
    pub focused_monitor_id: Option<MonitorId>,
    pub focused_workspace_id: Option<WorkspaceId>,
    pub focused_direction: Option<TilingDirection>,
    pub is_paused: bool,
    /// Last event that should surface the temporary status flyout.
    pub last_ui_change: Option<UiChange>,
    pub revision: u64,
    /// True when the snapshot is the last-known-good one kept while the
    /// connection is down.
    #[allow(dead_code)]
    pub stale: bool,
}

impl AppSnapshot {
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        matches!(self.connection, ConnectionState::Ready)
    }
}

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub order: usize,
    pub display_name: String,
    pub is_focused: bool,
    pub displayed_workspace_id: Option<WorkspaceId>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub direction: Option<TilingDirection>,
    /// Raw `hMonitor` value as reported by GlazeWM (used for name resolution).
    pub device_name: Option<String>,
    /// Physical-pixel rect (x, y, w, h) for fallback name matching.
    pub rect: (f64, f64, f64, f64),
}

#[derive(Clone, Debug)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub is_displayed: bool,
    pub is_focused: bool,
    pub window_count: usize,
    /// Whether a `focus --workspace <name>` command can be safely encoded.
    pub switchable: bool,
}

/// A user-initiated operation that is still awaiting confirmation.
/// (Runtime bookkeeping — deadlines, command ids — lives in the app.)
#[derive(Clone, Debug, PartialEq)]
pub enum PendingAction {
    FocusWorkspace {
        workspace_id: WorkspaceId,
        name: String,
    },
    ToggleDirection {
        monitor_id: MonitorId,
    },
    FocusThenToggle {
        workspace_id: WorkspaceId,
        name: String,
        monitor_id: MonitorId,
    },
    #[allow(dead_code)]
    Reconnect,
}

#[allow(dead_code)]
impl PendingAction {
    /// Workspace id this action targets, if any (for button visual state).
    #[allow(dead_code)]
    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            PendingAction::FocusWorkspace { workspace_id, .. }
            | PendingAction::FocusThenToggle { workspace_id, .. } => Some(workspace_id),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn monitor_id(&self) -> Option<&MonitorId> {
        match self {
            PendingAction::ToggleDirection { monitor_id }
            | PendingAction::FocusThenToggle { monitor_id, .. } => Some(monitor_id),
            _ => None,
        }
    }
}
