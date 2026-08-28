use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupTarget {
    pub group_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupCreateParams {
    pub name: String,
    /// Initial members; groups always have at least one member. Workspaces
    /// belonging to a worktree space are expanded to the whole space.
    pub workspace_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupRenameParams {
    pub group_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupAssignParams {
    pub group_id: String,
    pub workspace_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupUnassignParams {
    pub workspace_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupSetCollapsedParams {
    pub group_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupInfo {
    pub group_id: String,
    pub name: String,
    pub collapsed: bool,
    /// Member workspace ids in sidebar order.
    pub workspace_ids: Vec<String>,
}
