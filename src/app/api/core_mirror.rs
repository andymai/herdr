use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::layout::Direction;
use tracing::warn;

use super::responses::{encode_error, encode_success};

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, Method, PaneReportMetadataParams, PaneSplitParams,
    Request, ResponseResult, SplitDirection, TabCreateParams, TabMoveParams, TabTarget,
    WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceMoveBlockParams,
};
use crate::app::{App, Mode};
use crate::core_mirror::{
    CoreCwd, CoreLayout, CoreNode, CoreSpace, CoreStatus, CoreTab, CoreTokens, MirroredTab,
};
use crate::detect::AgentState;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::workspace::{CorePaneHost, Workspace};

const STATUS_SOURCE: &str = "herdr-core";

impl App {
    /// Make the mirrored workspaces match the app's layout. Workspaces and
    /// tabs the mirror did not create are left alone.
    pub(crate) fn reconcile_core_layout(&mut self, layout: CoreLayout) {
        let live_spaces: HashSet<&str> = layout.spaces.iter().map(|s| s.id.as_str()).collect();
        let live_tabs: HashSet<&str> = layout
            .spaces
            .iter()
            .flat_map(|space| space.tabs.iter().map(|tab| tab.id.as_str()))
            .collect();
        let mirror = &mut self.core_mirror;
        mirror
            .pending_spaces
            .retain(|id| !live_spaces.contains(id.as_str()));
        mirror
            .pending_tabs
            .retain(|id| !live_tabs.contains(id.as_str()));
        mirror
            .closed_spaces
            .retain(|id| live_spaces.contains(id.as_str()));
        mirror
            .closed_tabs
            .retain(|id| live_tabs.contains(id.as_str()));
        self.forget_vanished_core_tabs();

        let gone_spaces: Vec<String> = self
            .core_mirror
            .spaces
            .keys()
            .filter(|id| {
                !live_spaces.contains(id.as_str())
                    && !self.core_mirror.pending_spaces.contains(id.as_str())
            })
            .cloned()
            .collect();
        for space_id in gone_spaces {
            if let Some(workspace_id) = self.core_mirror.spaces.remove(&space_id) {
                self.core_mirror
                    .tabs
                    .retain(|_, tab| tab.workspace_id != workspace_id);
                self.close_core_workspace(&workspace_id);
            }
        }
        let gone_tabs: Vec<String> = self
            .core_mirror
            .tabs
            .keys()
            .filter(|id| {
                !live_tabs.contains(id.as_str())
                    && !self.core_mirror.pending_tabs.contains(id.as_str())
            })
            .cloned()
            .collect();
        for tab_id in gone_tabs {
            if let Some(tab) = self.core_mirror.tabs.remove(&tab_id) {
                self.close_core_tab(&tab);
            }
        }

        for space in &layout.spaces {
            if self.core_mirror.closed_spaces.contains(&space.id) {
                continue;
            }
            if let Err(err) = self.mirror_core_space(space) {
                warn!(space = %space.id, err = %err, "failed to mirror herdr-core space");
            }
        }
        self.order_core_workspaces(&layout);
    }

