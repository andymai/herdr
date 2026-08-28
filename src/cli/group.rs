use crate::api::schema::{
    GroupAssignParams, GroupCreateParams, GroupRenameParams, GroupSetCollapsedParams,
    GroupUnassignParams,
};

pub(super) fn run_group_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_group_help();
        return Ok(2);
    };

    match subcommand {
        "list" => group_list(&args[1..]),
        "get" => group_get(&args[1..]),
        "create" => group_create(&args[1..]),
        "rename" => group_rename(&args[1..]),
        "assign" => group_assign(&args[1..]),
        "unassign" => group_unassign(&args[1..]),
        "remove" => group_remove(&args[1..]),
        "close" => group_close(&args[1..]),
        "collapse" => group_set_collapsed(&args[1..], true),
        "expand" => group_set_collapsed(&args[1..], false),
        "help" | "--help" | "-h" => {
            print_group_help();
            Ok(0)
        }
        _ => {
            print_group_help();
            Ok(2)
        }
    }
}

fn group_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr group list");
        return Ok(2);
    }

    super::runtime::group_list()
}

fn group_get(args: &[String]) -> std::io::Result<i32> {
    let [group_id] = args else {
        eprintln!("usage: herdr group get <group>");
        return Ok(2);
    };

    super::runtime::group_get(group_id.clone())
}

fn group_create(args: &[String]) -> std::io::Result<i32> {
    let mut name = None;
    let mut workspace_ids = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--name" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --name");
                    return Ok(2);
                };
                name = Some(value.clone());
                index += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
            workspace_id => {
                workspace_ids.push(super::normalize_workspace_id(workspace_id));
                index += 1;
            }
        }
    }

    let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
        eprintln!("missing required --name");
        return Ok(2);
    };
    if workspace_ids.is_empty() {
        eprintln!("usage: herdr group create --name NAME <workspace>...");
        return Ok(2);
    }

    super::runtime::group_create(GroupCreateParams {
        name,
        workspace_ids,
    })
}

fn group_rename(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr group rename <group> <name>");
        return Ok(2);
    }

    super::runtime::group_rename(GroupRenameParams {
        group_id: args[0].clone(),
        name: args[1..].join(" "),
    })
}

fn group_assign(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr group assign <group> <workspace>...");
        return Ok(2);
    }

    super::runtime::group_assign(GroupAssignParams {
        group_id: args[0].clone(),
        workspace_ids: args[1..]
            .iter()
            .map(|id| super::normalize_workspace_id(id))
            .collect(),
    })
}

fn group_unassign(args: &[String]) -> std::io::Result<i32> {
    if args.is_empty() {
        eprintln!("usage: herdr group unassign <workspace>...");
        return Ok(2);
    }

    super::runtime::group_unassign(GroupUnassignParams {
        workspace_ids: args
            .iter()
            .map(|id| super::normalize_workspace_id(id))
            .collect(),
    })
}

fn group_remove(args: &[String]) -> std::io::Result<i32> {
    let [group_id] = args else {
        eprintln!("usage: herdr group remove <group>");
        return Ok(2);
    };

    super::runtime::group_remove(group_id.clone())
}

fn group_close(args: &[String]) -> std::io::Result<i32> {
    let [group_id] = args else {
        eprintln!("usage: herdr group close <group>");
        return Ok(2);
    };

    super::runtime::group_close(group_id.clone())
}

fn group_set_collapsed(args: &[String], collapsed: bool) -> std::io::Result<i32> {
    let verb = if collapsed { "collapse" } else { "expand" };
    let [group_id] = args else {
        eprintln!("usage: herdr group {verb} <group>");
        return Ok(2);
    };

    super::runtime::group_set_collapsed(GroupSetCollapsedParams {
        group_id: group_id.clone(),
        collapsed,
    })
}

fn print_group_help() {
    eprintln!("herdr group commands:");
    eprintln!("  herdr group list");
    eprintln!("  herdr group get <group>");
    eprintln!("  herdr group create --name NAME <workspace>...");
    eprintln!("  herdr group rename <group> <name>");
    eprintln!("  herdr group assign <group> <workspace>...");
    eprintln!("  herdr group unassign <workspace>...");
    eprintln!("  herdr group remove <group>");
    eprintln!("  herdr group close <group>");
    eprintln!("  herdr group collapse <group>");
    eprintln!("  herdr group expand <group>");
}
