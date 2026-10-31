use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::{PaneId, TileLayout};
use crate::pane::PaneState;
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

use super::{
    discover_workspace_git_identity, generate_workspace_id, git_branch, NewPane, Tab, Workspace,
};

/// Where a new herdr-core pane gets its size, colours and render hooks.
pub(crate) struct CorePaneHost {
    pub rows: u16,
    pub cols: u16,
    pub scrollback_limit_bytes: usize,
    pub host_terminal_theme: crate::terminal_theme::TerminalTheme,
    pub host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
}

struct TabHooks {
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
}

impl Workspace {
    /// A new workspace whose first tab shows `core_terminal` from herdr-core.
    pub(crate) fn new_core_attach(
        core_terminal: &str,
        fallback_cwd: PathBuf,
        host: &CorePaneHost,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let hooks = TabHooks {
            events,
            render_notify,
            render_dirty,
        };
        let (tab, terminal, runtime) = core_root_tab(1, core_terminal, fallback_cwd, host, hooks)?;
        let initial_cwd = terminal.cwd.clone();
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        let (cached_git_space, cached_auto_label, cached_git_status_key) =
            discover_workspace_git_identity(&initial_cwd);
        Ok((
            Self {
                id: generate_workspace_id(),
                custom_name: None,
                identity_cwd: initial_cwd.clone(),
                cached_identity_cwd: initial_cwd.clone(),
                cached_auto_label,
                cached_git_status_key,
                cached_git_branch: git_branch(&initial_cwd),
                cached_git_ahead_behind: None,
                cached_git_space,
                worktree_space: None,
                metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
                metadata_token_sequences: HashMap::new(),
                public_pane_numbers,
                next_public_pane_number: 2,
                next_public_tab_number: 2,
                tabs: vec![tab],
                active_tab: 0,
                #[cfg(test)]
                test_runtimes: HashMap::new(),
            },
            terminal,
            runtime,
        ))
    }

    /// A new tab whose only pane shows `core_terminal` from herdr-core.
    #[allow(clippy::too_many_arguments)]
    pub fn create_tab_core_attach(
        &mut self,
        rows: u16,
        cols: u16,
        fallback_cwd: PathBuf,
        core_terminal: &str,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        let host = CorePaneHost {
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
        };
        let hooks = self.tab_hooks();
        let number = self.next_public_tab_number;
        let (tab, terminal, runtime) =
            core_root_tab(number, core_terminal, fallback_cwd, &host, hooks)?;
        self.next_public_tab_number += 1;
        let pane_number = self.next_public_pane_number;
        self.register_new_pane_with_number(tab.root_pane, pane_number);
        self.tabs.push(tab);
        Ok((self.tabs.len() - 1, terminal, runtime))
    }

    /// Split `target` and show `core_terminal` in the new pane, which takes
    /// `1 - ratio` of the space.
    pub(crate) fn split_core_attach(
        &mut self,
        target: PaneId,
        direction: Direction,
        ratio: f32,
        core_terminal: &str,
        host: &CorePaneHost,
    ) -> Option<std::io::Result<(usize, NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(target)?;
        let pane_number = self.next_public_pane_number;
        let tab = &mut self.tabs[tab_idx];
        let new_id = tab.layout.split_pane(target, direction, ratio)?;
        let attached = TerminalRuntime::attach_core(
            new_id,
            host.rows,
            host.cols,
            core_terminal,
            host.scrollback_limit_bytes,
            host.host_terminal_theme,
            host.host_terminal_appearance,
            tab.events.clone(),
            tab.render_notify.clone(),
            tab.render_dirty.clone(),
        );
        let (runtime, core_cwd) = match attached {
            Ok(attached) => attached,
            Err(err) => {
                tab.layout.close_pane(new_id);
                return Some(Err(err));
            }
        };
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), core_cwd.unwrap_or_default());
        tab.panes.insert(new_id, PaneState::new(terminal_id));
        tab.zoomed = false;
        self.register_new_pane_with_number(new_id, pane_number);
        Some(Ok((
            tab_idx,
            NewPane {
                pane_id: new_id,
                terminal,
                runtime,
            },
        )))
    }

    fn tab_hooks(&self) -> TabHooks {
        let tab = self
            .active_tab()
            .expect("workspace must always have at least one tab");
        TabHooks {
            events: tab.events.clone(),
            render_notify: tab.render_notify.clone(),
            render_dirty: tab.render_dirty.clone(),
        }
    }
}

fn core_root_tab(
    number: usize,
    core_terminal: &str,
    fallback_cwd: PathBuf,
    host: &CorePaneHost,
    hooks: TabHooks,
) -> std::io::Result<(Tab, TerminalState, TerminalRuntime)> {
    let (layout, root_id) = TileLayout::new();
    let (runtime, core_cwd) = TerminalRuntime::attach_core(
        root_id,
        host.rows,
        host.cols,
        core_terminal,
        host.scrollback_limit_bytes,
        host.host_terminal_theme,
        host.host_terminal_appearance,
        hooks.events.clone(),
        hooks.render_notify.clone(),
        hooks.render_dirty.clone(),
    )?;
    let terminal_id = TerminalId::alloc();
    let terminal = TerminalState::new(terminal_id.clone(), core_cwd.unwrap_or(fallback_cwd));
    let mut panes = HashMap::new();
    panes.insert(root_id, PaneState::new(terminal_id));
    let tab = Tab {
        custom_name: None,
        number,
        root_pane: root_id,
        layout,
        panes,
        #[cfg(test)]
        runtimes: HashMap::new(),
        zoomed: false,
        events: hooks.events,
        render_notify: hooks.render_notify,
        render_dirty: hooks.render_dirty,
    };
    Ok((tab, terminal, runtime))
}
