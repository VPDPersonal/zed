use editor::{EditorSettings, ui_scrollbar_settings_from_raw};
use gpui::{Pixels, SharedString};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{
    DockSide, ProjectPanelEntrySpacing, ProjectPanelSortMode, ProjectPanelSortOrder,
    ProjectPanelViewSelector, RegisterSetting, Settings, ShowDiagnostics, ShowIndentGuides,
};
use ui::{
    px,
    scrollbars::{ScrollbarVisibility, ShowScrollbar},
};

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct ProjectPanelSettings {
    pub button: bool,
    pub hide_gitignore: bool,
    pub default_width: Pixels,
    pub dock: DockSide,
    pub entry_spacing: ProjectPanelEntrySpacing,
    pub file_icons: bool,
    pub folder_icons: bool,
    pub git_status: bool,
    pub indent_size: f32,
    pub indent_guides: IndentGuidesSettings,
    pub sticky_scroll: bool,
    pub auto_reveal_entries: bool,
    pub auto_fold_dirs: bool,
    pub fold_single_file_dirs: bool,
    pub bold_folder_labels: bool,
    pub starts_open: bool,
    pub scrollbar: ScrollbarSettings,
    pub show_diagnostics: ShowDiagnostics,
    pub hide_root: bool,
    pub hide_hidden: bool,
    pub drag_and_drop: bool,
    pub auto_open: AutoOpenSettings,
    pub sort_mode: ProjectPanelSortMode,
    pub sort_order: ProjectPanelSortOrder,
    pub diagnostic_badges: bool,
    pub git_status_indicator: bool,
    pub view_selector: ProjectPanelViewSelector,
}

/// A single resolved project panel view: a named filter over the worktree tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPanelView {
    pub name: SharedString,
    /// Id of a registered project panel view provider. When set, this view is rendered by
    /// that provider's own tree instead of filtering the worktree via glob; `include`,
    /// `exclude`, and `hide_dirs` are ignored for this view.
    pub provider: Option<SharedString>,
    /// Optional group name. Views sharing a group are presented together as one selector
    /// cluster in the panel header; views without a group share a single default cluster.
    /// Grouping only affects presentation — one view is active at a time.
    pub group: Option<SharedString>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Whether to hide the worktree root in this view, promoting its top-level
    /// folders to roots. Only takes effect when a single worktree is open.
    /// `None` inherits the global `project_panel.hide_root`; `Some(false)` lets a
    /// view re-show the root even when the global setting hides it.
    pub hide_root: Option<bool>,
    /// Literal worktree-relative paths of directories whose row is hidden in this view,
    /// splicing their children one level up. Stored normalized (no leading/trailing `/`).
    pub hide_dirs: Vec<String>,
    /// Whether this view folds a directory holding a single file into one `dir/file` row.
    /// `None` inherits the panel's `fold_single_file_dirs`; `Some` overrides it for this view.
    pub fold_single_file_dirs: Option<bool>,
}

