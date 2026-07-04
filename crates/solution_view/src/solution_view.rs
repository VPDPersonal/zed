//! A native project-panel view provider that renders a C#/.NET "Solution Explorer" tree
//! (`.sln` → projects → dependencies + source files), registered with the
//! [`project_panel::project_panel_view_provider::ProjectPanelViewProviderRegistry`].
//!
//! This is a native (built-in) provider rather than a WASM extension: the extension guest API
//! cannot enumerate or read a project's files (it only exposes worktree ids), whereas a native
//! provider receives an `Entity<Project>` with full `fs`/worktree access. The pure parsing logic
//! lives in [`parse`] with no gpui/fs dependency so it can back a WASM extension later.

pub mod parse;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, AsyncApp, Entity, SharedString, Subscription, Task};
use project::{Project, ProjectPath};
use project_panel::project_panel_view_provider::{
    ProjectPanelViewNode, ProjectPanelViewProvider, ProjectPanelViewProviderRegistry,
};
use settings::WorktreeId;
use util::rel_path::RelPath;

use crate::parse::{parse_csproj, parse_solution};

/// The id under which this provider registers; referenced by a `project_panel_views` entry
/// `{ "name": "Solution", "provider": "solution" }` in settings.
const PROVIDER_ID: &str = "solution";

/// Registers the Solution Explorer provider. Call once at startup (see `zed::main`).
pub fn init(cx: &mut App) {
    let registry = ProjectPanelViewProviderRegistry::default_global(cx);
    registry.update(cx, |registry, cx| {
        registry.register(PROVIDER_ID.into(), Arc::new(SolutionProvider), cx);
    });
}

struct SolutionProvider;

/// The kind of a node, encoded into (and decoded from) [`ProjectPanelViewNode::id`] since
/// `children` only receives the node itself. Format: `kind|worktree_id|rel_path`.
enum NodeKind {
    /// A `.sln` file; children are its projects. `rel` is the `.sln` path.
    Solution,
    /// A `.csproj` project; children are its `Dependencies` node plus its source root. `rel` is the `.csproj` path.
    Project,
    /// The virtual "Dependencies" node of a project; children are package/project references. `rel` is the `.csproj` path.
    Dependencies,
    /// A directory in a project's source tree; children are its entries. `rel` is the directory path.
    Directory,
}

impl NodeKind {
    fn tag(&self) -> &'static str {
        match self {
            NodeKind::Solution => "sln",
            NodeKind::Project => "proj",
            NodeKind::Dependencies => "deps",
            NodeKind::Directory => "dir",
        }
    }
}

fn encode_id(kind: NodeKind, worktree_id: WorktreeId, rel: &str) -> SharedString {
    format!("{}|{}|{}", kind.tag(), worktree_id.to_proto(), rel).into()
}

/// Decodes a container node id into `(tag, worktree_id, rel_path)`. Leaf nodes
/// (packages/references/files) are never passed to `children`, so only container tags decode here.
fn decode_id(id: &str) -> Option<(&str, WorktreeId, &str)> {
    let mut parts = id.splitn(3, '|');
    let tag = parts.next()?;
    let worktree_id = WorktreeId::from_proto(parts.next()?.parse().ok()?);
    let rel = parts.next()?;
    Some((tag, worktree_id, rel))
}

/// Everything needed to touch a worktree's files off the main thread, captured up front.
#[derive(Clone)]
struct WorktreeAccess {
    worktree_id: WorktreeId,
    abs_path: Arc<Path>,
    fs: Arc<dyn Fs>,
    entity: Entity<worktree::Worktree>,
}

impl WorktreeAccess {
    /// Reads the single visible worktree's access handle, or `None` when zero/many are visible
    /// (the provider, like glob views, only applies to a single-worktree project).
    fn resolve(project: &Entity<Project>, cx: &mut AsyncApp) -> Option<Self> {
        cx.update(|cx| {
            let project = project.read(cx);
            let fs = project.fs().clone();
            let mut worktrees = project.visible_worktrees(cx);
            let entity = worktrees.next()?;
            if worktrees.next().is_some() {
                return None;
            }
            let worktree = entity.read(cx);
            Some(WorktreeAccess {
                worktree_id: worktree.id(),
                abs_path: worktree.abs_path(),
                fs,
                entity: entity.clone(),
            })
        })
    }

