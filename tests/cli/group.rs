use super::harness::*;

fn create_workspace(socket_path: &std::path::Path, cwd: &std::path::Path) -> String {
    let created = run_cli_json(
        socket_path,
        &["workspace", "create", "--cwd", cwd.to_str().unwrap()],
    );
    created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn group_management_commands_work() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let first = create_workspace(&socket_path, &base);
    let second = create_workspace(&socket_path, &base);

    let created = run_cli(
        &socket_path,
        &["group", "create", "--name", "Client", &first],
    );
    assert!(
        created.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created_json["result"]["type"], "group_info");
    let group_id = created_json["result"]["group"]["group_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(group_id.starts_with('g'));
    assert_eq!(created_json["result"]["group"]["name"], "Client");
    assert_eq!(
        created_json["result"]["group"]["workspace_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let listed = run_cli_json(&socket_path, &["group", "list"]);
    assert_eq!(listed["result"]["type"], "group_list");
    assert_eq!(listed["result"]["groups"].as_array().unwrap().len(), 1);

    let workspaces = run_cli_json(&socket_path, &["workspace", "list"]);
    let grouped = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["workspace_id"] == first.as_str())
        .unwrap();
    assert_eq!(grouped["group_id"], group_id.as_str());

    let assigned = run_cli_json(&socket_path, &["group", "assign", &group_id, &second]);
    assert_eq!(
        assigned["result"]["group"]["workspace_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let renamed = run_cli_json(&socket_path, &["group", "rename", &group_id, "Team"]);
    assert_eq!(renamed["result"]["group"]["name"], "Team");

    let collapsed = run_cli_json(&socket_path, &["group", "collapse", &group_id]);
    assert_eq!(collapsed["result"]["group"]["collapsed"], true);
    let expanded = run_cli_json(&socket_path, &["group", "expand", &group_id]);
    assert_eq!(expanded["result"]["group"]["collapsed"], false);

    let fetched_by_name = run_cli_json(&socket_path, &["group", "get", "Team"]);
    assert_eq!(
        fetched_by_name["result"]["group"]["group_id"],
        group_id.as_str()
    );

    let unassigned = run_cli(&socket_path, &["group", "unassign", &second]);
    assert!(unassigned.status.success());
    let after_unassign = run_cli_json(&socket_path, &["group", "get", &group_id]);
    assert_eq!(
        after_unassign["result"]["group"]["workspace_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let removed = run_cli(&socket_path, &["group", "remove", &group_id]);
    assert!(removed.status.success());
    let after_remove = run_cli_json(&socket_path, &["workspace", "list"]);
    assert_eq!(
        after_remove["result"]["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let groups = run_cli_json(&socket_path, &["group", "list"]);
    assert_eq!(groups["result"]["groups"].as_array().unwrap().len(), 0);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn group_close_closes_all_members() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let first = create_workspace(&socket_path, &base);
    let second = create_workspace(&socket_path, &base);
    let third = create_workspace(&socket_path, &base);

    let created = run_cli_json(
        &socket_path,
        &["group", "create", "--name", "Client", &first, &second],
    );
    let group_id = created["result"]["group"]["group_id"]
        .as_str()
        .unwrap()
        .to_string();

    let missing = run_cli(&socket_path, &["group", "close", "missing"]);
    assert_eq!(missing.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(error["error"]["code"], "group_not_found");

    let closed = run_cli(&socket_path, &["group", "close", &group_id]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    let after_close = run_cli_json(&socket_path, &["workspace", "list"]);
    let remaining = after_close["result"]["workspaces"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["workspace_id"], third.as_str());
    let groups = run_cli_json(&socket_path, &["group", "list"]);
    assert_eq!(groups["result"]["groups"].as_array().unwrap().len(), 0);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn server_restart_restores_groups_and_collapse_state() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let mut herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let first = create_workspace(&socket_path, &base);
    let _second = create_workspace(&socket_path, &base);

    let created = run_cli_json(
        &socket_path,
        &["group", "create", "--name", "Client", &first],
    );
    let group_id = created["result"]["group"]["group_id"]
        .as_str()
        .unwrap()
        .to_string();
    let collapsed = run_cli_json(&socket_path, &["group", "collapse", &group_id]);
    assert_eq!(collapsed["result"]["group"]["collapsed"], true);

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(
        stopped.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let pid = herdr.child.process_id();
    let exit_status = herdr.child.wait().unwrap();
    unregister_spawned_herdr_pid(pid);
    assert!(exit_status.success(), "server stop should exit cleanly");
    drop(herdr);

    let restarted = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let groups = run_cli_json(&socket_path, &["group", "list"]);
    let restored = &groups["result"]["groups"].as_array().unwrap()[0];
    assert_eq!(restored["group_id"], group_id.as_str());
    assert_eq!(restored["name"], "Client");
    assert_eq!(restored["collapsed"], true);
    assert_eq!(restored["workspace_ids"].as_array().unwrap().len(), 1);

    let workspaces = run_cli_json(&socket_path, &["workspace", "list"]);
    let grouped_count = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|workspace| workspace["group_id"] == group_id.as_str())
        .count();
    assert_eq!(grouped_count, 1);

    cleanup_spawned_herdr(restarted, base);
}
