//! Minimal, tolerant DTOs for the GlazeWM WebSocket IPC protocol.
//!
//! Only the fields the UI actually needs are modelled. Every field is
//! optional/defaulted so unknown fields and shape variations across GlazeWM
//! versions cannot break parsing.
//!
//! Verified against GlazeWM 3.10.1:
//! - queries respond with `data: { "monitors": [...], ... }` (object wrappers);
//! - events arrive as `{ "messageType": "event_subscription", "data": { "eventType": ... } }`;
//! - responses echo `clientMessage`, and commands return `subjectContainerId`.

use serde::Deserialize;
use serde_json::Value;

use crate::state::TilingDirection;

/// Response to a query or command:
/// `{ "messageType": "client_response", "clientMessage": ..., "data": ..., "success": bool, "error": ... }`
#[derive(Debug, Deserialize)]
pub struct WsResponse {
    #[serde(default)]
    #[allow(dead_code)]
    pub message_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub client_message: Option<String>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

/// Subscription event. GlazeWM 3.10 uses
/// `{ "messageType": "event_subscription", "data": { "eventType": ..., ... } }`;
/// older payloads used `{ "event": ..., "data": ... }`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsEvent {
    #[serde(default)]
    #[allow(dead_code)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
}

impl WsEvent {
    #[allow(dead_code)]
    pub fn event_name(&self) -> Option<String> {
        if let Some(name) = &self.event {
            return Some(name.clone());
        }
        self.data
            .as_ref()
            .and_then(|d| d.get("eventType"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// A container in the GlazeWM tree: monitor, workspace, window or split. All
/// fields are optional; monitors and workspaces both carry geometry.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawContainer {
    #[serde(rename = "type")]
    pub typ: Option<String>,
    pub id: Option<String>,
    /// Parent container id: workspaces are children of monitors; windows and
    /// splits are children of workspaces.
    pub parent_id: Option<String>,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[allow(dead_code)]
    pub title: Option<String>,
    pub is_displayed: Option<bool>,
    pub has_focus: Option<bool>,
    #[allow(dead_code)]
    pub has_tiling_window: Option<bool>,
    pub display_name: Option<String>,
    pub name: Option<String>,
    pub tiling_direction: Option<String>,
    pub children: Option<Vec<RawContainer>>,
    pub monitor: Option<Box<RawContainer>>,
    pub workspace: Option<Box<RawContainer>>,
    #[allow(dead_code)]
    pub is_primary: Option<bool>,
    #[allow(dead_code)]
    pub scale_factor: Option<f64>,
    #[allow(dead_code)]
    pub h_monitor: Option<String>,
    /// Monitor device name (e.g. `\\.\DISPLAY1`) — used for display names.
    pub device_name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RawRect {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

impl RawContainer {
    pub fn rect(&self) -> (f64, f64, f64, f64) {
        (self.x, self.y, self.width, self.height)
    }

    /// The name shown to users: display name, config name, or the id.
    pub fn workspace_name(&self) -> String {
        self.display_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.name.clone().filter(|s| !s.is_empty()))
            .or_else(|| self.id.clone())
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn is_workspace(&self) -> bool {
        self.typ.as_deref() == Some("workspace")
    }

    #[allow(dead_code)]
    pub fn is_monitor(&self) -> bool {
        self.typ.as_deref() == Some("monitor")
    }
}

pub fn direction_from_str(s: &str) -> Option<TilingDirection> {
    match s.to_ascii_lowercase().as_str() {
        "horizontal" => Some(TilingDirection::Horizontal),
        "vertical" => Some(TilingDirection::Vertical),
        _ => None,
    }
}

/// Parse a tiling direction out of `query tiling-direction` data, which is
/// `{ "tilingDirection": "horizontal", "directionContainer": {...} }`.
pub fn direction_from_value(v: &Value) -> Option<TilingDirection> {
    if let Some(s) = v.as_str() {
        return direction_from_str(s);
    }
    v.get("tilingDirection")
        .or_else(|| v.get("direction"))
        .and_then(|d| d.as_str())
        .and_then(direction_from_str)
}

/// Parse a rect out of arbitrary JSON (used for `workArea`, `rect`, ...).
#[allow(dead_code)]
pub fn rect_from_value(v: &Value) -> Option<(f64, f64, f64, f64)> {
    let r: RawRect = serde_json::from_value(v.clone()).ok()?;
    Some((r.x, r.y, r.width, r.height))
}

pub fn error_message(v: &Option<Value>) -> Option<String> {
    let v = v.as_ref()?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_container_with_camel_case_fields() {
        let json = r#"{
            "type": "workspace",
            "id": "workspace-1",
            "parentId": "monitor-0",
            "name": "1",
            "displayName": "主屏",
            "isDisplayed": true,
            "hasFocus": true,
            "hasTilingWindow": true,
            "tilingDirection": "horizontal",
            "x": 0, "y": 0, "width": 1920, "height": 1040,
            "children": [ {"type": "window", "id": "w1", "title": "t"} ],
            "monitor": { "type": "monitor", "id": "monitor-0", "hMonitor": "0x000000000001002E",
                         "x": 0, "y": 0, "width": 1920, "height": 1080 }
        }"#;
        let c: RawContainer = serde_json::from_str(json).unwrap();
        assert!(c.is_workspace());
        assert_eq!(c.id.as_deref(), Some("workspace-1"));
        assert_eq!(c.parent_id.as_deref(), Some("monitor-0"));
        assert_eq!(c.workspace_name(), "主屏");
        assert_eq!(c.is_displayed, Some(true));
        assert_eq!(c.has_focus, Some(true));
        assert_eq!(c.children.as_ref().unwrap().len(), 1);
        assert_eq!(c.monitor.as_ref().unwrap().h_monitor.as_deref(), Some("0x000000000001002E"));
    }

    #[test]
    fn tolerates_unknown_fields_and_missing_optional_fields() {
        let json = r#"{"type":"window","id":"w1","someFutureField":{"a":1},"title":"x"}"#;
        let c: RawContainer = serde_json::from_str(json).unwrap();
        assert_eq!(c.id.as_deref(), Some("w1"));
        assert_eq!(c.workspace_name(), "w1");
        assert_eq!(c.rect(), (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn direction_parsing_accepts_string_and_object() {
        assert_eq!(direction_from_value(&serde_json::json!("horizontal")), Some(TilingDirection::Horizontal));
        assert_eq!(direction_from_value(&serde_json::json!("VERTICAL")), Some(TilingDirection::Vertical));
        assert_eq!(
            direction_from_value(&serde_json::json!({"tilingDirection": "vertical"})),
            Some(TilingDirection::Vertical)
        );
        assert_eq!(direction_from_value(&serde_json::json!({"foo": 1})), None);
        assert_eq!(direction_from_str("bogus"), None);
    }

    #[test]
    fn event_parsing_3_10_shape() {
        let e: WsEvent = serde_json::from_str(
            r#"{"messageType":"event_subscription","data":{"eventType":"focus_changed","focusedContainer":{"type":"workspace","id":"w2"}}}"#,
        )
        .unwrap();
        assert_eq!(e.event_name().as_deref(), Some("focus_changed"));
    }

    #[test]
    fn event_parsing_legacy_shape() {
        let e: WsEvent = serde_json::from_str(
            r#"{"event":"focus_changed","data":{"current":{"type":"workspace","id":"w2"}}}"#,
        )
        .unwrap();
        assert_eq!(e.event_name().as_deref(), Some("focus_changed"));
    }
}
