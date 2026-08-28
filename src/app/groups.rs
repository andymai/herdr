use std::collections::HashSet;

use super::state::AppState;
use crate::workspace::{generate_group_id, WorkspaceGroup};

impl AppState {
    pub(crate) fn group_index_by_id(&self, group_id: &str) -> Option<usize> {
        self.groups.iter().position(|group| group.id == group_id)
    }

    /// Member indices in `workspaces` order.
    pub(crate) fn group_member_indices(&self, group_id: &str) -> Vec<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.group_id.as_deref() == Some(group_id))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Grow a workspace-index set to whole worktree spaces, preserving
    /// `workspaces` order and dropping out-of-range indices.
    fn expand_to_space_mates(&self, indices: &[usize]) -> Vec<usize> {
        let direct: HashSet<usize> = indices
            .iter()
            .copied()
            .filter(|idx| *idx < self.workspaces.len())
            .collect();
        let space_keys: HashSet<&str> = direct
            .iter()
            .filter_map(|idx| self.workspaces[*idx].worktree_space())
            .map(|space| space.key.as_str())
            .collect();
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(idx, ws)| {
                direct.contains(idx)
                    || ws
                        .worktree_space()
                        .is_some_and(|space| space_keys.contains(space.key.as_str()))
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Create a group containing `member_indices` (expanded to whole spaces).
    /// Returns the new group id, or `None` for an empty name or no members.
    pub(crate) fn create_group(&mut self, name: &str, member_indices: &[usize]) -> Option<String> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let members = self.expand_to_space_mates(member_indices);
        if members.is_empty() {
            return None;
        }
        let group_id = generate_group_id();
        self.groups.push(WorkspaceGroup {
            id: group_id.clone(),
            name: name.to_string(),
        });
        for idx in members {
            self.workspaces[idx].group_id = Some(group_id.clone());
        }
        self.dissolve_empty_groups();
        self.mark_session_dirty();
        Some(group_id)
    }

    /// Move `member_indices` (expanded to whole spaces) into an existing group.
    pub(crate) fn assign_workspaces_to_group(
        &mut self,
        group_id: &str,
        member_indices: &[usize],
    ) -> bool {
        if self.group_index_by_id(group_id).is_none() {
            return false;
        }
        let members = self.expand_to_space_mates(member_indices);
        let mut changed = false;
        for idx in members {
            if self.workspaces[idx].group_id.as_deref() != Some(group_id) {
                self.workspaces[idx].group_id = Some(group_id.to_string());
                changed = true;
            }
        }
        if changed {
            self.dissolve_empty_groups();
            self.mark_session_dirty();
        }
        changed
    }

    /// Return `member_indices` (expanded to whole spaces) to the top level.
    pub(crate) fn unassign_workspaces(&mut self, member_indices: &[usize]) -> bool {
        let members = self.expand_to_space_mates(member_indices);
        let mut changed = false;
        for idx in members {
            if self.workspaces[idx].group_id.take().is_some() {
                changed = true;
            }
        }
        if changed {
            self.dissolve_empty_groups();
            self.mark_session_dirty();
        }
        changed
    }

    pub(crate) fn rename_group(&mut self, group_id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(idx) = self.group_index_by_id(group_id) else {
            return false;
        };
        if self.groups[idx].name == name {
            return false;
        }
        self.groups[idx].name = name.to_string();
        self.mark_session_dirty();
        true
    }

    /// Remove a group, returning its members to the top level. Never closes
    /// workspaces; closing members is a separate explicit action.
    pub(crate) fn remove_group(&mut self, group_id: &str) -> bool {
        let Some(idx) = self.group_index_by_id(group_id) else {
            return false;
        };
        for ws in &mut self.workspaces {
            if ws.group_id.as_deref() == Some(group_id) {
                ws.group_id = None;
            }
        }
        self.groups.remove(idx);
        self.collapsed_group_ids.remove(group_id);
        self.mark_session_dirty();
        true
    }

    /// Drop groups that no workspace references, and their collapse state.
    /// Callers mark the session dirty as part of the surrounding mutation.
    pub(crate) fn dissolve_empty_groups(&mut self) {
        let referenced: HashSet<String> = self
            .workspaces
            .iter()
            .filter_map(|ws| ws.group_id.clone())
            .collect();
        self.groups.retain(|group| referenced.contains(&group.id));
        let live: HashSet<&str> = self.groups.iter().map(|group| group.id.as_str()).collect();
        self.collapsed_group_ids
            .retain(|id| live.contains(id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use crate::app::state::AppState;
    use crate::workspace::{Workspace, WorktreeSpaceMembership};

    fn app_with_workspaces(names: &[&str]) -> AppState {
        let mut state = AppState::test_new();
        for name in names {
            state.workspaces.push(Workspace::test_new(name));
        }
        state.ensure_test_terminals();
        state.active = Some(0);
        state.selected = 0;
        state
    }

    fn mark_space(state: &mut AppState, idx: usize, key: &str, linked: bool) {
        state.workspaces[idx].worktree_space = Some(WorktreeSpaceMembership {
            key: key.to_string(),
            label: key.to_string(),
            repo_root: std::path::PathBuf::from("/tmp/repo"),
            checkout_path: std::path::PathBuf::from("/tmp/repo"),
            is_linked_worktree: linked,
        });
    }

    #[test]
    fn create_group_assigns_members_and_generates_id() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        let group_id = state.create_group("Client", &[0, 2]).expect("group id");
        assert!(group_id.starts_with('g'));
        assert_eq!(state.groups.len(), 1);
        assert_eq!(
            state.workspaces[0].group_id.as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(state.workspaces[1].group_id, None);
        assert_eq!(
            state.workspaces[2].group_id.as_deref(),
            Some(group_id.as_str())
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn create_group_rejects_empty_name_and_empty_members() {
        let mut state = app_with_workspaces(&["a"]);
        assert_eq!(state.create_group("  ", &[0]), None);
        assert_eq!(state.create_group("Client", &[]), None);
        assert_eq!(state.create_group("Client", &[7]), None);
        assert!(state.groups.is_empty());
    }

    #[test]
    fn assign_expands_to_whole_space() {
        let mut state = app_with_workspaces(&["parent", "linked", "other"]);
        mark_space(&mut state, 0, "repo", false);
        mark_space(&mut state, 1, "repo", true);
        let group_id = state.create_group("Client", &[2]).expect("group id");
        assert!(state.assign_workspaces_to_group(&group_id, &[1]));
        assert_eq!(
            state.workspaces[0].group_id.as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(
            state.workspaces[1].group_id.as_deref(),
            Some(group_id.as_str())
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn moving_last_member_dissolves_previous_group() {
        let mut state = app_with_workspaces(&["a", "b"]);
        let first = state.create_group("First", &[0]).expect("group id");
        let second = state.create_group("Second", &[1]).expect("group id");
        assert!(state.assign_workspaces_to_group(&second, &[0]));
        assert_eq!(state.group_index_by_id(&first), None);
        assert_eq!(state.group_member_indices(&second), vec![0, 1]);
        state.assert_invariants_for_test();
    }

    #[test]
    fn unassign_last_member_dissolves_group_and_collapse_state() {
        let mut state = app_with_workspaces(&["a"]);
        let group_id = state.create_group("Client", &[0]).expect("group id");
        state.collapsed_group_ids.insert(group_id.clone());
        assert!(state.unassign_workspaces(&[0]));
        assert!(state.groups.is_empty());
        assert!(state.collapsed_group_ids.is_empty());
        assert_eq!(state.workspaces[0].group_id, None);
        state.assert_invariants_for_test();
    }

    #[test]
    fn remove_group_ungroups_without_closing() {
        let mut state = app_with_workspaces(&["a", "b"]);
        let group_id = state.create_group("Client", &[0, 1]).expect("group id");
        state.collapsed_group_ids.insert(group_id.clone());
        assert!(state.remove_group(&group_id));
        assert_eq!(state.workspaces.len(), 2);
        assert!(state.groups.is_empty());
        assert!(state.collapsed_group_ids.is_empty());
        assert!(state.workspaces.iter().all(|ws| ws.group_id.is_none()));
        state.assert_invariants_for_test();
    }

    #[test]
    fn rename_group_trims_and_rejects_empty() {
        let mut state = app_with_workspaces(&["a"]);
        let group_id = state.create_group("Client", &[0]).expect("group id");
        assert!(state.rename_group(&group_id, "  Team  "));
        assert_eq!(state.groups[0].name, "Team");
        assert!(!state.rename_group(&group_id, "   "));
        assert!(!state.rename_group("g_missing", "x"));
    }
}