/// The list of user-defined project panel views. Kept in its own settings type
/// (rather than on [`ProjectPanelSettings`]) because it holds a `Vec` and so cannot
/// be `Copy` like the rest of the panel settings.
#[derive(Clone, Debug, PartialEq, Eq, RegisterSetting)]
pub struct ProjectPanelViewsSettings {
    pub views: Vec<ProjectPanelView>,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct IndentGuidesSettings {
    pub show: ShowIndentGuides,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    /// When to show the scrollbar in the project panel.
    ///
    /// Default: inherits editor scrollbar settings
    pub show: Option<ShowScrollbar>,
    /// Whether to allow horizontal scrolling in the project panel.
    /// When false, the view is locked to the leftmost position and long file names are clipped.
    ///
    /// Default: true
    pub horizontal_scroll: bool,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct AutoOpenSettings {
    pub on_create: bool,
    pub on_paste: bool,
    pub on_drop: bool,
}

impl AutoOpenSettings {
    #[inline]
    pub fn should_open_on_create(self) -> bool {
        self.on_create
    }

    #[inline]
    pub fn should_open_on_paste(self) -> bool {
        self.on_paste
    }

    #[inline]
    pub fn should_open_on_drop(self) -> bool {
        self.on_drop
    }
}

#[derive(Default)]
pub(crate) struct ProjectPanelScrollbarProxy;

impl ScrollbarVisibility for ProjectPanelScrollbarProxy {
    fn visibility(&self, cx: &ui::App) -> ShowScrollbar {
        ProjectPanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl Settings for ProjectPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let project_panel = content.project_panel.clone().unwrap();
        Self {
            button: project_panel.button.unwrap(),
            hide_gitignore: project_panel.hide_gitignore.unwrap(),
            default_width: px(project_panel.default_width.unwrap()),
            dock: project_panel.dock.unwrap(),
            entry_spacing: project_panel.entry_spacing.unwrap(),
            file_icons: project_panel.file_icons.unwrap(),
            folder_icons: project_panel.folder_icons.unwrap(),
            git_status: project_panel.git_status.unwrap()
                && content
                    .git
                    .as_ref()
                    .unwrap()
                    .enabled
                    .unwrap()
                    .is_git_status_enabled(),
            indent_size: project_panel.indent_size.unwrap(),
            indent_guides: IndentGuidesSettings {
                show: project_panel.indent_guides.unwrap().show.unwrap(),
            },
            sticky_scroll: project_panel.sticky_scroll.unwrap(),
            auto_reveal_entries: project_panel.auto_reveal_entries.unwrap(),
            auto_fold_dirs: project_panel.auto_fold_dirs.unwrap(),
            fold_single_file_dirs: project_panel.fold_single_file_dirs.unwrap(),
            bold_folder_labels: project_panel.bold_folder_labels.unwrap(),
            starts_open: project_panel.starts_open.unwrap(),
            scrollbar: {
                let scrollbar = project_panel.scrollbar.unwrap();
                ScrollbarSettings {
                    show: scrollbar.show.map(ui_scrollbar_settings_from_raw),
                    horizontal_scroll: scrollbar.horizontal_scroll.unwrap(),
                }
            },
            show_diagnostics: project_panel.show_diagnostics.unwrap(),
            hide_root: project_panel.hide_root.unwrap(),
            hide_hidden: project_panel.hide_hidden.unwrap(),
            drag_and_drop: project_panel.drag_and_drop.unwrap(),
            auto_open: {
                let auto_open = project_panel.auto_open.unwrap();
                AutoOpenSettings {
                    on_create: auto_open.on_create.unwrap(),
                    on_paste: auto_open.on_paste.unwrap(),
                    on_drop: auto_open.on_drop.unwrap(),
                }
            },
            sort_mode: project_panel.sort_mode.unwrap(),
            sort_order: project_panel.sort_order.unwrap(),
            diagnostic_badges: project_panel.diagnostic_badges.unwrap(),
            git_status_indicator: project_panel.git_status_indicator.unwrap(),
            view_selector: project_panel.view_selector.unwrap(),
        }
    }
}

impl Settings for ProjectPanelViewsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let views = content
            .project
            .project_panel_views
            .clone()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, view)| ProjectPanelView {
                name: view
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("View {}", index + 1))
                    .into(),
                provider: view.provider.map(Into::into),
                group: view
                    .group
                    .filter(|group| !group.is_empty())
                    .map(Into::into),
                include: view.include.unwrap_or_default(),
                exclude: view.exclude.unwrap_or_default(),
                hide_root: view.hide_root,
                hide_dirs: view
                    .hide_dirs
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|dir| {
                        let normalized = dir.trim_matches('/');
                        if normalized.is_empty() {
                            None
                        } else {
                            Some(normalized.to_string())
                        }
                    })
                    .collect(),
                fold_single_file_dirs: view.fold_single_file_dirs,
            })
            .collect();
        Self { views }
    }
}
