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

/// How many directory levels below the worktree root to search for `.sln` files. Solutions
/// commonly live one level down (e.g. a Unity project folder), not at the very root.
const MAX_SOLUTION_SEARCH_DEPTH: usize = 3;

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
    /// A directory in an SDK-style project's source tree; children are its entries. `rel` is the directory path.
    Directory,
    /// A virtual directory of a legacy project's explicit compile-item tree; `rel` is
    /// `csproj_rel|subdir` (the owning project plus the csproj-relative directory).
    ItemDirectory,
}

impl NodeKind {
    fn tag(&self) -> &'static str {
        match self {
            NodeKind::Solution => "sln",
            NodeKind::Project => "proj",
            NodeKind::Dependencies => "deps",
            NodeKind::Directory => "dir",
            NodeKind::ItemDirectory => "itemdir",
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

    /// Lists the direct children of `parent_rel`, skipping build-output/tooling dirs. Returns
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
                if entry.is_dir() && is_noise_dir(name) {
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

    /// Searches the worktree (root plus a bounded depth of subdirectories, skipping build/tooling
    /// dirs) for `.sln` files. Returns `(file_name, rel_path)` sorted by path for stable ordering.
    fn find_solutions(&self, cx: &mut AsyncApp) -> Vec<(String, String)> {
        cx.update(|cx| {
            let snapshot = self.entity.read(cx).snapshot();
            let mut solutions = Vec::new();
            let root: std::sync::Arc<RelPath> = RelPath::empty().into();
            let mut stack = vec![(root, 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                for entry in snapshot.child_entries(dir.as_ref()) {
                    let Some(name) = entry.path.file_name() else {
                        continue;
                    };
                    if entry.is_dir() {
                        if depth < MAX_SOLUTION_SEARCH_DEPTH && !is_solution_search_skip_dir(name) {
                            stack.push((entry.path.clone(), depth + 1));
                        }
                    } else if has_extension(name, "sln") {
                        solutions
                            .push((name.to_string(), entry.path.as_unix_str().to_string()));
                    }
                }
            }
            solutions.sort_by(|left, right| left.1.cmp(&right.1));
            solutions
        })
    }
}

/// Build output and tooling directories hidden from a project's source tree.
fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        "bin" | "obj" | ".vs" | ".idea" | "Library" | "Temp" | "Logs" | "node_modules"
    )
}

/// Directories not worth descending into when searching for solution files (build/tooling noise,
/// large asset/package trees that never hold the main `.sln`, and dotfiles like `.git`/`.claude`).
fn is_solution_search_skip_dir(name: &str) -> bool {
    is_noise_dir(name)
        || name.starts_with('.')
        || matches!(name, "Assets" | "Packages" | "PackageCache" | "docs")
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

/// Embedded SVG icon paths for the *virtual* nodes that have no backing file and thus no
/// icon-theme association (rooted at `assets/icons/…`, so `Icon::from_path` resolves them without
/// an icon theme). File-backed nodes (`.sln`, `.csproj`, project references, source files) instead
/// carry a `project_path` and leave `icon` unset, so the host resolves their glyph from the user's
/// active icon theme by file type — matching whatever the file tree shows for the same file.
const DEPENDENCIES_ICON: &str = "icons/blocks.svg";
const PACKAGE_ICON: &str = "icons/file_icons/package.svg";

fn container(
    id: SharedString,
    title: impl Into<SharedString>,
    icon: Option<SharedString>,
    project_path: Option<ProjectPath>,
) -> ProjectPanelViewNode {
    ProjectPanelViewNode {
        id,
        title: title.into(),
        icon,
        is_container: true,
        project_path,
    }
}

fn leaf(
    id: SharedString,
    title: impl Into<SharedString>,
    project_path: Option<ProjectPath>,
    icon: Option<SharedString>,
) -> ProjectPanelViewNode {
    ProjectPanelViewNode {
        id,
        title: title.into(),
        icon,
        is_container: false,
        project_path,
    }
}

impl SolutionProvider {
    /// Root nodes: one per `.sln` found in the worktree, or (fallback) top-level `.csproj` projects.
    async fn root_nodes_impl(
        &self,
        access: WorktreeAccess,
        cx: &mut AsyncApp,
    ) -> Vec<ProjectPanelViewNode> {
        let worktree_id = access.worktree_id;
        let solutions = access.find_solutions(cx);
        if !solutions.is_empty() {
            return solutions
                .into_iter()
                .map(|(name, rel)| {
                    container(
                        encode_id(NodeKind::Solution, worktree_id, &rel),
                        name,
                        None,
                        project_path(worktree_id, &rel),
                    )
                })
                .collect();
        }
        // Fallback: no solution file, surface top-level projects directly.
        access
            .list_dir("", cx)
            .into_iter()
            .filter(|(name, is_dir, _)| !is_dir && has_extension(name, "csproj"))
            .map(|(name, _, rel)| project_node(worktree_id, &name, &rel))
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
                    Some(DEPENDENCIES_ICON.into()),
                    None,
                )];
                let text = access.load(rel).await?;
                let info = parse_csproj(&text);
                if info.is_sdk_style {
                    nodes.extend(dir_nodes(worktree_id, &parent_dir(rel), &access, cx));
                } else {
                    // Legacy projects (e.g. Unity-generated) share one directory with every other
                    // project of the solution, so their tree is built from explicit compile items
                    // rather than a directory listing.
                    nodes.extend(item_nodes(worktree_id, rel, &info.compile_items, ""));
                }
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
                    nodes.push(leaf(
                        format!("pkg|{rel}|{name}").into(),
                        title,
                        None,
                        Some(PACKAGE_ICON.into()),
                    ));
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
                        None,
                    ));
                }
                Ok(nodes)
            }
            "dir" => Ok(dir_nodes(worktree_id, rel, &access, cx)),
            "itemdir" => {
                let Some((csproj_rel, subdir)) = rel.split_once('|') else {
                    return Ok(Vec::new());
                };
                let text = access.load(csproj_rel).await?;
                let info = parse_csproj(&text);
                Ok(item_nodes(
                    worktree_id,
                    csproj_rel,
                    &info.compile_items,
                    subdir,
                ))
            }
            _ => Ok(Vec::new()),
        }
    }
}

