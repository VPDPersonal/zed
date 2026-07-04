/// A worktree-relative location backing a project panel view node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPanelViewPath {
    pub worktree_id: u64,
    pub path: String,
}

/// A single node in a provider-supplied project panel tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPanelViewNode {
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub is_container: bool,
    /// Absent for purely virtual nodes with no backing file (e.g. a "Dependencies" node).
    pub project_path: Option<ProjectPanelViewPath>,
}