    fn absolutize(&self, rel: &str) -> Result<PathBuf> {
        let rel = RelPath::unix(rel).with_context(|| format!("invalid relative path: {rel}"))?;
        Ok(self.abs_path.join(rel.as_std_path()))
    }

    async fn load(&self, rel: &str) -> Result<String> {
        let path = self.absolutize(rel)?;
        self.fs.load(&path).await
    }

    /// Lists the direct children of `parent_rel`, skipping build output dirs. Returns
    /// `(file_name, is_dir, rel_path)` tuples read from the already-scanned worktree snapshot.
    fn list_dir(&self, parent_rel: &str, cx: &mut AsyncApp) -> Vec<(String, bool, String)> {
        cx.update(|cx| {
            let Ok(parent) = RelPath::unix(parent_rel) else {
                return Vec::new();
            };
            let snapshot = self.entity.read(cx).snapshot();
            let mut entries = Vec::new();
            for entry in snapshot.child_entries(parent) {
                let Some(name) = entry.path.file_name() else {
                    continue;
                };
                if entry.is_dir() && is_build_output_dir(name) {
                    continue;
                }
                entries.push((
                    name.to_string(),
                    entry.is_dir(),
                    entry.path.as_unix_str().to_string(),
                ));
            }
            entries
        })
    }
}

fn is_build_output_dir(name: &str) -> bool {
    matches!(name, "bin" | "obj")
}

fn project_path(worktree_id: WorktreeId, rel: &str) -> Option<ProjectPath> {
    Some(ProjectPath {
        worktree_id,
        path: RelPath::unix(rel).ok()?.into(),
    })
}

