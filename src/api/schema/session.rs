use serde::{Deserialize, Serialize};

use super::agents::AgentInfo;
use super::panes::{PaneInfo, PaneLayoutSnapshot};
use super::tabs::TabInfo;
use super::workspaces::WorkspaceInfo;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSnapshot {
    pub version: String,
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<String>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    pub layouts: Vec<PaneLayoutSnapshot>,
    pub agents: Vec<AgentInfo>,
    /// The server incarnation this snapshot was taken from. Absent means
    /// UNAVAILABLE, never "the empty epoch".
    ///
    /// Carried here as WELL as on `AgentInfo` because the token is otherwise
    /// observable only when an agent exists — and the staleness case it
    /// exists to detect is precisely the one where the agent may be gone. A
    /// consumer must be able to ask "which incarnation am I talking to?"
    /// without one.
    ///
    /// This is the API snapshot (`src/api/schema/session.rs`), which is a
    /// RESPONSE type. It is deliberately NOT `persist::SessionSnapshot`
    /// (`src/persist/snapshot.rs`), the type written to disk by
    /// `persist::io::save` — putting a non-persisted incarnation token into a
    /// persisted structure would manufacture the exact stale-snapshot forgery
    /// this field exists to make detectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_epoch: Option<String>,
}
