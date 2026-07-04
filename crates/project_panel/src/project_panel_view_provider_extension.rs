use std::sync::Arc;

use anyhow::Result;
use extension::{Extension, ExtensionHostProxy, ExtensionProjectPanelViewProxy, ProjectDelegate};
use gpui::{App, AsyncApp, Entity, SharedString, Subscription, Task};
use project::{Project, ProjectPath};
use settings::WorktreeId;
use util::rel_path::RelPath;

use crate::project_panel_view_provider::{
    ProjectPanelViewNode, ProjectPanelViewProvider, ProjectPanelViewProviderRegistry,
};

pub fn init(cx: &mut App) {
    let proxy = ExtensionHostProxy::default_global(cx);
    proxy.register_project_panel_view_proxy(ProjectPanelViewProviderRegistryProxy {
        registry: ProjectPanelViewProviderRegistry::default_global(cx),
    });
}

struct ExtensionProject {
    worktree_ids: Vec<u64>,
}

impl ProjectDelegate for ExtensionProject {
    fn worktree_ids(&self) -> Vec<u64> {
        self.worktree_ids.clone()
    }
}

fn extension_project(project: &Entity<Project>, cx: &AsyncApp) -> Arc<dyn ProjectDelegate> {
    cx.update(|cx| {
        Arc::new(ExtensionProject {
            worktree_ids: project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).id().to_proto())
                .collect(),
        }) as Arc<dyn ProjectDelegate>
    })
}

fn node_from_extension(node: extension::ProjectPanelViewNode) -> Result<ProjectPanelViewNode> {
    let project_path = node
        .project_path
        .map(|path| -> Result<ProjectPath> {
            Ok(ProjectPath {
                worktree_id: WorktreeId::from_proto(path.worktree_id),
                path: RelPath::unix(&path.path)?.into(),
            })
        })
        .transpose()?;

    Ok(ProjectPanelViewNode {
        id: node.id.into(),
        title: node.title.into(),
        icon: node.icon.map(Into::into),
        is_container: node.is_container,
        project_path,
    })
}

fn node_to_extension(node: ProjectPanelViewNode) -> extension::ProjectPanelViewNode {
    extension::ProjectPanelViewNode {
        id: node.id.to_string(),
        title: node.title.to_string(),
        icon: node.icon.map(|icon| icon.to_string()),
        is_container: node.is_container,
        project_path: node.project_path.map(|path| extension::ProjectPanelViewPath {
            worktree_id: path.worktree_id.to_proto(),
            path: path.path.as_unix_str().to_string(),
        }),
    }
}

struct ExtensionBackedProjectPanelViewProvider {
    id: Arc<str>,
    extension: Arc<dyn Extension>,
}

impl ProjectPanelViewProvider for ExtensionBackedProjectPanelViewProvider {
    fn id(&self) -> Arc<str> {
        self.id.clone()
    }

    fn display_name(&self) -> SharedString {
        self.id.to_string().into()
    }

    fn root_nodes(
        &self,
        project: Entity<Project>,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>> {
        let provider_id = self.id.clone();
        let extension = self.extension.clone();
        cx.spawn(async move |cx| {
            let project_delegate = extension_project(&project, cx);
            let nodes = extension
                .project_panel_view_root_nodes(provider_id, project_delegate)
                .await?;
            nodes.into_iter().map(node_from_extension).collect()
        })
    }

    fn children(
        &self,
        project: Entity<Project>,
        parent: ProjectPanelViewNode,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>> {
        let provider_id = self.id.clone();
        let extension = self.extension.clone();
        cx.spawn(async move |cx| {
            let project_delegate = extension_project(&project, cx);
            let nodes = extension
                .project_panel_view_children(provider_id, project_delegate, node_to_extension(parent))
                .await?;
            nodes.into_iter().map(node_from_extension).collect()
        })
    }

    fn subscribe_invalidation(
        &self,
        _cx: &mut App,
        _on_invalidate: Box<dyn Fn(&mut App) + 'static>,
    ) -> Subscription {
        // The extension WIT API has no push-based invalidation event yet; a provider's tree is
        // only refreshed when the panel re-queries it (e.g. on expand/collapse).
        Subscription::new(|| {})
    }
}

struct ProjectPanelViewProviderRegistryProxy {
    registry: Entity<ProjectPanelViewProviderRegistry>,
}

impl ExtensionProjectPanelViewProxy for ProjectPanelViewProviderRegistryProxy {
    fn register_project_panel_view(
        &self,
        extension: Arc<dyn Extension>,
        provider_id: Arc<str>,
        cx: &mut App,
    ) {
        self.registry.update(cx, |registry, cx| {
            registry.register(
                provider_id.clone(),
                Arc::new(ExtensionBackedProjectPanelViewProvider {
                    id: provider_id,
                    extension,
                }),
                cx,
            )
        });
    }

    fn unregister_project_panel_view(&self, provider_id: Arc<str>, cx: &mut App) {
        self.registry
            .update(cx, |registry, cx| registry.unregister(&provider_id, cx));
    }
}