fn project_node(worktree_id: WorktreeId, name: &str, csproj_rel: &str) -> ProjectPanelViewNode {
    container(
        encode_id(NodeKind::Project, worktree_id, csproj_rel),
        name.to_string(),
        None,
        project_path(worktree_id, csproj_rel),
    )
}

/// Directory listing as nodes: subdirectories become `dir` containers, files become leaves
/// carrying a `ProjectPath` so a click opens them. Solution/project files are hidden — they are
/// already represented by their own tree nodes.
fn dir_nodes(
    worktree_id: WorktreeId,
    dir_rel: &str,
    access: &WorktreeAccess,
    cx: &mut AsyncApp,
) -> Vec<ProjectPanelViewNode> {
    access
        .list_dir(dir_rel, cx)
        .into_iter()
        .filter(|(name, is_dir, _)| {
            *is_dir || !(has_extension(name, "csproj") || has_extension(name, "sln"))
        })
        .map(|(name, is_dir, rel)| {
            if is_dir {
                container(
                    encode_id(NodeKind::Directory, worktree_id, &rel),
                    name,
                    None,
                    None,
                )
            } else {
                leaf(
                    format!("file|{rel}").into(),
                    name,
                    project_path(worktree_id, &rel),
                    None,
                )
            }
        })
        .collect()
}

