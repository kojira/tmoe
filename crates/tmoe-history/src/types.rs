use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AgentView {
    Worker,
    Supervisor,
    Observer,
}

impl AgentView {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentView::Worker => "worker",
            AgentView::Supervisor => "supervisor",
            AgentView::Observer => "observer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "worker" => Some(AgentView::Worker),
            "supervisor" => Some(AgentView::Supervisor),
            "observer" => Some(AgentView::Observer),
            _ => None,
        }
    }

    pub fn all() -> [AgentView; 3] {
        [AgentView::Worker, AgentView::Supervisor, AgentView::Observer]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    Planned,
    InProgress,
    Done,
    Abandoned,
}

impl FeatureStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureStatus::Planned => "planned",
            FeatureStatus::InProgress => "in_progress",
            FeatureStatus::Done => "done",
            FeatureStatus::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "planned" => Some(FeatureStatus::Planned),
            "in_progress" => Some(FeatureStatus::InProgress),
            "done" => Some(FeatureStatus::Done),
            "abandoned" => Some(FeatureStatus::Abandoned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawKind {
    Plan,
    Turn,
    Decision,
    CodeChange,
    ToolCall,
    Note,
}

impl RawKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RawKind::Plan => "plan",
            RawKind::Turn => "turn",
            RawKind::Decision => "decision",
            RawKind::CodeChange => "code_change",
            RawKind::ToolCall => "tool_call",
            RawKind::Note => "note",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(RawKind::Plan),
            "turn" => Some(RawKind::Turn),
            "decision" => Some(RawKind::Decision),
            "code_change" => Some(RawKind::CodeChange),
            "tool_call" => Some(RawKind::ToolCall),
            "note" => Some(RawKind::Note),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Feature {
    pub id: String,
    pub title: String,
    pub status: FeatureStatus,
    pub root_node_id: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawNode {
    pub id: String,
    pub feature_id: String,
    pub parent_id: Option<String>,
    pub kind: RawKind,
    pub content_hash: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSummaryNode {
    pub id: String,
    pub feature_id: String,
    pub agent: AgentView,
    pub parent_id: Option<String>,
    pub summary: String,
    pub ref_raw_ids: Vec<String>,
    pub ref_hashes: Vec<String>,
    pub level: i32,
    pub created_at: i64,
    pub updated_at: i64,
}
