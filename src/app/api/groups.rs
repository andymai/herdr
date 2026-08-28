use crate::api::schema::{
    EventData, EventEnvelope, EventKind, GroupAssignParams, GroupCreateParams, GroupInfo,
    GroupRenameParams, GroupSetCollapsedParams, GroupTarget, GroupUnassignParams, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

enum GroupLookup {
    Found(usize),
    AmbiguousName,
    NotFound,
}

impl App {
    fn lookup_group(&self, id_or_name: &str) -> GroupLookup {
        if let Some(idx) = self
            .state
            .groups
            .iter()
            .position(|group| group.id == id_or_name)
        {
            return GroupLookup::Found(idx);
        }
        let mut matches = self
            .state
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.name == id_or_name)
            .map(|(idx, _)| idx);
        match (matches.next(), matches.next()) {
            (Some(idx), None) => GroupLookup::Found(idx),
            (Some(_), Some(_)) => GroupLookup::AmbiguousName,
            (None, _) => GroupLookup::NotFound,
        }
    }

    fn resolve_group(&self, request_id: &str, group_id: &str) -> Result<usize, String> {
        match self.lookup_group(group_id) {
            GroupLookup::Found(idx) => Ok(idx),
            GroupLookup::AmbiguousName => Err(encode_error(
                request_id.to_string(),
                "group_name_ambiguous",
                format!("multiple groups are named {group_id}; use the group id"),
            )),
            GroupLookup::NotFound => Err(group_not_found(request_id.to_string(), group_id)),
        }
    }

    fn group_info(&self, group_idx: usize) -> GroupInfo {
        let group = &self.state.groups[group_idx];
        GroupInfo {
            group_id: group.id.clone(),
            name: group.name.clone(),
            collapsed: self.state.collapsed_group_ids.contains(&group.id),
            workspace_ids: self
                .state
                .group_member_indices(&group.id)
                .into_iter()
                .map(|idx| self.public_workspace_id(idx))
                .collect(),
        }
    }

    fn resolve_member_indices(
        &self,
        request_id: &str,
        workspace_ids: &[String],
    ) -> Result<Vec<usize>, String> {
        if workspace_ids.is_empty() {
            return Err(encode_error(
                request_id.to_string(),
                "group_member_required",
                "workspace_ids must not be empty",
            ));
        }
        let mut indices = Vec::with_capacity(workspace_ids.len());
        for requested_id in workspace_ids {
            let Some(index) = self.parse_workspace_id(requested_id) else {
                return Err(encode_error(
                    request_id.to_string(),
                    "workspace_not_found",
                    format!("workspace {requested_id} not found"),
                ));
            };
            indices.push(index);
        }
        Ok(indices)
    }

    fn emit_group_updated(&mut self, group_idx: usize) {
        let group = self.group_info(group_idx);
        self.emit_event(EventEnvelope {
            event: EventKind::GroupUpdated,
            data: EventData::GroupUpdated { group },
        });
    }

    pub(super) fn handle_group_list(&mut self, id: String) -> String {
        let groups = (0..self.state.groups.len())
            .map(|idx| self.group_info(idx))
            .collect();
        encode_success(id, ResponseResult::GroupList { groups })
    }

    pub(super) fn handle_group_get(&mut self, id: String, target: GroupTarget) -> String {
        let group_idx = match self.resolve_group(&id, &target.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        encode_success(
            id,
            ResponseResult::GroupInfo {
                group: self.group_info(group_idx),
            },
        )
    }

    pub(super) fn handle_group_create(&mut self, id: String, params: GroupCreateParams) -> String {
        let member_indices = match self.resolve_member_indices(&id, &params.workspace_ids) {
            Ok(indices) => indices,
            Err(response) => return response,
        };
        let Some(group_id) = self.state.create_group(&params.name, &member_indices) else {
            return encode_error(
                id,
                "group_create_failed",
                "group name must not be empty and members must exist",
            );
        };
        self.schedule_session_save();
        let Some(group_idx) = self.state.group_index_by_id(&group_id) else {
            return encode_error(id, "group_create_failed", "group was not created");
        };
        let group = self.group_info(group_idx);
        for ws_idx in self.state.group_member_indices(&group_id) {
            self.emit_workspace_updated(ws_idx);
        }
        self.emit_event(EventEnvelope {
            event: EventKind::GroupCreated,
            data: EventData::GroupCreated {
                group: group.clone(),
            },
        });
        encode_success(id, ResponseResult::GroupInfo { group })
    }

    pub(super) fn handle_group_rename(&mut self, id: String, params: GroupRenameParams) -> String {
        let group_idx = match self.resolve_group(&id, &params.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        let group_id = self.state.groups[group_idx].id.clone();
        if params.name.trim().is_empty() {
            return encode_error(id, "group_rename_failed", "group name must not be empty");
        }
        if self.state.rename_group(&group_id, &params.name) {
            self.schedule_session_save();
            self.emit_group_updated(group_idx);
        }
        encode_success(
            id,
            ResponseResult::GroupInfo {
                group: self.group_info(group_idx),
            },
        )
    }

    pub(super) fn handle_group_assign(&mut self, id: String, params: GroupAssignParams) -> String {
        let group_idx = match self.resolve_group(&id, &params.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        let group_id = self.state.groups[group_idx].id.clone();
        let member_indices = match self.resolve_member_indices(&id, &params.workspace_ids) {
            Ok(indices) => indices,
            Err(response) => return response,
        };
        if self
            .state
            .assign_workspaces_to_group(&group_id, &member_indices)
        {
            self.schedule_session_save();
            for ws_idx in self.state.group_member_indices(&group_id) {
                self.emit_workspace_updated(ws_idx);
            }
            if let Some(group_idx) = self.state.group_index_by_id(&group_id) {
                self.emit_group_updated(group_idx);
            }
        }
        match self.state.group_index_by_id(&group_id) {
            Some(group_idx) => encode_success(
                id,
                ResponseResult::GroupInfo {
                    group: self.group_info(group_idx),
                },
            ),
            None => group_not_found(id, &group_id),
        }
    }

    pub(super) fn handle_group_unassign(
        &mut self,
        id: String,
        params: GroupUnassignParams,
    ) -> String {
        let member_indices = match self.resolve_member_indices(&id, &params.workspace_ids) {
            Ok(indices) => indices,
            Err(response) => return response,
        };
        let previous_groups: Vec<String> = member_indices
            .iter()
            .filter_map(|idx| self.state.workspaces.get(*idx))
            .filter_map(|ws| ws.group_id.clone())
            .collect();
        if self.state.unassign_workspaces(&member_indices) {
            self.schedule_session_save();
            for ws_idx in member_indices {
                self.emit_workspace_updated(ws_idx);
            }
            for group_id in previous_groups {
                match self.state.group_index_by_id(&group_id) {
                    Some(group_idx) => self.emit_group_updated(group_idx),
                    None => self.emit_event(EventEnvelope {
                        event: EventKind::GroupRemoved,
                        data: EventData::GroupRemoved { group_id },
                    }),
                }
            }
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_group_remove(&mut self, id: String, target: GroupTarget) -> String {
        let group_idx = match self.resolve_group(&id, &target.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        let group_id = self.state.groups[group_idx].id.clone();
        let member_indices = self.state.group_member_indices(&group_id);
        if self.state.remove_group(&group_id) {
            self.schedule_session_save();
            for ws_idx in member_indices {
                self.emit_workspace_updated(ws_idx);
            }
            self.emit_event(EventEnvelope {
                event: EventKind::GroupRemoved,
                data: EventData::GroupRemoved { group_id },
            });
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_group_close(&mut self, id: String, target: GroupTarget) -> String {
        let group_idx = match self.resolve_group(&id, &target.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        let group_id = self.state.groups[group_idx].id.clone();
        let member_indices = self.state.group_member_indices(&group_id);
        let closed_workspaces = member_indices
            .iter()
            .map(|index| {
                (
                    self.public_workspace_id(*index),
                    self.workspace_info(*index),
                )
            })
            .collect::<Vec<_>>();
        for (workspace_id, _) in &closed_workspaces {
            if let Some(index) = self.parse_workspace_id(workspace_id) {
                self.state.selected = index;
                self.state.close_selected_workspace();
            }
        }
        self.shutdown_detached_terminal_runtimes();
        for (workspace_id, workspace) in closed_workspaces {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
        }
        self.emit_event(EventEnvelope {
            event: EventKind::GroupRemoved,
            data: EventData::GroupRemoved { group_id },
        });
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_group_set_collapsed(
        &mut self,
        id: String,
        params: GroupSetCollapsedParams,
    ) -> String {
        let group_idx = match self.resolve_group(&id, &params.group_id) {
            Ok(idx) => idx,
            Err(response) => return response,
        };
        let group_id = self.state.groups[group_idx].id.clone();
        let changed = if params.collapsed {
            self.state.collapsed_group_ids.insert(group_id)
        } else {
            self.state.collapsed_group_ids.remove(&group_id)
        };
        if changed {
            self.state.mark_session_dirty();
            self.emit_group_updated(group_idx);
        }
        encode_success(
            id,
            ResponseResult::GroupInfo {
                group: self.group_info(group_idx),
            },
        )
    }
}

fn group_not_found(id: String, group_id: &str) -> String {
    encode_error(id, "group_not_found", format!("group {group_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::SuccessResponse;
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn app_with_workspaces(names: &[&str]) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        app.state.ensure_test_terminals();
        app
    }

    fn mark_space(app: &mut App, idx: usize, key: &str, linked: bool) {
        app.state.workspaces[idx].worktree_space =
            Some(crate::workspace::WorktreeSpaceMembership {
                key: key.into(),
                label: key.into(),
                repo_root: "/repo".into(),
                checkout_path: "/repo".into(),
                is_linked_worktree: linked,
            });
    }

    fn created_group_id(response: &str) -> String {
        let success: SuccessResponse = serde_json::from_str(response).expect("success response");
        match success.result {
            ResponseResult::GroupInfo { group } => group.group_id,
            other => panic!("expected GroupInfo result, got {other:?}"),
        }
    }

    fn error_code(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).expect("json response");
        value["error"]["code"]
            .as_str()
            .expect("error code")
            .to_string()
    }

    fn create_group(app: &mut App, name: &str, workspace_ids: Vec<String>) -> String {
        let response = app.handle_group_create(
            "req".into(),
            GroupCreateParams {
                name: name.into(),
                workspace_ids,
            },
        );
        created_group_id(&response)
    }

    #[test]
    fn api_group_create_requires_members_and_name() {
        let mut app = app_with_workspaces(&["a"]);
        let response = app.handle_group_create(
            "req".into(),
            GroupCreateParams {
                name: "Client".into(),
                workspace_ids: Vec::new(),
            },
        );
        assert_eq!(error_code(&response), "group_member_required");

        let ws_id = app.public_workspace_id(0);
        let response = app.handle_group_create(
            "req".into(),
            GroupCreateParams {
                name: "  ".into(),
                workspace_ids: vec![ws_id],
            },
        );
        assert_eq!(error_code(&response), "group_create_failed");
        assert!(app.state.groups.is_empty());
    }

    #[test]
    fn api_group_create_assigns_members() {
        let mut app = app_with_workspaces(&["a", "b"]);
        let ws_id = app.public_workspace_id(0);
        let group_id = create_group(&mut app, "Client", vec![ws_id]);
        assert!(group_id.starts_with('g'));
        assert_eq!(
            app.state.workspaces[0].group_id.as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(app.state.workspaces[1].group_id, None);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn api_group_assign_expands_to_whole_space() {
        let mut app = app_with_workspaces(&["parent", "linked", "other"]);
        mark_space(&mut app, 0, "repo-key", false);
        mark_space(&mut app, 1, "repo-key", true);
        let other_id = app.public_workspace_id(2);
        let group_id = create_group(&mut app, "Client", vec![other_id]);

        let linked_id = app.public_workspace_id(1);
        let response = app.handle_group_assign(
            "req".into(),
            GroupAssignParams {
                group_id: group_id.clone(),
                workspace_ids: vec![linked_id],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).expect("success");
        match success.result {
            ResponseResult::GroupInfo { group } => {
                assert_eq!(group.workspace_ids.len(), 3);
            }
            other => panic!("expected GroupInfo result, got {other:?}"),
        }
        assert_eq!(
            app.state.workspaces[0].group_id.as_deref(),
            Some(group_id.as_str())
        );
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn api_group_unassign_dissolves_empty_group() {
        let mut app = app_with_workspaces(&["a"]);
        let ws_id = app.public_workspace_id(0);
        let group_id = create_group(&mut app, "Client", vec![ws_id.clone()]);
        app.state.collapsed_group_ids.insert(group_id.clone());

        let response = app.handle_group_unassign(
            "req".into(),
            GroupUnassignParams {
                workspace_ids: vec![ws_id],
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).expect("success");
        assert!(app.state.groups.is_empty());
        assert!(app.state.collapsed_group_ids.is_empty());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn api_group_remove_ungroups_without_closing() {
        let mut app = app_with_workspaces(&["a", "b"]);
        let ids = vec![app.public_workspace_id(0), app.public_workspace_id(1)];
        let group_id = create_group(&mut app, "Client", ids);

        let response = app.handle_group_remove("req".into(), GroupTarget { group_id });
        let _: SuccessResponse = serde_json::from_str(&response).expect("success");
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(app.state.groups.is_empty());
        assert!(app.state.workspaces.iter().all(|ws| ws.group_id.is_none()));
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn api_group_close_closes_space_members_without_worktree_guard() {
        let mut app = app_with_workspaces(&["parent", "linked", "other"]);
        mark_space(&mut app, 0, "repo-key", false);
        mark_space(&mut app, 1, "repo-key", true);
        let parent_id = app.public_workspace_id(0);
        let group_id = create_group(&mut app, "Client", vec![parent_id]);
        assert_eq!(app.state.group_member_indices(&group_id), vec![0, 1]);

        let response = app.handle_group_close("req".into(), GroupTarget { group_id });
        let _: SuccessResponse = serde_json::from_str(&response).expect("success");
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(
            app.state.workspaces[0].custom_name.as_deref(),
            Some("other")
        );
        assert!(app.state.groups.is_empty());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn api_group_resolves_unique_name_and_rejects_ambiguous() {
        let mut app = app_with_workspaces(&["a", "b"]);
        let first = app.public_workspace_id(0);
        let second = app.public_workspace_id(1);
        create_group(&mut app, "Client", vec![first]);

        let response = app.handle_group_get(
            "req".into(),
            GroupTarget {
                group_id: "Client".into(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).expect("success");

        create_group(&mut app, "Client", vec![second]);
        let response = app.handle_group_get(
            "req".into(),
            GroupTarget {
                group_id: "Client".into(),
            },
        );
        assert_eq!(error_code(&response), "group_name_ambiguous");

        let response = app.handle_group_get(
            "req".into(),
            GroupTarget {
                group_id: "missing".into(),
            },
        );
        assert_eq!(error_code(&response), "group_not_found");
    }

    #[test]
    fn api_group_set_collapsed_round_trips() {
        let mut app = app_with_workspaces(&["a"]);
        let ws_id = app.public_workspace_id(0);
        let group_id = create_group(&mut app, "Client", vec![ws_id]);

        let response = app.handle_group_set_collapsed(
            "req".into(),
            GroupSetCollapsedParams {
                group_id: group_id.clone(),
                collapsed: true,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).expect("success");
        match success.result {
            ResponseResult::GroupInfo { group } => assert!(group.collapsed),
            other => panic!("expected GroupInfo result, got {other:?}"),
        }
        assert!(app.state.collapsed_group_ids.contains(&group_id));

        let response = app.handle_group_set_collapsed(
            "req".into(),
            GroupSetCollapsedParams {
                group_id: group_id.clone(),
                collapsed: false,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).expect("success");
        assert!(app.state.collapsed_group_ids.is_empty());
    }
}
