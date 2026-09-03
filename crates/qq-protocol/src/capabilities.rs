//! Versioned capability discovery.
//!
//! Clients learn what a server supports from this document rather than from
//! provider names or trial commands. The response is deliberately tolerant of
//! unknown fields so a client built against an older revision can still read
//! the parts it knows; inbound requests stay strict.

use serde::{Deserialize, Serialize};

use crate::{
    AgentProfileId, ApprovalMode, BudgetLimitKind, ContentHash, InputPartKind, ToolExposure,
    WorkspaceId, sessions::SessionCommandKind,
};

/// Schema version of [`ServerCapabilities`]. Bumped when the meaning of an
/// existing field changes; additive fields do not bump it.
pub const CAPABILITIES_VERSION: u16 = 1;

/// Asks for the capability document. Workspace-scoped sections (profiles) are
/// included only when a workspace is named, because they derive from that
/// workspace's layered configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub version: u16,
    pub protocol_version: u16,
    pub server_version: String,
    /// Input part kinds the server accepts in prompts and steering.
    pub input_parts: Vec<InputPartKind>,
    /// Every session command the server routes.
    pub commands: Vec<SessionCommandKind>,
    pub steering: SteeringCapabilities,
    pub limits: LimitCapabilities,
    /// Approval decision tags the server accepts.
    pub approvals: Vec<String>,
    /// Approval modes a session may select.
    pub approval_modes: Vec<ApprovalMode>,
    /// Present only when the request named a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<AgentProfileSummary>>,
    /// Bounds every plan's tool catalog observes, and how large external
    /// catalogs are disclosed.
    pub tools: ToolCapabilities,
    /// Present only when the request named a workspace: the external tool
    /// hosts and skill index of that workspace's default plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_tools: Option<WorkspaceToolCapabilities>,
}

/// Catalog and exposure bounds. Clients learn from here that a run may see
/// `select_tools` and `load_skill` rather than inferring it from tool names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilities {
    pub max_catalog_tools: u32,
    pub max_tool_schema_bytes: u64,
    pub max_catalog_schema_bytes: u64,
    /// External catalogs at or under both thresholds are sent whole.
    pub full_exposure_tools: u32,
    pub full_exposure_schema_bytes: u64,
    /// Most external tools one run may pin under progressive exposure.
    pub max_pinned_tools: u32,
    pub max_indexed_skills: u32,
    /// Prefixes external tool names carry, by host kind.
    pub external_prefixes: Vec<String>,
}

/// One workspace's external hosts and skills as its default plan sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceToolCapabilities {
    pub catalog_digest: ContentHash,
    pub exposure: ToolExposure,
    pub hosts: Vec<ToolHostSummary>,
    pub excluded_tools: u32,
    pub skills: SkillCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolHostSummary {
    pub name: String,
    pub generation: u64,
    pub tool_count: u32,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCapabilities {
    pub digest: ContentHash,
    pub indexed: u32,
    /// Documents the model may load itself through `load_skill`.
    pub disclosed: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringCapabilities {
    /// Steering input is injected at the next model/tool boundary.
    pub boundary: bool,
    /// Steering may also interrupt the in-flight provider stream or tool.
    pub interrupt: bool,
    /// Most steering messages pending per run.
    pub max_pending_per_run: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitCapabilities {
    /// Budget kinds `RunLimits` may impose and settle with a typed outcome.
    pub supported: Vec<BudgetLimitKind>,
    pub max_request_bytes: u64,
    pub max_event_bytes: u64,
    pub max_input_parts: u16,
    pub max_input_text_bytes: u64,
    pub max_input_file_parts: u16,
    pub max_input_file_bytes: u64,
    pub max_pending_prompts: u16,
    /// Hard ceiling on `RunLimits::max_children`.
    pub max_children: u16,
    /// Hard ceiling on `RunLimits::max_concurrent_children`.
    pub max_concurrent_children: u16,
    /// Deepest sub-agent nesting the runtime executes.
    pub max_child_depth: u16,
    pub max_correlation_entries: u16,
}

/// A configured profile a session may select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfileSummary {
    pub id: AgentProfileId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub approval_mode: ApprovalMode,
    /// Set when the profile is declared by an agent pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<PackSummary>,
}

/// The agent pack behind a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackSummary {
    pub id: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_tolerate_unknown_response_fields_but_not_request_fields() {
        let json = r#"{
            "version": 1,
            "protocol_version": 14,
            "server_version": "0.1.0",
            "input_parts": ["text", "workspace_file", "hologram"],
            "commands": ["submit_prompt"],
            "steering": {"boundary": true, "interrupt": true, "max_pending_per_run": 4, "telepathy": true},
            "limits": {
                "supported": ["duration"],
                "max_request_bytes": 1, "max_event_bytes": 1, "max_input_parts": 1,
                "max_input_text_bytes": 1, "max_input_file_parts": 1, "max_input_file_bytes": 1,
                "max_pending_prompts": 1, "max_children": 1, "max_concurrent_children": 1,
                "max_child_depth": 1, "max_correlation_entries": 1
            },
            "approvals": ["deny"],
            "approval_modes": ["auto"],
            "tools": {
                "max_catalog_tools": 512, "max_tool_schema_bytes": 16384,
                "max_catalog_schema_bytes": 1048576, "full_exposure_tools": 24,
                "full_exposure_schema_bytes": 32768, "max_pinned_tools": 32,
                "max_indexed_skills": 64, "external_prefixes": ["mcp__"]
            },
            "future_section": {"anything": 1}
        }"#;
        // Unknown enum values inside arrays are still rejected: a client that
        // cannot name a kind must not silently pretend it understands it.
        assert!(serde_json::from_str::<ServerCapabilities>(json).is_err());
        let known = json.replace(", \"hologram\"", "");
        let decoded: ServerCapabilities = serde_json::from_str(&known).unwrap();
        assert_eq!(decoded.protocol_version, 14);
        assert!(decoded.profiles.is_none());
        assert!(decoded.workspace_tools.is_none());
        assert_eq!(decoded.tools.max_pinned_tools, 32);
        assert!(decoded.steering.interrupt);

        assert!(serde_json::from_str::<CapabilitiesRequest>(r#"{"workspace":"x"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<CapabilitiesRequest>("{}").unwrap(),
            CapabilitiesRequest::default()
        );
    }
}