/// One level of a legacy project's virtual tree, derived from its explicit compile-item paths.
/// `prefix` is the csproj-relative directory being expanded (empty for the project root).
/// Directories come first, each group sorted case-insensitively.
fn item_nodes(
    worktree_id: WorktreeId,
    csproj_rel: &str,
    items: &[String],
    prefix: &str,
) -> Vec<ProjectPanelViewNode> {
    let base = parent_dir(csproj_rel);
    let mut directories = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for item in items {
        let remainder = if prefix.is_empty() {
            item.as_str()
        } else {
            match item.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) => rest,
                None => continue,
            }
        };
        match remainder.split_once('/') {
            Some((head, _)) => {
                if !directories.iter().any(|existing| existing == head) {
                    directories.push(head.to_string());
                }
            }
            None => {
                if !remainder.is_empty() {
                    files.push((remainder.to_string(), item.clone()));
                }
            }
        }
    }
    directories.sort_by_key(|name| name.to_lowercase());
    files.sort_by_key(|(name, _)| name.to_lowercase());

    let mut nodes = Vec::new();
    for name in directories {
        let subdir = join_rel(prefix, &name);
        nodes.push(container(
            encode_id(
                NodeKind::ItemDirectory,
                worktree_id,
                &format!("{csproj_rel}|{subdir}"),
            ),
            name,
            None,
            None,
        ));
    }
    for (name, item) in files {
        let target = normalize_project_reference(&base, &item);
        nodes.push(leaf(
            format!("item|{csproj_rel}|{item}").into(),
            name,
            project_path(worktree_id, &target),
            None,
        ));
    }
    nodes
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext, TestAppContext};
    use pretty_assertions::assert_eq;
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            // Scan everything (including Library/obj) so the provider's own noise filtering is exercised.
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_exclusions = Some(Vec::new());
                });
            });
        });
    }

    /// A fixture mirroring the Aspid.FastTools layout: solutions nested one directory below the
    /// worktree root, alongside Unity build noise (`Library`, `obj`) that must be hidden.
    async fn fasttools_project(cx: &mut TestAppContext) -> Entity<Project> {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": { "HEAD": "" },
                "docs": { "readme.md": "" },
                "Aspid.FastTools.Generators": {
                    "Aspid.FastTools.Generators.sln":
                        "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Aspid.FastTools.Generators\", \"Aspid.FastTools.Generators.csproj\", \"{1}\"\n",
                    "Aspid.FastTools.Generators.csproj":
                        "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup><PackageReference Include=\"Microsoft.CodeAnalysis\" Version=\"4.0.0\" /></ItemGroup></Project>",
                    "Generator.cs": "",
                },
                "Aspid.FastTools": {
                    "Aspid.FastTools.sln":
                        "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Aspid.FastTools\", \"Aspid.FastTools.csproj\", \"{1}\"\nProject(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Solution Items\", \"Solution Items\", \"{9}\"\n",
                    // Legacy Unity-style project: no `Sdk` attribute, explicit compile items.
                    "Aspid.FastTools.csproj":
                        "<Project ToolsVersion=\"4.0\"><ItemGroup><Compile Include=\"Source\\Foo.cs\" /><Compile Include=\"Assets\\Bar.cs\" /><PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.3\" /><ProjectReference Include=\"..\\Aspid.FastTools.Generators\\Aspid.FastTools.Generators.csproj\" /></ItemGroup></Project>",
                    "Source": { "Foo.cs": "" },
                    "Assets": { "Bar.cs": "" },
                    "Library": { "ScriptAssemblies": { "x.dll": "" } },
                    "obj": { "project.assets.json": "" },
                },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.run_until_parked();
        project
    }

    fn titles(nodes: &[ProjectPanelViewNode]) -> Vec<String> {
        nodes.iter().map(|node| node.title.to_string()).collect()
    }

    #[gpui::test]
    async fn discovers_nested_solutions(cx: &mut TestAppContext) {
        let project = fasttools_project(cx).await;
        let roots = SolutionProvider
            .root_nodes(project, &cx.to_async())
            .await
            .unwrap();
        // Both nested solutions found, sorted by path; the `docs`/`.git` dirs are skipped.
        assert_eq!(
            titles(&roots),
            vec![
                "Aspid.FastTools.Generators.sln".to_string(),
                "Aspid.FastTools.sln".to_string(),
            ]
        );
    }

    #[gpui::test]
    async fn expands_solution_project_and_dependencies(cx: &mut TestAppContext) {
        let project = fasttools_project(cx).await;
        let async_cx = cx.to_async();

        let roots = SolutionProvider
            .root_nodes(project.clone(), &async_cx)
            .await
            .unwrap();
        let solution = roots
            .iter()
            .find(|node| node.title.as_ref() == "Aspid.FastTools.sln")
            .cloned()
            .expect("solution node");

        // Solution folders (`Solution Items`) are skipped; only the real project remains.
        let projects = SolutionProvider
            .children(project.clone(), solution, &async_cx)
            .await
            .unwrap();
        assert_eq!(titles(&projects), vec!["Aspid.FastTools".to_string()]);

        // A legacy Unity-style project shows only its explicit compile items (as virtual
        // directories), not the shared directory next to the `.sln` with every other project.
        let project_children = SolutionProvider
            .children(project.clone(), projects[0].clone(), &async_cx)
            .await
            .unwrap();
        assert_eq!(
            titles(&project_children),
            vec![
                "Dependencies".to_string(),
                "Assets".to_string(),
                "Source".to_string(),
            ]
        );

        let assets = project_children
            .iter()
            .find(|node| node.title.as_ref() == "Assets")
            .cloned()
            .expect("assets node");
        let assets_children = SolutionProvider
            .children(project.clone(), assets, &async_cx)
            .await
            .unwrap();
        assert_eq!(titles(&assets_children), vec!["Bar.cs".to_string()]);
        assert_eq!(
            assets_children[0].project_path,
            project_path(
                project.read_with(&async_cx, |project, cx| {
                    project.visible_worktrees(cx).next().unwrap().read(cx).id()
                }),
                "Aspid.FastTools/Assets/Bar.cs",
            ),
        );

        let dependencies = project_children
            .into_iter()
            .find(|node| node.title.as_ref() == "Dependencies")
            .expect("dependencies node");
        let deps = SolutionProvider
            .children(project, dependencies, &async_cx)
            .await
            .unwrap();
        assert_eq!(
            titles(&deps),
            vec![
                "Newtonsoft.Json (13.0.3)".to_string(),
                "Aspid.FastTools.Generators".to_string(),
            ]
        );
    }

    #[gpui::test]
    async fn sdk_project_lists_its_directory_without_project_files(cx: &mut TestAppContext) {
        let project = fasttools_project(cx).await;
        let async_cx = cx.to_async();

        let roots = SolutionProvider
            .root_nodes(project.clone(), &async_cx)
            .await
            .unwrap();
        let solution = roots
            .iter()
            .find(|node| node.title.as_ref() == "Aspid.FastTools.Generators.sln")
            .cloned()
            .expect("generators solution node");
        let projects = SolutionProvider
            .children(project.clone(), solution, &async_cx)
            .await
            .unwrap();
        assert_eq!(
            titles(&projects),
            vec!["Aspid.FastTools.Generators".to_string()]
        );

        // SDK-style projects list their directory, minus `.sln`/`.csproj` entries which are
        // already represented by the solution/project nodes themselves.
        let children = SolutionProvider
            .children(project, projects[0].clone(), &async_cx)
            .await
            .unwrap();
        assert_eq!(
            titles(&children),
            vec!["Dependencies".to_string(), "Generator.cs".to_string()]
        );
    }
}
