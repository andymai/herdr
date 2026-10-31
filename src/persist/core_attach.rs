//! Panes that show herdr-core terminals are views of the core's state, so the
//! session file leaves them out: the core keeps the programs, and the mirror
//! rebuilds its workspaces on start. Restoring them would start stray shells.

use std::collections::HashSet;

use crate::terminal::TerminalRuntimeRegistry;
use crate::workspace::Workspace;

use super::snapshot::{LayoutSnapshot, SessionSnapshot, WorkspaceSnapshot};

pub(super) fn prune_core_panes(
    snapshot: &mut SessionSnapshot,
    workspaces: &[Workspace],
    terminal_runtimes: &TerminalRuntimeRegistry,
) {
    let core_panes: Vec<HashSet<u32>> = workspaces
        .iter()
        .map(|workspace| {
            workspace
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter())
                .filter(|(_, pane)| {
                    terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .and_then(|runtime| runtime.core_terminal())
                        .is_some()
                })
                .map(|(pane_id, _)| pane_id.raw())
                .collect()
        })
        .collect();
    prune_snapshot(snapshot, &core_panes);
}

/// `core_panes[i]` lists the pane ids of workspace `i` to leave out.
fn prune_snapshot(snapshot: &mut SessionSnapshot, core_panes: &[HashSet<u32>]) {
    if core_panes.iter().all(HashSet::is_empty) {
        return;
    }
    let mut kept = Vec::with_capacity(snapshot.workspaces.len());
    let mut new_index = Vec::with_capacity(snapshot.workspaces.len());
    for (idx, mut workspace) in std::mem::take(&mut snapshot.workspaces)
        .into_iter()
        .enumerate()
    {
        if let Some(core) = core_panes.get(idx).filter(|core| !core.is_empty()) {
            prune_workspace(&mut workspace, core);
        }
        if workspace.tabs.is_empty() {
            new_index.push(None);
        } else {
            new_index.push(Some(kept.len()));
            kept.push(workspace);
        }
    }
    let remap = |idx: usize| {
        new_index
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| (!kept.is_empty()).then_some(0))
    };
    snapshot.active = snapshot.active.and_then(remap);
    snapshot.selected = remap(snapshot.selected).unwrap_or(0);
    snapshot.workspaces = kept;
}

fn prune_workspace(workspace: &mut WorkspaceSnapshot, core: &HashSet<u32>) {
    let numbered = workspace.public_tab_numbers.len() == workspace.tabs.len();
    let numbers = std::mem::take(&mut workspace.public_tab_numbers);
    let mut tabs = Vec::with_capacity(workspace.tabs.len());
    let mut kept_numbers = Vec::new();
    let mut active_tab = 0;
    for (idx, mut tab) in std::mem::take(&mut workspace.tabs).into_iter().enumerate() {
        tab.panes.retain(|pane_id, _| !core.contains(pane_id));
        let layout = std::mem::replace(&mut tab.layout, LayoutSnapshot::Pane(0));
        let Some(layout) = prune_layout(layout, core) else {
            continue;
        };
        let first = first_pane(&layout);
        tab.layout = layout;
        for pane in [&mut tab.root_pane, &mut tab.focused] {
            if pane.is_some_and(|id| core.contains(&id)) {
                *pane = Some(first);
            }
        }
        if idx == workspace.active_tab {
            active_tab = tabs.len();
        }
        if numbered {
            kept_numbers.push(numbers[idx]);
        }
        tabs.push(tab);
    }
    workspace.tabs = tabs;
    workspace.public_tab_numbers = if numbered { kept_numbers } else { numbers };
    workspace.active_tab = active_tab;
    workspace
        .public_pane_numbers
        .retain(|pane_id, _| !core.contains(pane_id));
    if let Some(cwd) = workspace
        .tabs
        .first()
        .and_then(|tab| tab.root_pane.and_then(|id| tab.panes.get(&id)))
        .map(|pane| pane.cwd.clone())
    {
        workspace.identity_cwd = cwd;
    }
}

fn prune_layout(layout: LayoutSnapshot, core: &HashSet<u32>) -> Option<LayoutSnapshot> {
    match layout {
        LayoutSnapshot::Pane(id) => (!core.contains(&id)).then_some(LayoutSnapshot::Pane(id)),
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => match (prune_layout(*first, core), prune_layout(*second, core)) {
            (Some(first), Some(second)) => Some(LayoutSnapshot::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

fn first_pane(layout: &LayoutSnapshot) -> u32 {
    match layout {
        LayoutSnapshot::Pane(id) => *id,
        LayoutSnapshot::Split { first, .. } => first_pane(first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cwd: &str) -> serde_json::Value {
        serde_json::json!({"cwd": cwd})
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "active": 1,
            "selected": 1,
            "workspaces": [
                {
                    "id": "local", "identity_cwd": "/a", "public_tab_numbers": [1, 2],
                    "public_pane_numbers": {"1": 1, "2": 2, "3": 3},
                    "next_public_pane_number": 4, "next_public_tab_number": 3, "active_tab": 1,
                    "tabs": [
                        {"layout": {"Pane": 1}, "panes": {"1": pane("/a")}, "zoomed": false,
                         "focused": 1, "root_pane": 1},
                        {"layout": {"Split": {"direction": "Horizontal", "ratio": 0.5,
                            "first": {"Pane": 2}, "second": {"Pane": 3}}},
                         "panes": {"2": pane("/core"), "3": pane("/b")}, "zoomed": false,
                         "focused": 2, "root_pane": 2}
                    ]
                },
                {
                    "id": "mirrored", "identity_cwd": "/m", "public_tab_numbers": [1],
                    "public_pane_numbers": {"1": 1}, "next_public_pane_number": 2,
                    "next_public_tab_number": 2, "active_tab": 0,
                    "tabs": [{"layout": {"Pane": 1}, "panes": {"1": pane("/m")}, "zoomed": false,
                              "focused": 1, "root_pane": 1}]
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn all_core_workspaces_are_dropped_and_indices_follow() {
        let mut snapshot = snapshot();
        prune_snapshot(&mut snapshot, &[HashSet::from([2]), HashSet::from([1])]);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.active, Some(0));
        assert_eq!(snapshot.selected, 0);
    }

    #[test]
    fn a_core_pane_in_a_split_collapses_into_its_sibling() {
        let mut snapshot = snapshot();
        prune_snapshot(&mut snapshot, &[HashSet::from([2]), HashSet::new()]);
        let workspace = &snapshot.workspaces[0];
        let tab = &workspace.tabs[1];
        assert!(matches!(tab.layout, LayoutSnapshot::Pane(3)));
        assert_eq!(tab.root_pane, Some(3));
        assert_eq!(tab.focused, Some(3));
        assert!(!workspace.public_pane_numbers.contains_key(&2));
        assert_eq!(workspace.active_tab, 1);
        assert_eq!(snapshot.workspaces.len(), 2);
    }

    #[test]
    fn an_all_core_tab_is_dropped_with_its_number() {
        let mut snapshot = snapshot();
        prune_snapshot(&mut snapshot, &[HashSet::from([2, 3]), HashSet::new()]);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.public_tab_numbers, vec![1]);
        assert_eq!(workspace.active_tab, 0);
    }
}
