use std::sync::Arc;

use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Subscription, Task};
use project::{Project, ProjectPath};

/// A single node in a provider-supplied project panel tree. Unlike worktree entries,
/// a node may have no backing `ProjectPath` at all (e.g. a virtual "Dependencies" node).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPanelViewNode {
    pub id: SharedString,
    pub title: SharedString,
    pub icon: Option<SharedString>,
    pub is_container: bool,
    pub project_path: Option<ProjectPath>,
}

/// Supplies a tree of [`ProjectPanelViewNode`]s to render in a project panel view, in place
/// of the default glob-filtered worktree tree. Registered by id (see [`ProjectPanelView::provider`]
/// in `project_panel_settings.rs`) and looked up through [`ProjectPanelViewProviderRegistry`].
///
/// Every method returns a `Task` (rather than a plain value) so that a future WASM extension
/// proxy (phase 3 of the panel-views-extension architecture) can implement this trait by making
/// an RPC round trip without changing the trait's shape.
pub trait ProjectPanelViewProvider: Send + Sync {
    fn id(&self) -> Arc<str>;

    fn display_name(&self) -> SharedString;

    fn root_nodes(
        &self,
        project: Entity<Project>,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>>;

    fn children(
        &self,
        project: Entity<Project>,
        parent: ProjectPanelViewNode,
        cx: &AsyncApp,
    ) -> Task<Result<Vec<ProjectPanelViewNode>>>;

    /// Registers a callback to be invoked when the provider's tree changes out-of-band
    /// (e.g. an extension re-parsing a project file after it changed on disk).
    fn subscribe_invalidation(
        &self,
        cx: &mut App,
        on_invalidate: Box<dyn Fn(&mut App) + 'static>,
    ) -> Subscription;
}

struct GlobalProjectPanelViewProviderRegistry(Entity<ProjectPanelViewProviderRegistry>);

impl Global for GlobalProjectPanelViewProviderRegistry {}

#[derive(Default)]
pub struct ProjectPanelViewProviderRegistry {
    providers: HashMap<Arc<str>, Arc<dyn ProjectPanelViewProvider>>,
}

impl ProjectPanelViewProviderRegistry {
    /// Returns the global [`ProjectPanelViewProviderRegistry`].
    ///
    /// Inserts a default [`ProjectPanelViewProviderRegistry`] if one does not yet exist.
    pub fn default_global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalProjectPanelViewProviderRegistry>() {
            let registry = cx.new(|_| Self::default());
            cx.set_global(GlobalProjectPanelViewProviderRegistry(registry));
        }
        cx.global::<GlobalProjectPanelViewProviderRegistry>()
            .0
            .clone()
    }

    pub fn register(
        &mut self,
        id: Arc<str>,
        provider: Arc<dyn ProjectPanelViewProvider>,
        cx: &mut Context<Self>,
    ) {
        self.providers.insert(id, provider);
        cx.notify();
    }

    pub fn unregister(&mut self, id: &str, cx: &mut Context<Self>) {
        self.providers.remove(id);
        cx.notify();
    }

    pub fn provider(&self, id: &str) -> Option<Arc<dyn ProjectPanelViewProvider>> {
        self.providers.get(id).cloned()
    }
}