/// The directory containing `file_rel`, as a `/`-separated string (empty string for the root).
fn parent_dir(file_rel: &str) -> String {
    match file_rel.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Joins a `/`-separated directory with a project-relative child path.
fn join_rel(dir: &str, child: &str) -> String {
    if dir.is_empty() {
        child.to_string()
    } else {
        format!("{dir}/{child}")
    }
}

fn container(id: SharedString, title: impl Into<SharedString>) -> ProjectPanelViewNode {
    ProjectPanelViewNode {
        id,
        title: title.into(),
        icon: None,
        is_container: true,
        project_path: None,
    }
}

fn leaf(
    id: SharedString,
    title: impl Into<SharedString>,
    project_path: Option<ProjectPath>,
) -> ProjectPanelViewNode {
    ProjectPanelViewNode {
        id,
        title: title.into(),
        icon: None,
        is_container: false,
        project_path,
    }
}

impl SolutionProvider {
    /// Root nodes: one per `.sln` at the worktree root, or (fallback) top-level `.csproj` projects.
    async fn root_nodes_impl(
        &self,
        access: WorktreeAccess,
        cx: &mut AsyncApp,
    ) -> Vec<ProjectPanelViewNode> {
        let worktree_id = access.worktree_id;
        let root_entries = access.list_dir("", cx);
        let solutions: Vec<_> = root_entries
            .iter()
            .filter(|(name, is_dir, _)| !is_dir && has_extension(name, "sln"))
            .collect();
        if !solutions.is_empty() {
            return solutions
                .into_iter()
                .map(|(name, _, rel)| {
                    container(encode_id(NodeKind::Solution, worktree_id, rel), name.clone())
                })
                .collect();
        }
        // Fallback: no solution file, surface top-level projects directly.
        root_entries
            .iter()
            .filter(|(name, is_dir, _)| !is_dir && has_extension(name, "csproj"))
            .map(|(name, _, rel)| project_node(worktree_id, name, rel))
            .collect()
    }

    async fn children_impl(
        &self,
        parent: ProjectPanelViewNode,
        access: WorktreeAccess,
        cx: &mut AsyncApp,
    ) -> Result<Vec<ProjectPanelViewNode>> {
        let Some((tag, worktree_id, rel)) = decode_id(&parent.id) else {
            return Ok(Vec::new());
        };
        match tag {
            "sln" => {
                let text = access.load(rel).await?;
                let base = parent_dir(rel);
                Ok(parse_solution(&text)
                    .into_iter()
                    .map(|project| {
                        let csproj_rel = join_rel(&base, &project.csproj_rel);
                        project_node(worktree_id, &project.name, &csproj_rel)
                    })
                    .collect())
            }
            "proj" => {
                let mut nodes = vec![container(
                    encode_id(NodeKind::Dependencies, worktree_id, rel),
                    "Dependencies",
                )];
                nodes.extend(dir_nodes(worktree_id, &parent_dir(rel), &access, cx));
                Ok(nodes)
            }
            "deps" => {
                let text = access.load(rel).await?;
                let info = parse_csproj(&text);
                let base = parent_dir(rel);
                let mut nodes = Vec::new();
                for (name, version) in info.package_references {
                    let title = match version {
                        Some(version) => format!("{name} ({version})"),
                        None => name.clone(),
                    };
                    // Qualify leaf ids with the owning `.csproj` so the same package referenced by
                    // two projects yields distinct element ids in the rendered list.
                    nodes.push(leaf(format!("pkg|{rel}|{name}").into(), title, None));
                }
                for reference in info.project_references {
                    let title = reference
                        .rsplit('/')
                        .next()
                        .unwrap_or(&reference)
                        .trim_end_matches(".csproj")
                        .to_string();
                    let target = normalize_project_reference(&base, &reference);
                    nodes.push(leaf(
                        format!("projref|{rel}|{target}").into(),
                        title,
                        project_path(worktree_id, &target),
                    ));
                }
                Ok(nodes)
            }
            "dir" => Ok(dir_nodes(worktree_id, rel, &access, cx)),
            _ => Ok(Vec::new()),
        }
    }
}

fn project_node(worktree_id: WorktreeId, name: &str, csproj_rel: &str) -> ProjectPanelViewNode {
    container(
        encode_id(NodeKind::Project, worktree_id, csproj_rel),
        name.to_string(),
    )
}

/// Directory listing as nodes: subdirectories become `dir` containers, files become leaves
/// carrying a `ProjectPath` so a click opens them.
fn dir_nodes(
    worktree_id: WorktreeId,
    dir_rel: &str,
    access: &WorktreeAccess,
    cx: &mut AsyncApp,
) -> Vec<ProjectPanelViewNode> {
    access
        .list_dir(dir_rel, cx)
        .into_iter()
        .map(|(name, is_dir, rel)| {
            if is_dir {
                container(encode_id(NodeKind::Directory, worktree_id, &rel), name)
            } else {
                leaf(
                    format!("file|{rel}").into(),
                    name,
                    project_path(worktree_id, &rel),
                )
            }
        })
        .collect()
}

/// Resolves a `..`-relative `ProjectReference` path against the referencing project's directory,
/// producing a worktree-relative path. Falls back to the raw reference if it escapes the root.
fn normalize_project_reference(base_dir: &str, reference: &str) -> String {
    let mut components: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for part in reference.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return reference.to_string();
                }
            }
            other => components.push(other),
        }
    }
    components.join("/")
}

fn has_extension(name: &str, extension: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(extension))
}

impl ProjectPanelViewProvider for SolutionProvider {
    fn id(&self) -> Arc<str> {
        PROVIDER_ID.into()
    }

    fn display_name(&self) -> SharedString {
        "Solution".into()
    }

    fn root_nodes(
        &self,
        project: Entity<Project>,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>> {
        cx.spawn(async move |cx| {
            let Some(access) = WorktreeAccess::resolve(&project, cx) else {
                return Ok(Vec::new());
            };
            Ok(SolutionProvider.root_nodes_impl(access, cx).await)
        })
    }

    fn children(
        &self,
        project: Entity<Project>,
        parent: ProjectPanelViewNode,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>> {
        cx.spawn(async move |cx| {
            let Some(access) = WorktreeAccess::resolve(&project, cx) else {
                return Ok(Vec::new());
            };
            SolutionProvider.children_impl(parent, access, cx).await
        })
    }

    fn subscribe_invalidation(
        &self,
        _cx: &mut App,
        _on_invalidate: Box<dyn Fn(&mut App) + 'static>,
    ) -> Subscription {
        // No push-based invalidation yet; the tree refreshes when the panel re-queries on expand.
        Subscription::new(|| {})
    }
}