    /// Mirrored workspaces sit together in the app's order, where the first
    /// of them already is, so moving that block elsewhere sticks.
    fn order_core_workspaces(&mut self, layout: &CoreLayout) {
        let desired: Vec<String> = layout
            .spaces
            .iter()
            .filter_map(|space| self.core_mirror.spaces.get(&space.id))
            .filter(|workspace_id| self.core_workspace_index(workspace_id).is_some())
            .cloned()
            .collect();
        if desired.len() < 2 {
            return;
        }
        let mirrored: HashSet<&str> = desired.iter().map(String::as_str).collect();
        let order: Vec<&str> = self
            .state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.as_str())
            .collect();
        let Some(first) = order.iter().position(|id| mirrored.contains(id)) else {
            return;
        };
        let block = &order[first..(first + desired.len()).min(order.len())];
        if block.iter().copied().eq(desired.iter().map(String::as_str)) {
            return;
        }
        let before_workspace_id = order[first..]
            .iter()
            .find(|id| !mirrored.contains(*id))
            .map(|id| (*id).to_owned());
        let _ = self.handle_workspace_move_block(
            String::new(),
            WorkspaceMoveBlockParams {
                workspace_ids: desired,
                before_workspace_id,
            },
        );
    }

    /// Mirrored tabs come first, in the app's order; local tabs follow.
    fn order_core_tabs(&mut self, ws_idx: usize, space: &CoreSpace) {
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let numbers: Vec<usize> = space
            .tabs
            .iter()
            .filter_map(|tab| self.core_mirror.tabs.get(&tab.id))
            .filter(|mirrored| mirrored.workspace_id == workspace_id)
            .map(|mirrored| mirrored.tab_number)
            .collect();
        for (target, number) in numbers.into_iter().enumerate() {
            let Some(current) = self.core_tab_index(ws_idx, number) else {
                continue;
            };
            if current == target {
                continue;
            }
            if let Some(tab_id) = self.public_tab_id(ws_idx, current) {
                let _ = self.handle_tab_move(
                    String::new(),
                    TabMoveParams {
                        tab_id,
                        insert_index: target,
                    },
                );
            }
        }
    }

    /// Only panes some client can see may set their core terminal's size.
    pub(crate) fn sync_core_pane_visibility(&self, visible: &HashSet<PaneId>) {
        for tab in self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
        {
            for (pane_id, pane) in &tab.panes {
                if let Some(runtime) = self
                    .terminal_runtimes
                    .get(&pane.attached_terminal_id)
                    .filter(|runtime| runtime.core_terminal().is_some())
                {
                    runtime.set_core_visible(visible.contains(pane_id));
                }
            }
        }
    }

    pub(crate) fn apply_core_status(
        &mut self,
        status: CoreStatus,
    ) -> Vec<crate::app::actions::PaneStateUpdate> {
        let panes = self.core_panes_showing(&status.terminal);
        let mut updates = Vec::new();
        for pane_id in panes {
            updates.extend(self.report_core_status(pane_id, &status));
        }
        if status.state.is_some() {
            self.core_mirror
                .statuses
                .insert(status.terminal.clone(), status);
        } else {
            self.core_mirror.statuses.remove(&status.terminal);
        }
        updates
    }

    fn mirror_core_space(&mut self, space: &CoreSpace) -> std::io::Result<()> {
        let existing = self
            .core_mirror
            .spaces
            .get(&space.id)
            .and_then(|workspace_id| self.core_workspace_index(workspace_id));
        let ws_idx = match existing {
            Some(ws_idx) => ws_idx,
            None => {
                let first = &space.tabs[0];
                let ws_idx = self.create_core_workspace(first, space.folder.clone())?;
                let workspace_id = self.state.workspaces[ws_idx].id.clone();
                self.core_mirror
                    .tabs
                    .retain(|tab_id, _| space.tabs.iter().all(|tab| &tab.id != tab_id));
                self.core_mirror.tabs.insert(
                    first.id.clone(),
                    MirroredTab {
                        workspace_id: workspace_id.clone(),
                        tab_number: self.state.workspaces[ws_idx].tabs[0].number,
                        signature: first.root.signature(),
                    },
                );
                self.core_mirror
                    .spaces
                    .insert(space.id.clone(), workspace_id);
                ws_idx
            }
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        self.name_core_workspace(ws_idx, space.name.as_deref());

        for tab in &space.tabs {
            if self.core_mirror.closed_tabs.contains(&tab.id) {
                continue;
            }
            let signature = tab.root.signature();
            let mirrored = self
                .core_mirror
                .tabs
                .get(&tab.id)
                .filter(|mirrored| mirrored.workspace_id == workspace_id)
                .cloned();
            let current = mirrored
                .as_ref()
                .and_then(|mirrored| self.core_tab_index(ws_idx, mirrored.tab_number));
            if let (Some(mirrored), Some(tab_idx)) = (&mirrored, current) {
                if mirrored.signature == signature {
                    self.name_core_tab(ws_idx, tab_idx, tab.name.as_deref());
                    continue;
                }
            }
            let replaced = mirrored.as_ref().zip(current).map(|(_, old_idx)| {
                let workspace = &self.state.workspaces[ws_idx];
                (
                    workspace.active_tab_index() == old_idx,
                    self.core_tab_name_set_here(ws_idx, old_idx),
                )
            });
            let tab_idx = self.create_core_tab(ws_idx, tab)?;
            if let Some((was_active, kept_name)) = replaced {
                let workspace = &mut self.state.workspaces[ws_idx];
                if let Some(name) = kept_name {
                    workspace.tabs[tab_idx].custom_name = Some(name);
                }
                if was_active {
                    workspace.switch_tab(tab_idx);
                }
            }
            self.core_mirror.tabs.insert(
                tab.id.clone(),
                MirroredTab {
                    workspace_id: workspace_id.clone(),
                    tab_number: self.state.workspaces[ws_idx].tabs[tab_idx].number,
                    signature,
                },
            );
            if let (Some(mirrored), Some(_)) = (mirrored, current) {
                self.close_core_tab(&mirrored);
            }
        }
        self.order_core_tabs(ws_idx, space);
        Ok(())
    }

    fn create_core_workspace(&mut self, tab: &CoreTab, folder: PathBuf) -> std::io::Result<usize> {
        let host = self.core_pane_host();
        let (workspace, terminal, runtime) = Workspace::new_core_attach(
            tab.root.first_terminal(),
            folder,
            &host,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.workspaces.push(workspace);
        let ws_idx = self.state.workspaces.len() - 1;
        let root_pane = self.state.workspaces[ws_idx].tabs[0].root_pane;
        self.state.remove_alias_shadowed_by_new_pane(root_pane);
        if self.state.active.is_none() {
            self.state.switch_workspace(ws_idx);
            self.state.mode = Mode::Terminal;
        }
        self.name_core_tab(ws_idx, 0, tab.name.as_deref());
        self.build_core_tree(ws_idx, root_pane, &tab.root);
        self.report_cached_core_state(root_pane, tab.root.first_terminal());
        self.schedule_session_save();
        self.emit_workspace_open_events(ws_idx);
        Ok(ws_idx)
    }

    fn create_core_tab(&mut self, ws_idx: usize, tab: &CoreTab) -> std::io::Result<usize> {
        let host = self.core_pane_host();
        let fallback_cwd = self.resolve_new_terminal_cwd(None);
        let (tab_idx, terminal, runtime) = self.state.workspaces[ws_idx].create_tab_core_attach(
            host.rows,
            host.cols,
            fallback_cwd,
            tab.root.first_terminal(),
            host.scrollback_limit_bytes,
            host.host_terminal_theme,
            host.host_terminal_appearance,
        )?;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        let root_pane = self.state.workspaces[ws_idx].tabs[tab_idx].root_pane;
        self.state.remove_alias_shadowed_by_new_pane(root_pane);
        self.name_core_tab(ws_idx, tab_idx, tab.name.as_deref());
        self.build_core_tree(ws_idx, root_pane, &tab.root);
        self.report_cached_core_state(root_pane, tab.root.first_terminal());
        self.schedule_session_save();
        self.emit_tab_created_events(ws_idx, tab_idx);
        Ok(tab_idx)
    }

    /// `pane` already shows the node's first terminal. Each split hands the
    /// rest of the node to a new pane, so earlier children keep their share.
    fn build_core_tree(&mut self, ws_idx: usize, pane: PaneId, node: &CoreNode) {
        let CoreNode::Split {
            side_by_side,
            children,
        } = node
        else {
            return;
        };
        let direction = if *side_by_side {
            Direction::Horizontal
        } else {
            Direction::Vertical
        };
        let mut remaining: f32 = children.iter().map(|(weight, _)| weight).sum();
        let mut placed = vec![(pane, &children[0].1)];
        let mut current = pane;
        for pair in children.windows(2) {
            let ((weight, _), (_, next)) = (&pair[0], &pair[1]);
            let ratio = weight / remaining;
            remaining -= weight;
            match self.split_core_pane(ws_idx, current, direction, ratio, next.first_terminal()) {
                Ok(new_pane) => {
                    placed.push((new_pane, next));
                    current = new_pane;
                }
                Err(err) => {
                    warn!(terminal = next.first_terminal(), err = %err, "failed to attach herdr-core split");
                    break;
                }
            }
        }
        for (pane, child) in placed {
            self.build_core_tree(ws_idx, pane, child);
        }
    }

    fn split_core_pane(
        &mut self,
        ws_idx: usize,
        target: PaneId,
        direction: Direction,
        ratio: f32,
        terminal: &str,
    ) -> std::io::Result<PaneId> {
        let host = self.core_pane_host();
        let (tab_idx, new_pane) = self.state.workspaces[ws_idx]
            .split_core_attach(target, direction, ratio, terminal, &host)
            .ok_or_else(|| std::io::Error::other("split target pane is not in the layout"))??;
        let pane_id = new_pane.pane_id;
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.report_cached_core_state(pane_id, terminal);
        if let Some(pane) = self.pane_info(ws_idx, pane_id) {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);
        Ok(pane_id)
    }

    fn close_core_workspace(&mut self, workspace_id: &str) {
        if let Some(ws_idx) = self.core_workspace_index(workspace_id) {
            let workspace_id = self.public_workspace_id(ws_idx);
            let _ = self.handle_workspace_close(
                String::new(),
                WorkspaceCloseParams {
                    workspace_id,
                    close_group: false,
                },
            );
        }
    }

    fn close_core_tab(&mut self, tab: &MirroredTab) {
        let Some(ws_idx) = self.core_workspace_index(&tab.workspace_id) else {
            return;
        };
        let Some(tab_idx) = self.core_tab_index(ws_idx, tab.tab_number) else {
            return;
        };
        if let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) {
            let _ = self.handle_tab_close(String::new(), TabTarget { tab_id });
        }
    }

    /// Takes the app's name unless the workspace was renamed here since the
    /// mirror last named it.
    fn name_core_workspace(&mut self, ws_idx: usize, name: Option<&str>) {
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let current = self.state.workspaces[ws_idx].custom_name.clone();
        if let Some(applied) = self.core_mirror.applied_workspace_names.get(&workspace_id) {
            if *applied != current {
                return;
            }
        }
        let desired = name.map(str::to_owned);
        self.core_mirror
            .applied_workspace_names
            .insert(workspace_id, desired.clone());
        if current != desired {
            self.state.workspaces[ws_idx].custom_name = desired;
            self.schedule_session_save();
        }
    }

    fn name_core_tab(&mut self, ws_idx: usize, tab_idx: usize, name: Option<&str>) {
        let Some(key) = self.core_tab_key(ws_idx, tab_idx) else {
            return;
        };
        let current = self.state.workspaces[ws_idx].tabs[tab_idx]
            .custom_name
            .clone();
        if let Some(applied) = self.core_mirror.applied_tab_names.get(&key) {
            if *applied != current {
                return;
            }
        }
        let desired = name.map(str::to_owned);
        self.core_mirror
            .applied_tab_names
            .insert(key, desired.clone());
        if current != desired {
            self.state.workspaces[ws_idx].tabs[tab_idx].custom_name = desired;
            self.schedule_session_save();
        }
    }

    fn core_tab_name_set_here(&self, ws_idx: usize, tab_idx: usize) -> Option<String> {
        let key = self.core_tab_key(ws_idx, tab_idx)?;
        let current = self.state.workspaces[ws_idx].tabs[tab_idx]
            .custom_name
            .clone();
        let applied = self.core_mirror.applied_tab_names.get(&key)?;
        (*applied != current).then_some(current).flatten()
    }

    fn core_tab_key(&self, ws_idx: usize, tab_idx: usize) -> Option<(String, usize)> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        Some((workspace.id.clone(), workspace.tabs.get(tab_idx)?.number))
    }

    /// Pending tabs and spaces whose workspace or tab is gone here (closed, or
    /// its program ended) stop being waited for.
    fn forget_vanished_core_tabs(&mut self) {
        let vanished: Vec<String> = self
            .core_mirror
            .tabs
            .iter()
            .filter(|(_, tab)| {
                self.core_workspace_index(&tab.workspace_id)
                    .and_then(|ws_idx| self.core_tab_index(ws_idx, tab.tab_number))
                    .is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in vanished {
            if self.core_mirror.pending_tabs.remove(&id) {
                self.core_mirror.tabs.remove(&id);
            }
        }
        let vanished: Vec<String> = self
            .core_mirror
            .spaces
            .iter()
            .filter(|(_, workspace_id)| self.core_workspace_index(workspace_id).is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in vanished {
            if self.core_mirror.pending_spaces.remove(&id) {
                self.core_mirror.spaces.remove(&id);
            }
        }
    }

    fn core_pane_host(&self) -> CorePaneHost {
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        CorePaneHost {
            rows,
            cols,
            scrollback_limit_bytes: self.state.pane_scrollback_limit_bytes,
            host_terminal_theme: self.state.host_terminal_theme,
            host_terminal_appearance: self.state.host_terminal_appearance,
        }
    }

    fn core_workspace_index(&self, workspace_id: &str) -> Option<usize> {
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
    }

    fn core_tab_index(&self, ws_idx: usize, tab_number: usize) -> Option<usize> {
        self.state
            .workspaces
            .get(ws_idx)?
            .tabs
            .iter()
            .position(|tab| tab.number == tab_number)
    }

    fn core_panes_showing(&self, core_terminal: &str) -> Vec<PaneId> {
        self.state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.iter())
            .filter(|(_, pane)| {
                self.terminal_runtimes
                    .get(&pane.attached_terminal_id)
                    .and_then(|runtime| runtime.core_terminal())
                    == Some(core_terminal)
            })
            .map(|(pane_id, _)| *pane_id)
            .collect()
    }

    fn report_cached_core_state(&mut self, pane_id: PaneId, core_terminal: &str) {
        if let Some(status) = self.core_mirror.statuses.get(core_terminal).cloned() {
            self.report_core_status(pane_id, &status);
        }
        if let Some(cwd) = self.core_mirror.cwds.get(core_terminal).cloned() {
            self.report_core_cwd(pane_id, PathBuf::from(cwd));
        }
        if let Some(tokens) = self.core_mirror.tokens.get(core_terminal).cloned() {
            let patch = tokens.into_iter().map(|(k, v)| (k, Some(v))).collect();
            self.report_core_tokens(pane_id, patch);
        }
    }

    pub(crate) fn apply_core_tokens(&mut self, update: CoreTokens) {
        let previous = self
            .core_mirror
            .tokens
            .remove(&update.terminal)
            .unwrap_or_default();
        let patch = token_patch(&previous, &update.tokens);
        if !patch.is_empty() {
            for pane_id in self.core_panes_showing(&update.terminal) {
                self.report_core_tokens(pane_id, patch.clone());
            }
        }
        if !update.tokens.is_empty() {
            self.core_mirror
                .tokens
                .insert(update.terminal, update.tokens);
        }
    }

    fn report_core_tokens(&mut self, pane_id: PaneId, tokens: HashMap<String, Option<String>>) {
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.pane_state(pane_id).is_some())
        else {
            return;
        };
        let Some(public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return;
        };
        let params = PaneReportMetadataParams {
            pane_id: public_id,
            source: STATUS_SOURCE.to_owned(),
            agent: None,
            applies_to_source: None,
            title: None,
            display_agent: None,
            state_labels: HashMap::new(),
            tokens,
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            seq: None,
            ttl_ms: None,
        };
        let response = self.handle_pane_report_metadata("core-mirror".to_owned(), params);
        if !response_ok(&response) {
            warn!(response = %response, "failed to mirror herdr-core tokens");
        }
    }

    pub(crate) fn apply_core_cwd(&mut self, update: CoreCwd) {
        for pane_id in self.core_panes_showing(&update.terminal) {
            self.report_core_cwd(pane_id, PathBuf::from(&update.cwd));
        }
        self.core_mirror.cwds.insert(update.terminal, update.cwd);
    }

    /// The folder may be on another machine, so unlike an OSC 7 report it is
    /// not checked against this file system.
    fn report_core_cwd(&mut self, pane_id: PaneId, cwd: PathBuf) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return;
        };
        if let Some(runtime) = self.terminal_runtimes.get(&terminal_id) {
            runtime.set_core_cwd(cwd.clone());
        }
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.cwd = cwd;
        }
    }

    fn report_core_status(
        &mut self,
        pane_id: PaneId,
        status: &CoreStatus,
    ) -> Vec<crate::app::actions::PaneStateUpdate> {
        let previous_app = self
            .core_mirror
            .statuses
            .get(&status.terminal)
            .and_then(|previous| previous.app.clone());
        match core_status_event(pane_id, status, previous_app.as_deref()) {
            Some(event) => self.handle_internal_event_with_pane_updates(event),
            None => Vec::new(),
        }
    }

    /// Changes to mirrored workspaces also go to herdr-core, so the app shows
    /// them: new tabs, splits and spaces become core terminals carrying the
    /// app's adoption labels, and closing ends the terminals, as in the app.
    /// Other requests come back for the normal handlers.
    pub(crate) fn route_core_mirror_request(
        &mut self,
        request: Request,
    ) -> Result<String, Box<Request>> {
        if !self.core_mirror.enabled {
            return Err(Box::new(request));
        }
        let Request { id, method } = request;
        match method {
            Method::TabCreate(params) => match self.core_tab_create_target(&params) {
                Some((ws_idx, space_id)) => {
                    Ok(self.create_core_tab_here(id, params, ws_idx, space_id))
                }
                None => Err(Box::new(Request {
                    id,
                    method: Method::TabCreate(params),
                })),
            },
            Method::PaneSplit(params) => match self.core_split_target(&params) {
                Some(target) => Ok(self.split_core_pane_here(id, params, target)),
                None => Err(Box::new(Request {
                    id,
                    method: Method::PaneSplit(params),
                })),
            },
            Method::WorkspaceCreate(params) => self.create_core_space_here(id, params),
            Method::WorkspaceRename(params) => {
                let target = self.parse_workspace_id(&params.workspace_id);
                let label = params.label.clone();
                let response = self.handle_workspace_rename(id, params);
                if let Some(ws_idx) = target.filter(|_| response_ok(&response)) {
                    let terminals = self.core_terminals_in(ws_idx, None);
                    if !terminals.is_empty() {
                        let workspace_id = self.state.workspaces[ws_idx].id.clone();
                        let current = self.state.workspaces[ws_idx].custom_name.clone();
                        self.core_mirror
                            .applied_workspace_names
                            .insert(workspace_id, current);
                        crate::core_mirror::label_terminals(
                            terminals,
                            crate::core_mirror::SPACE_NAME_LABEL,
                            label,
                        );
                    }
                }
                Ok(response)
            }
            Method::TabRename(params) => {
                let target = self.parse_tab_id(&params.tab_id);
                let label = params.label.clone();
                let response = self.handle_tab_rename(id, params);
                if let Some((ws_idx, tab_idx)) = target.filter(|_| response_ok(&response)) {
                    let panes = self.state.workspaces[ws_idx].tabs[tab_idx]
                        .layout
                        .pane_ids();
                    let terminals = self.core_terminals_in(ws_idx, Some(&panes));
                    if let Some(key) = self
                        .core_tab_key(ws_idx, tab_idx)
                        .filter(|_| !terminals.is_empty())
                    {
                        let current = self.state.workspaces[ws_idx].tabs[tab_idx]
                            .custom_name
                            .clone();
                        self.core_mirror.applied_tab_names.insert(key, current);
                        crate::core_mirror::label_terminals(
                            terminals,
                            crate::core_mirror::TAB_NAME_LABEL,
                            label,
                        );
                    }
                }
                Ok(response)
            }
            Method::PaneClose(target) => {
                let ending = self
                    .parse_pane_id(&target.pane_id)
                    .map(|(ws_idx, pane_id)| self.core_terminals_in(ws_idx, Some(&[pane_id])))
                    .unwrap_or_default();
                let response = self.handle_pane_close(id, target);
                Ok(self.after_core_close(ending, response))
            }
            Method::TabClose(target) => {
                let ending = self
                    .parse_tab_id(&target.tab_id)
                    .and_then(|(ws_idx, tab_idx)| {
                        let panes: Vec<PaneId> = self.state.workspaces[ws_idx]
                            .tabs
                            .get(tab_idx)?
                            .layout
                            .pane_ids();
                        Some(self.core_terminals_in(ws_idx, Some(&panes)))
                    })
                    .unwrap_or_default();
                let response = self.handle_tab_close(id, target);
                Ok(self.after_core_close(ending, response))
            }
            Method::WorkspaceClose(params) => {
                let ending = self
                    .parse_workspace_id(&params.workspace_id)
                    .map(|ws_idx| self.core_terminals_in(ws_idx, None))
                    .unwrap_or_default();
                let response = self.handle_workspace_close(id, params);
                Ok(self.after_core_close(ending, response))
            }
            method => Err(Box::new(Request { id, method })),
        }
    }

    fn create_core_tab_here(
        &mut self,
        id: String,
        params: TabCreateParams,
        ws_idx: usize,
        space_id: String,
    ) -> String {
        let tab_id = crate::core_mirror::new_app_id("tb");
        let cwd = params.cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let names: Vec<(&str, &str)> = params
            .label
            .as_deref()
            .map(|label| (crate::core_mirror::TAB_NAME_LABEL, label))
            .into_iter()
            .collect();
        let terminal = match run_core_terminal(&space_id, &tab_id, &cwd, &params.env, &names) {
            Ok(terminal) => terminal,
            Err(err) => return encode_error(id, "tab_create_failed", err.to_string()),
        };
        let tab = CoreTab {
            id: tab_id.clone(),
            name: None,
            root: CoreNode::Terminal(terminal.clone()),
        };
        let tab_idx = match self.create_core_tab(ws_idx, &tab) {
            Ok(tab_idx) => tab_idx,
            Err(err) => {
                crate::core_mirror::end_terminals(vec![terminal]);
                return encode_error(id, "tab_create_failed", err.to_string());
            }
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_number = self.state.workspaces[ws_idx].tabs[tab_idx].number;
        self.core_mirror.tabs.insert(
            tab_id.clone(),
            MirroredTab {
                workspace_id,
                tab_number,
                signature: terminal,
            },
        );
        self.core_mirror.pending_tabs.insert(tab_id);
        if let Some(key) = self.core_tab_key(ws_idx, tab_idx) {
            self.core_mirror
                .applied_tab_names
                .insert(key, params.label.clone());
        }
        if let Some(label) = params.label {
            self.state.workspaces[ws_idx].tabs[tab_idx].custom_name = Some(label);
        }
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = Mode::Terminal;
        }
        match self.tab_created_result(ws_idx, tab_idx) {
            Some(result) => encode_success(id, result),
            None => encode_error(id, "tab_create_failed", "new tab has no root pane"),
        }
    }

    fn split_core_pane_here(
        &mut self,
        id: String,
        params: PaneSplitParams,
        (ws_idx, target, space_id, tab_id): (usize, PaneId, String, String),
    ) -> String {
        let cwd = params.cwd.map(PathBuf::from).unwrap_or_else(|| {
            let follow = self.launch_cwd_for_pane_in_workspace(ws_idx, target);
            self.resolve_new_terminal_cwd(follow)
        });
        let terminal = match run_core_terminal(&space_id, &tab_id, &cwd, &params.env, &[]) {
            Ok(terminal) => terminal,
            Err(err) => return encode_error(id, "pane_split_failed", err.to_string()),
        };
        let direction = match params.direction {
            SplitDirection::Right => Direction::Horizontal,
            SplitDirection::Down => Direction::Vertical,
        };
        let ratio = params.ratio.unwrap_or(0.5);
        let previous_focus = self.state.current_pane_focus_target();
        let pane_id = match self.split_core_pane(ws_idx, target, direction, ratio, &terminal) {
            Ok(pane_id) => pane_id,
            Err(err) => {
                crate::core_mirror::end_terminals(vec![terminal]);
                return encode_error(id, "pane_split_failed", err.to_string());
            }
        };
        if let Some(mirrored) = self.core_mirror.tabs.get_mut(&tab_id) {
            mirrored.signature =
                crate::core_mirror::signature_with_adopted(&mirrored.signature, &terminal);
        }
        if params.focus {
            if let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(pane_id) {
                self.state.switch_workspace_tab(ws_idx, tab_idx);
                if let Some(tab) = self.state.workspaces[ws_idx].tabs.get_mut(tab_idx) {
                    tab.layout.focus_pane(pane_id);
                }
                self.state
                    .record_pane_focus_change(previous_focus, ws_idx, pane_id);
                self.state.mode = Mode::Terminal;
            }
        }
        match self.pane_info(ws_idx, pane_id) {
            Some(pane) => encode_success(id, ResponseResult::PaneInfo { pane }),
            None => encode_error(id, "pane_split_failed", "new pane is missing"),
        }
    }

    /// New workspaces become app spaces. If herdr-core can't start one, the
    /// request falls back to an ordinary local workspace.
    fn create_core_space_here(
        &mut self,
        id: String,
        params: WorkspaceCreateParams,
    ) -> Result<String, Box<Request>> {
        let space_id = crate::core_mirror::new_app_id("sp");
        let tab_id = crate::core_mirror::new_app_id("tb");
        let cwd = params
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.resolve_new_terminal_cwd(None));
        let names: Vec<(&str, &str)> = params
            .label
            .as_deref()
            .map(|label| (crate::core_mirror::SPACE_NAME_LABEL, label))
            .into_iter()
            .collect();
        let terminal = match run_core_terminal(&space_id, &tab_id, &cwd, &params.env, &names) {
            Ok(terminal) => terminal,
            Err(err) => {
                warn!(err = %err, "herdr-core could not start a space; creating a local workspace");
                return Err(Box::new(Request {
                    id,
                    method: Method::WorkspaceCreate(params),
                }));
            }
        };
        let tab = CoreTab {
            id: tab_id.clone(),
            name: None,
            root: CoreNode::Terminal(terminal.clone()),
        };
        let ws_idx = match self.create_core_workspace(&tab, cwd) {
            Ok(ws_idx) => ws_idx,
            Err(err) => {
                crate::core_mirror::end_terminals(vec![terminal]);
                return Ok(encode_error(id, "workspace_create_failed", err.to_string()));
            }
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_number = self.state.workspaces[ws_idx].tabs[0].number;
        self.core_mirror
            .spaces
            .insert(space_id.clone(), workspace_id.clone());
        self.core_mirror.pending_spaces.insert(space_id);
        self.core_mirror.tabs.insert(
            tab_id.clone(),
            MirroredTab {
                workspace_id,
                tab_number,
                signature: terminal,
            },
        );
        self.core_mirror.pending_tabs.insert(tab_id);
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        self.core_mirror
            .applied_workspace_names
            .insert(workspace_id, params.label.clone());
        if let Some(label) = params.label {
            self.state.workspaces[ws_idx].custom_name = Some(label);
        }
        if params.focus {
            self.state.switch_workspace(ws_idx);
            self.state.mode = Mode::Terminal;
        }
        Ok(match self.workspace_created_result(ws_idx) {
            Some(result) => encode_success(id, result),
            None => encode_error(
                id,
                "workspace_create_failed",
                "new workspace has no root pane",
            ),
        })
    }

    fn core_tab_create_target(&self, params: &TabCreateParams) -> Option<(usize, String)> {
        let ws_idx = match params.workspace_id.as_deref() {
            Some(workspace_id) => self.parse_workspace_id(workspace_id)?,
            None => self.state.active?,
        };
        Some((ws_idx, self.core_space_of(ws_idx)?))
    }

    fn core_split_target(
        &self,
        params: &PaneSplitParams,
    ) -> Option<(usize, PaneId, String, String)> {
        let (ws_idx, pane_id) = if let Some(pane_id) = params.target_pane_id.as_deref() {
            self.parse_pane_id(pane_id)?
        } else {
            let ws_idx = match params.workspace_id.as_deref() {
                Some(workspace_id) => self.parse_workspace_id(workspace_id)?,
                None => self.state.active?,
            };
            (
                ws_idx,
                self.state.workspaces.get(ws_idx)?.focused_pane_id()?,
            )
        };
        let space_id = self.core_space_of(ws_idx)?;
        let workspace = &self.state.workspaces[ws_idx];
        let tab_number = workspace.tabs[workspace.find_tab_index_for_pane(pane_id)?].number;
        let tab_id = self
            .core_mirror
            .tabs
            .iter()
            .find(|(_, tab)| tab.workspace_id == workspace.id && tab.tab_number == tab_number)
            .map(|(id, _)| id.clone())?;
        Some((ws_idx, pane_id, space_id, tab_id))
    }

    fn core_space_of(&self, ws_idx: usize) -> Option<String> {
        let workspace_id = &self.state.workspaces.get(ws_idx)?.id;
        self.core_mirror
            .spaces
            .iter()
            .find(|(_, mirrored)| *mirrored == workspace_id)
            .map(|(space_id, _)| space_id.clone())
    }

    /// The core terminals shown in a mirrored workspace, or in some of its
    /// panes. Attached panes in local workspaces are views and stay running.
    fn core_terminals_in(&self, ws_idx: usize, panes: Option<&[PaneId]>) -> Vec<String> {
        if self.core_space_of(ws_idx).is_none() {
            return Vec::new();
        }
        self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter(|(pane_id, _)| panes.is_none_or(|panes| panes.contains(pane_id)))
            .filter_map(|(_, pane)| {
                self.terminal_runtimes
                    .get(&pane.attached_terminal_id)
                    .and_then(|runtime| runtime.core_terminal())
                    .map(str::to_owned)
            })
            .collect()
    }

    /// After a close here: tabs and spaces that went away are not rebuilt
    /// while their programs end, and the programs are ended.
    fn after_core_close(&mut self, ending: Vec<String>, response: String) -> String {
        if !response_ok(&response) {
            return response;
        }
        let gone_tabs: Vec<String> = self
            .core_mirror
            .tabs
            .iter()
            .filter(|(_, tab)| {
                self.core_workspace_index(&tab.workspace_id)
                    .and_then(|ws_idx| self.core_tab_index(ws_idx, tab.tab_number))
                    .is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for tab_id in gone_tabs {
            self.core_mirror.tabs.remove(&tab_id);
            self.core_mirror.pending_tabs.remove(&tab_id);
            self.core_mirror.closed_tabs.insert(tab_id);
        }
        let gone_spaces: Vec<String> = self
            .core_mirror
            .spaces
            .iter()
            .filter(|(_, workspace_id)| self.core_workspace_index(workspace_id).is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for space_id in gone_spaces {
            self.core_mirror.spaces.remove(&space_id);
            self.core_mirror.pending_spaces.remove(&space_id);
            self.core_mirror.closed_spaces.insert(space_id);
        }
        crate::core_mirror::end_terminals(ending);
        response
    }
}

/// The core clears the status of a terminal when its program stops reporting
/// or ends. Core panes have no local detection, so an empty status must clear
/// the agent. If the cached app fills the gap, the agent stays forever.
fn core_status_event(
    pane_id: PaneId,
    status: &CoreStatus,
    previous_app: Option<&str>,
) -> Option<AppEvent> {
    if status.app.is_none() && status.state.is_none() {
        return Some(AppEvent::HookAuthorityCleared {
            pane_id,
            source: Some(STATUS_SOURCE.to_owned()),
            seq: None,
        });
    }
    let agent_label = status
        .app
        .as_deref()
        .or(previous_app)
        .and_then(super::super::api_helpers::normalize_reported_agent_label)?;
    let state = match status.state.as_deref() {
        Some("working") => AgentState::Working,
        Some("blocked") => AgentState::Blocked,
        Some(_) => AgentState::Idle,
        None => AgentState::Unknown,
    };
    Some(AppEvent::HookStateReported {
        pane_id,
        source: STATUS_SOURCE.to_owned(),
        agent_label,
        state,
        message: status.message.clone(),
        seq: None,
        session_ref: None,
    })
}

/// What changed between two sets of label tokens, as a metadata patch: a
/// removed label clears its token.
fn token_patch(
    previous: &std::collections::BTreeMap<String, String>,
    current: &std::collections::BTreeMap<String, String>,
) -> HashMap<String, Option<String>> {
    let removed = previous
        .keys()
        .filter(|name| !current.contains_key(*name))
        .map(|name| (name.clone(), None));
    let changed = current
        .iter()
        .filter(|(name, value)| previous.get(*name) != Some(*value))
        .map(|(name, value)| (name.clone(), Some(value.clone())));
    removed.chain(changed).collect()
}

fn response_ok(response: &str) -> bool {
    serde_json::from_str::<Value>(response).is_ok_and(|value| value.get("error").is_none())
}

/// Starts a shell in herdr-core labeled so the app adopts it into `tab_id` of
/// `space_id`, creating either when it doesn't know them yet. A space on
/// another machine gets its terminal there, labeled with that machine's ids.
fn run_core_terminal(
    space_id: &str,
    tab_id: &str,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
    names: &[(&str, &str)],
) -> std::io::Result<String> {
    let (machine, space_id) = crate::core_mirror::split_machine(space_id);
    let (_, tab_id) = crate::core_mirror::split_machine(tab_id);
    let mut labels = serde_json::Map::new();
    labels.insert("herdr.space".into(), space_id.into());
    labels.insert("herdr.tab".into(), tab_id.into());
    for (key, name) in names {
        labels.insert((*key).into(), (*name).into());
    }
    let mut params = json!({
        "cwd": cwd,
        "env": env,
        "keep": true,
        "no_parent": true,
        "labels": labels,
    });
    if let Some(machine) = machine {
        params["on"] = machine.into();
    }
    let result = crate::core_mirror::call("terminal.run", params)?;
    result
        .get("address")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| std::io::Error::other("herdr-core did not return the new terminal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(app: Option<&str>, state: Option<&str>) -> CoreStatus {
        CoreStatus {
            terminal: "t7".into(),
            app: app.map(str::to_owned),
            state: state.map(str::to_owned),
            message: None,
        }
    }

    #[test]
    fn empty_status_clears_the_core_agent_even_with_a_cached_app() {
        let pane = PaneId::from_raw(3);
        let event = core_status_event(pane, &status(None, None), Some("claude-code"));
        assert!(matches!(
            event,
            Some(AppEvent::HookAuthorityCleared { pane_id, source: Some(ref source), seq: None })
                if pane_id == pane && source == STATUS_SOURCE
        ));
    }

    #[test]
    fn state_without_app_keeps_the_cached_agent() {
        let pane = PaneId::from_raw(3);
        let event = core_status_event(pane, &status(None, Some("working")), Some("claude-code"));
        assert!(matches!(
            event,
            Some(AppEvent::HookStateReported { ref source, ref agent_label, state: AgentState::Working, .. })
                if source == STATUS_SOURCE && agent_label == "claude"
        ));
    }

    #[test]
    fn token_patch_sets_changes_and_clears_removed_labels() {
        let previous = std::collections::BTreeMap::from([
            ("task".to_owned(), "Old".to_owned()),
            ("pr".to_owned(), "#1".to_owned()),
            ("keep".to_owned(), "same".to_owned()),
        ]);
        let current = std::collections::BTreeMap::from([
            ("task".to_owned(), "New".to_owned()),
            ("keep".to_owned(), "same".to_owned()),
            ("fresh".to_owned(), "x".to_owned()),
        ]);
        let patch = token_patch(&previous, &current);
        assert_eq!(
            patch,
            HashMap::from([
                ("task".to_owned(), Some("New".to_owned())),
                ("pr".to_owned(), None),
                ("fresh".to_owned(), Some("x".to_owned())),
            ])
        );
        assert!(token_patch(&current, &current).is_empty());
    }

    #[test]
    fn state_from_an_unknown_program_reports_nothing() {
        let event = core_status_event(PaneId::from_raw(3), &status(None, Some("working")), None);
        assert!(event.is_none());
    }
}
