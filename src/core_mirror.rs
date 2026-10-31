//! Mirrors the Herdr app's spaces, tabs and splits from herdr-core.
//!
//! The app keeps its layout in the core's `herdr-app` document. One thread
//! polls that document and another follows terminal status reports; both hand
//! what they read to the main loop, which builds and updates workspaces whose
//! panes attach to the same core terminals.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::events::AppEvent;

const LAYOUT_DOC: &str = "herdr-app";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RESYNC_INTERVAL: Duration = Duration::from_secs(15);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoreLayout {
    pub spaces: Vec<CoreSpace>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoreSpace {
    pub id: String,
    pub name: Option<String>,
    pub folder: PathBuf,
    pub tabs: Vec<CoreTab>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoreTab {
    pub id: String,
    pub name: Option<String>,
    pub root: CoreNode,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CoreNode {
    Terminal(String),
    Split {
        side_by_side: bool,
        children: Vec<(f32, CoreNode)>,
    },
}

impl CoreLayout {
    /// Another machine's layout, with its ids and terminals addressed the way
    /// this core sees them.
    fn on_machine(self, machine: &str) -> Self {
        let qualify = |id: String| format!("{machine}/{id}");
        let spaces = self
            .spaces
            .into_iter()
            .map(|space| CoreSpace {
                id: qualify(space.id),
                tabs: space
                    .tabs
                    .into_iter()
                    .map(|tab| CoreTab {
                        id: qualify(tab.id),
                        root: tab.root.on_machine(machine),
                        ..tab
                    })
                    .collect(),
                ..space
            })
            .collect();
        CoreLayout { spaces }
    }

    fn keep_terminals(self, live: &HashSet<String>) -> Self {
        let spaces = self
            .spaces
            .into_iter()
            .filter_map(|space| {
                let tabs: Vec<CoreTab> = space
                    .tabs
                    .into_iter()
                    .filter_map(|tab| {
                        Some(CoreTab {
                            root: tab.root.keep_terminals(live)?,
                            ..tab
                        })
                    })
                    .collect();
                (!tabs.is_empty()).then_some(CoreSpace { tabs, ..space })
            })
            .collect();
        CoreLayout { spaces }
    }
}

impl CoreNode {
    fn on_machine(self, machine: &str) -> Self {
        match self {
            CoreNode::Terminal(terminal) => CoreNode::Terminal(format!("{machine}/{terminal}")),
            CoreNode::Split {
                side_by_side,
                children,
            } => CoreNode::Split {
                side_by_side,
                children: children
                    .into_iter()
                    .map(|(weight, child)| (weight, child.on_machine(machine)))
                    .collect(),
            },
        }
    }

    fn keep_terminals(self, live: &HashSet<String>) -> Option<Self> {
        match self {
            CoreNode::Terminal(terminal) => live
                .contains(&terminal)
                .then_some(CoreNode::Terminal(terminal)),
            CoreNode::Split {
                side_by_side,
                children,
            } => {
                let mut children: Vec<(f32, CoreNode)> = children
                    .into_iter()
                    .filter_map(|(weight, child)| Some((weight, child.keep_terminals(live)?)))
                    .collect();
                match children.len() {
                    0 => None,
                    1 => children.pop().map(|(_, child)| child),
                    _ => Some(CoreNode::Split {
                        side_by_side,
                        children,
                    }),
                }
            }
        }
    }

    fn terminal_list(&self) -> Vec<String> {
        match self {
            CoreNode::Terminal(terminal) => vec![terminal.clone()],
            CoreNode::Split { children, .. } => children
                .iter()
                .flat_map(|(_, child)| child.terminal_list())
                .collect(),
        }
    }

    pub(crate) fn first_terminal(&self) -> &str {
        match self {
            CoreNode::Terminal(terminal) => terminal,
            CoreNode::Split { children, .. } => children[0].1.first_terminal(),
        }
    }

    /// A stable description of the tree, so a tab is rebuilt only when its
    /// shape or terminals change.
    pub(crate) fn signature(&self) -> String {
        match self {
            CoreNode::Terminal(terminal) => terminal.clone(),
            CoreNode::Split {
                side_by_side,
                children,
            } => {
                let parts: Vec<String> = children
                    .iter()
                    .map(|(weight, child)| format!("{weight}:{}", child.signature()))
                    .collect();
                let kind = if *side_by_side { "row" } else { "column" };
                format!("{kind}({})", parts.join(","))
            }
        }
    }
}

/// Which workspaces and tabs mirror which app spaces and tabs, and the last
/// status each core terminal reported.
#[derive(Debug, Default)]
pub(crate) struct CoreMirrorState {
    pub enabled: bool,
    pub spaces: HashMap<String, String>,
    pub tabs: HashMap<String, MirroredTab>,
    pub statuses: HashMap<String, CoreStatus>,
    /// Spaces and tabs started here that the app has not adopted yet.
    pub pending_spaces: HashSet<String>,
    pub pending_tabs: HashSet<String>,
    /// Spaces and tabs closed here whose programs are still ending; they are
    /// not rebuilt while the layout still lists them.
    pub closed_spaces: HashSet<String>,
    pub closed_tabs: HashSet<String>,
    /// The name the mirror last gave each workspace and tab. A different
    /// current name means someone renamed it here, and that name is kept.
    pub applied_workspace_names: HashMap<String, Option<String>>,
    pub applied_tab_names: HashMap<(String, usize), Option<String>>,
}

impl CoreMirrorState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }
}

/// The signature a tab will have once a terminal labeled for it is adopted
/// (see `adopt_labeled_terminals`), so a split made here is not rebuilt
/// before the app places it.
pub(crate) fn signature_with_adopted(signature: &str, terminal: &str) -> String {
    match signature
        .strip_prefix("row(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(children) => format!("row({children},1:{terminal})"),
        None => format!("row(1:{signature},1:{terminal})"),
    }
}

/// One request to herdr-core on a fresh connection.
pub(crate) fn call(method: &str, params: Value) -> io::Result<Value> {
    Connection::open()?.call(method, params)
}

/// Set one label on each terminal in the background; an empty value removes it.
pub(crate) fn label_terminals(terminals: Vec<String>, key: &'static str, value: String) {
    if terminals.is_empty() {
        return;
    }
    let spawned = crate::thread_spawn::spawn_named("herdr-core-mirror-label", move || {
        for terminal in terminals {
            let params = json!({"terminal": terminal, "labels": {key: value}});
            if let Err(err) = call("terminal.label", params) {
                warn!(terminal = %terminal, err = %err, "failed to label herdr-core terminal");
            }
        }
    });
    if let Err(err) = spawned {
        warn!(err = %err, "failed to start labeling herdr-core terminals");
    }
}

/// End programs in the background: a kill waits for each one to exit.
pub(crate) fn end_terminals(terminals: Vec<String>) {
    if terminals.is_empty() {
        return;
    }
    let spawned = crate::thread_spawn::spawn_named("herdr-core-mirror-end", move || {
        for terminal in terminals {
            if let Err(err) = call("terminal.kill", json!({"terminal": terminal})) {
                warn!(terminal = %terminal, err = %err, "failed to end herdr-core terminal");
            }
        }
    });
    if let Err(err) = spawned {
        warn!(err = %err, "failed to start ending herdr-core terminals");
    }
}

/// A fresh id in the app's style (`sp…`, `tb…`): a prefix and 32 hex digits.
pub(crate) fn new_app_id(prefix: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let seed = format!(
        "{nanos}:{}:{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let digest = Sha256::digest(seed.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}{hex}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirroredTab {
    pub workspace_id: String,
    pub tab_number: usize,
    pub signature: String,
}

/// A terminal's latest OSC 7501 report as the core relays it. `state` is
/// `None` once the program stops reporting or ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreStatus {
    pub terminal: String,
    pub app: Option<String>,
    pub state: Option<String>,
    pub message: Option<String>,
}

pub(crate) fn spawn(events: mpsc::Sender<AppEvent>) -> io::Result<()> {
    let layout_events = events.clone();
    crate::thread_spawn::spawn_named("herdr-core-mirror-layout", move || {
        layout_loop(&layout_events)
    })?;
    crate::thread_spawn::spawn_named("herdr-core-mirror-status", move || status_loop(&events))?;
    Ok(())
}

fn layout_loop(events: &mpsc::Sender<AppEvent>) {
    loop {
        if let Err(err) = follow_layout(events) {
            debug!(err = %err, "herdr-core layout mirror disconnected");
        }
        if events.is_closed() {
            return;
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

// The document can name terminals that already ended (the app tidies it only
// while it runs), so only live terminals are mirrored.
fn follow_layout(events: &mpsc::Sender<AppEvent>) -> io::Result<()> {
    let mut connection = Connection::open()?;
    let mut last_layout = None;
    let mut last_sent = Instant::now();
    loop {
        let doc = connection.call("doc.get", json!({"name": LAYOUT_DOC}))?;
        let mut layout = parse_layout(doc.get("value").unwrap_or(&Value::Null));
        for machine in connected_machines(&connection.call("machine.list", json!({}))?) {
            match connection.call("doc.get", json!({"name": LAYOUT_DOC, "on": machine})) {
                Ok(doc) => layout.spaces.extend(
                    parse_layout(doc.get("value").unwrap_or(&Value::Null))
                        .on_machine(&machine)
                        .spaces,
                ),
                Err(err) => debug!(machine = %machine, err = %err, "no layout from machine"),
            }
        }
        let terminals = connection.call("terminal.list", json!({}))?;
        let live = live_terminals(&terminals);
        let mut layout = layout.keep_terminals(&live);
        adopt_labeled_terminals(&mut layout, &terminals);
        name_from_labels(&mut layout, &terminals);
        layout
            .spaces
            .sort_by(|a, b| split_machine(&a.id).0.cmp(&split_machine(&b.id).0));
        if last_layout.as_ref() != Some(&layout) || last_sent.elapsed() >= RESYNC_INTERVAL {
            if events
                .blocking_send(AppEvent::CoreLayout(layout.clone()))
                .is_err()
            {
                return Ok(());
            }
            last_layout = Some(layout);
            last_sent = Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

const SPACE_LABEL: &str = "herdr.space";
const TAB_LABEL: &str = "herdr.tab";
pub(crate) const SPACE_NAME_LABEL: &str = "herdr.space.name";
pub(crate) const TAB_NAME_LABEL: &str = "herdr.tab.name";

/// Names given here travel as labels on the terminals, because the app has
/// no way to take a name from outside its window. They fill in where the app
/// has no name of its own.
fn name_from_labels(layout: &mut CoreLayout, list: &Value) {
    let labels: HashMap<&str, &Value> = list
        .get("terminals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|record| Some((record.get("address")?.as_str()?, record.get("labels")?)))
        .collect();
    let name_in = |terminals: Vec<String>, key: &str| {
        terminals.iter().find_map(|terminal| {
            labels
                .get(terminal.as_str())?
                .get(key)?
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
        })
    };
    for space in &mut layout.spaces {
        for tab in &mut space.tabs {
            if tab.name.is_none() {
                tab.name = name_in(tab.root.terminal_list(), TAB_NAME_LABEL);
            }
        }
        if space.name.is_none() {
            let terminals = space
                .tabs
                .iter()
                .flat_map(|tab| tab.root.terminal_list())
                .collect();
            space.name = name_in(terminals, SPACE_NAME_LABEL);
        }
    }
}

/// Live terminals labeled for a space the document doesn't show them in yet
/// (started here, or by an agent, while the app wasn't running to adopt
/// them) join their space and tab the way the app would add them: a new tab,
/// or side by side in an existing one. Once the app adopts them the document
/// says where they are. Sub-agents (terminals with a parent) get no pane, as
/// in the app.
fn adopt_labeled_terminals(layout: &mut CoreLayout, list: &Value) {
    let mut shown: HashSet<String> = layout
        .spaces
        .iter()
        .flat_map(|space| space.tabs.iter())
        .flat_map(|tab| tab.root.terminal_list())
        .collect();
    let mut labeled: Vec<&Value> = list
        .get("terminals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|record| record.get("state").and_then(Value::as_str) == Some("live"))
        .filter(|record| record.pointer("/labels/herdr.space").is_some())
        .filter(|record| record.get("parent").is_none_or(Value::is_null))
        .collect();
    labeled.sort_by_key(|record| record.get("created_ms").and_then(Value::as_u64));
    for record in labeled {
        let Some(address) = record.get("address").and_then(Value::as_str) else {
            continue;
        };
        if !shown.insert(address.to_owned()) {
            continue;
        }
        let label = |key: &str| {
            record
                .get("labels")
                .and_then(|labels| labels.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let Some(space_id) = label(SPACE_LABEL) else {
            continue;
        };
        let tab_id = label(TAB_LABEL).unwrap_or_else(|| format!("{address}-tab"));
        let (space_id, tab_id) = match split_machine(address).0 {
            Some(machine) => (
                format!("{machine}/{space_id}"),
                format!("{machine}/{tab_id}"),
            ),
            None => (space_id, tab_id),
        };
        let terminal = CoreNode::Terminal(address.to_owned());
        let space = match layout.spaces.iter().position(|space| space.id == space_id) {
            Some(idx) => &mut layout.spaces[idx],
            None => {
                layout.spaces.push(CoreSpace {
                    id: space_id,
                    name: None,
                    folder: record
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .unwrap_or_default(),
                    tabs: Vec::new(),
                });
                layout.spaces.last_mut().expect("just pushed")
            }
        };
        match space.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            Some(tab) => {
                let root = std::mem::replace(&mut tab.root, CoreNode::Terminal(String::new()));
                tab.root = match root {
                    CoreNode::Split {
                        side_by_side: true,
                        mut children,
                    } => {
                        children.push((1.0, terminal));
                        CoreNode::Split {
                            side_by_side: true,
                            children,
                        }
                    }
                    other => CoreNode::Split {
                        side_by_side: true,
                        children: vec![(1.0, other), (1.0, terminal)],
                    },
                };
            }
            None => space.tabs.push(CoreTab {
                id: tab_id,
                name: None,
                root: terminal,
            }),
        }
    }
}

/// Machines this core reaches now, by name, the order the app lists them in.
fn connected_machines(list: &Value) -> Vec<String> {
    let mut machines: Vec<String> = list
        .get("machines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|machine| machine.get("state").and_then(Value::as_str) == Some("connected"))
        .filter(|machine| {
            !machine
                .get("temporary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|machine| machine.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    machines.sort();
    machines
}

/// Splits `machine/id` (another machine's id as seen from here) from a local
/// id. App ids never contain a slash.
pub(crate) fn split_machine(id: &str) -> (Option<&str>, &str) {
    match id.split_once('/') {
        Some((machine, local)) => (Some(machine), local),
        None => (None, id),
    }
}

fn live_terminals(list: &Value) -> HashSet<String> {
    list.get("terminals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|record| record.get("state").and_then(Value::as_str) == Some("live"))
        .filter_map(|record| record.get("address").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn status_loop(events: &mpsc::Sender<AppEvent>) {
    loop {
        if let Err(err) = follow_status(events) {
            debug!(err = %err, "herdr-core status mirror disconnected");
        }
        if events.is_closed() {
            return;
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

fn follow_status(events: &mpsc::Sender<AppEvent>) -> io::Result<()> {
    let mut connection = Connection::open()?;
    let snapshot = connection.call("events.subscribe", json!({}))?;
    for record in snapshot
        .get("terminals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(status) = status_from_record(record) {
            if events.blocking_send(AppEvent::CoreStatus(status)).is_err() {
                return Ok(());
            }
        }
    }
    loop {
        let message = connection.read()?;
        for status in statuses_from_notification(&message) {
            if events.blocking_send(AppEvent::CoreStatus(status)).is_err() {
                return Ok(());
            }
        }
    }
}

struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Connection {
    fn open() -> io::Result<Self> {
        let writer = UnixStream::connect(crate::core_attach::socket_path()?)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self {
            writer,
            reader,
            next_id: 1,
        })
    }

    fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        loop {
            let message = self.read()?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return crate::core_attach::rpc_result(message);
            }
        }
    }

    fn read(&mut self) -> io::Result<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "herdr-core closed the connection",
            ));
        }
        serde_json::from_str(&line).map_err(io::Error::other)
    }
}

pub(crate) fn parse_layout(value: &Value) -> CoreLayout {
    let spaces = value
        .get("spaces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_space)
        .collect();
    CoreLayout { spaces }
}

fn parse_space(value: &Value) -> Option<CoreSpace> {
    let id = value.get("id")?.as_str()?.to_owned();
    let tabs: Vec<CoreTab> = value
        .get("tabs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_tab)
        .collect();
    if tabs.is_empty() {
        return None;
    }
    Some(CoreSpace {
        id,
        name: non_empty(value.get("name")),
        folder: value
            .get("folder")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default(),
        tabs,
    })
}

fn parse_tab(value: &Value) -> Option<CoreTab> {
    Some(CoreTab {
        id: value.get("id")?.as_str()?.to_owned(),
        name: non_empty(value.get("name")),
        root: parse_node(value.get("root")?)?,
    })
}

// Mod panes have no terminal and are left out; a split left with one child
// collapses into it.
fn parse_node(value: &Value) -> Option<CoreNode> {
    match value.get("type").and_then(Value::as_str)? {
        "block" => value
            .pointer("/content/terminal")
            .and_then(Value::as_str)
            .map(|terminal| CoreNode::Terminal(terminal.to_owned())),
        "stack" => {
            let children = value.get("children").and_then(Value::as_array)?;
            let active = value.get("active").and_then(Value::as_str);
            children
                .iter()
                .find(|child| active.is_some() && child.get("id").and_then(Value::as_str) == active)
                .or_else(|| children.first())
                .and_then(parse_node)
        }
        kind @ ("row" | "column") => {
            let mut children: Vec<(f32, CoreNode)> = value
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|child| Some((weight(child), parse_node(child)?)))
                .collect();
            match children.len() {
                0 => None,
                1 => children.pop().map(|(_, child)| child),
                _ => Some(CoreNode::Split {
                    side_by_side: kind == "row",
                    children,
                }),
            }
        }
        _ => None,
    }
}

fn weight(value: &Value) -> f32 {
    value
        .pointer("/size/weight")
        .and_then(Value::as_f64)
        .filter(|weight| *weight > 0.0)
        .map_or(1.0, |weight| weight as f32)
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn status_from_record(record: &Value) -> Option<CoreStatus> {
    let terminal = record.get("address")?.as_str()?.to_owned();
    Some(status_for(terminal, record.get("status")))
}

fn status_for(terminal: String, status: Option<&Value>) -> CoreStatus {
    let status = status.filter(|status| !status.is_null());
    let field = |name: &str| {
        status
            .and_then(|status| status.get(name))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    CoreStatus {
        terminal,
        app: field("app"),
        state: field("state"),
        message: field("msg"),
    }
}

fn statuses_from_notification(message: &Value) -> Vec<CoreStatus> {
    let Some(params) = message.get("params") else {
        return Vec::new();
    };
    match message.get("method").and_then(Value::as_str) {
        Some("event") => {
            let Some(terminal) = params.get("terminal").and_then(Value::as_str) else {
                return Vec::new();
            };
            let data = params.get("data");
            match params.get("type").and_then(Value::as_str) {
                Some("status") => vec![status_for(
                    terminal.to_owned(),
                    data.and_then(|data| data.get("status")),
                )],
                Some("created" | "restarted") => {
                    data.and_then(status_from_record).into_iter().collect()
                }
                Some("lost" | "finished" | "removed") => {
                    vec![status_for(terminal.to_owned(), None)]
                }
                _ => Vec::new(),
            }
        }
        Some("missed") => params
            .get("terminals")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(status_from_record)
            .collect(),
        Some(other) if other != "heartbeat" => {
            warn!(method = other, "unexpected herdr-core notification");
            Vec::new()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_keeps_terminal_blocks_and_drops_mod_panes() {
        let layout = parse_layout(&json!({"spaces": [
            {"id": "sp1", "name": "", "folder": "/repo", "tabs": [
                {"id": "tb1", "name": "", "root": {"id": "r", "type": "row", "children": [
                    {"id": "p1", "type": "block", "content": {"terminal": "t1"}},
                    {"id": "p2", "type": "block", "content": {"mod": "markdown"}},
                ]}},
                {"id": "tb2", "name": "review", "root": {"id": "r2", "type": "row", "children": [
                    {"id": "p3", "type": "block", "content": {"mod": "markdown"}},
                ]}},
            ]},
            {"id": "sp2", "name": "named", "folder": "/x", "tabs": []},
        ]}));
        assert_eq!(
            layout,
            CoreLayout {
                spaces: vec![CoreSpace {
                    id: "sp1".into(),
                    name: None,
                    folder: "/repo".into(),
                    tabs: vec![CoreTab {
                        id: "tb1".into(),
                        name: None,
                        root: CoreNode::Terminal("t1".into()),
                    }],
                }],
            }
        );
    }

    #[test]
    fn nested_splits_keep_direction_order_and_weights() {
        let node = parse_node(&json!({"id": "r", "type": "row", "children": [
            {"id": "a", "type": "block", "size": {"weight": 2}, "content": {"terminal": "t1"}},
            {"id": "b", "type": "column", "children": [
                {"id": "c", "type": "block", "content": {"terminal": "t2"}},
                {"id": "d", "type": "block", "content": {"terminal": "t3"}},
            ]},
        ]}))
        .unwrap();
        assert_eq!(node.first_terminal(), "t1");
        assert_eq!(node.signature(), "row(2:t1,1:column(1:t2,1:t3))");
    }

    #[test]
    fn ended_terminals_drop_out_and_empty_spaces_go() {
        let layout = parse_layout(&json!({"spaces": [
            {"id": "sp1", "name": "", "folder": "/a", "tabs": [
                {"id": "tb1", "name": "", "root": {"id": "r", "type": "row", "children": [
                    {"id": "p1", "type": "block", "content": {"terminal": "t1"}},
                    {"id": "p2", "type": "block", "content": {"terminal": "t2"}},
                ]}},
            ]},
            {"id": "sp2", "name": "", "folder": "/b", "tabs": [
                {"id": "tb2", "name": "", "root": {"id": "p3", "type": "block", "content": {"terminal": "t3"}}},
            ]},
        ]}))
        .keep_terminals(&HashSet::from(["t1".to_owned()]));
        assert_eq!(layout.spaces.len(), 1);
        assert_eq!(
            layout.spaces[0].tabs[0].root,
            CoreNode::Terminal("t1".into())
        );
        assert_eq!(
            live_terminals(&json!({"terminals": [
                {"address": "t1", "state": "live"},
                {"address": "t2", "state": "finished"},
            ]})),
            HashSet::from(["t1".to_owned()])
        );
    }

    #[test]
    fn labeled_terminals_join_their_space_and_tab() {
        let mut layout = parse_layout(&json!({"spaces": [
            {"id": "sp1", "name": "", "folder": "/a", "tabs": [
                {"id": "tb1", "name": "", "root": {"id": "p1", "type": "block", "content": {"terminal": "t1"}}},
            ]},
        ]}));
        adopt_labeled_terminals(
            &mut layout,
            &json!({"terminals": [
                {"address": "t1", "state": "live", "created_ms": 1,
                 "labels": {"herdr.space": "sp1", "herdr.tab": "tb1"}},
                {"address": "t2", "state": "live", "created_ms": 2,
                 "labels": {"herdr.space": "sp1", "herdr.tab": "tb1"}},
                {"address": "t3", "state": "live", "created_ms": 3,
                 "labels": {"herdr.space": "sp1", "herdr.tab": "tb2"}},
                {"address": "t4", "state": "live", "created_ms": 4, "cwd": "/new",
                 "labels": {"herdr.space": "sp2"}},
                {"address": "t5", "state": "finished", "created_ms": 5,
                 "labels": {"herdr.space": "sp3"}},
                {"address": "t6", "state": "live", "created_ms": 6, "labels": {}},
                {"address": "t7", "state": "live", "created_ms": 7, "parent": "t1",
                 "labels": {"herdr.space": "sp1", "herdr.tab": "tb1"}},
            ]}),
        );
        let sp1 = &layout.spaces[0];
        assert_eq!(sp1.tabs[0].root.signature(), "row(1:t1,1:t2)");
        assert_eq!(sp1.tabs[1].root, CoreNode::Terminal("t3".into()));
        assert_eq!(layout.spaces.len(), 2);
        assert_eq!(layout.spaces[1].id, "sp2");
        assert_eq!(layout.spaces[1].folder, PathBuf::from("/new"));
        assert_eq!(layout.spaces[1].tabs[0].id, "t4-tab");
    }

    #[test]
    fn predicted_signature_matches_adoption() {
        for root in [
            json!({"id": "p1", "type": "block", "content": {"terminal": "t1"}}),
            json!({"id": "r", "type": "row", "children": [
                {"id": "p1", "type": "block", "size": {"weight": 2}, "content": {"terminal": "t1"}},
                {"id": "c", "type": "column", "children": [
                    {"id": "p2", "type": "block", "content": {"terminal": "t2"}},
                    {"id": "p3", "type": "block", "content": {"terminal": "t3"}},
                ]},
            ]}),
            json!({"id": "c", "type": "column", "children": [
                {"id": "p1", "type": "block", "content": {"terminal": "t1"}},
                {"id": "p2", "type": "block", "content": {"terminal": "t2"}},
            ]}),
        ] {
            let mut layout = parse_layout(&json!({"spaces": [
                {"id": "sp", "name": "", "folder": "/", "tabs": [{"id": "tb", "name": "", "root": root}]},
            ]}));
            let before = layout.spaces[0].tabs[0].root.signature();
            adopt_labeled_terminals(
                &mut layout,
                &json!({"terminals": [{"address": "t9", "state": "live",
                    "labels": {"herdr.space": "sp", "herdr.tab": "tb"}}]}),
            );
            assert_eq!(
                signature_with_adopted(&before, "t9"),
                layout.spaces[0].tabs[0].root.signature()
            );
        }
    }

    #[test]
    fn label_names_fill_in_only_where_the_app_has_none() {
        let mut layout = parse_layout(&json!({"spaces": [
            {"id": "sp1", "name": "", "folder": "/a", "tabs": [
                {"id": "tb1", "name": "", "root": {"id": "p1", "type": "block", "content": {"terminal": "t1"}}},
                {"id": "tb2", "name": "kept", "root": {"id": "p2", "type": "block", "content": {"terminal": "t2"}}},
            ]},
        ]}));
        name_from_labels(
            &mut layout,
            &json!({"terminals": [
                {"address": "t1", "labels": {"herdr.space.name": "phone work", "herdr.tab.name": "logs"}},
                {"address": "t2", "labels": {"herdr.tab.name": "ignored"}},
            ]}),
        );
        assert_eq!(layout.spaces[0].name.as_deref(), Some("phone work"));
        assert_eq!(layout.spaces[0].tabs[0].name.as_deref(), Some("logs"));
        assert_eq!(layout.spaces[0].tabs[1].name.as_deref(), Some("kept"));
    }

    #[test]
    fn other_machines_are_qualified_and_listed_by_name() {
        let layout = parse_layout(&json!({"spaces": [
            {"id": "spR", "name": "", "folder": "/r", "tabs": [
                {"id": "tbR", "name": "", "root": {"id": "p", "type": "block", "content": {"terminal": "t1"}}},
            ]},
        ]}))
        .on_machine("box");
        assert_eq!(layout.spaces[0].id, "box/spR");
        assert_eq!(layout.spaces[0].tabs[0].id, "box/tbR");
        assert_eq!(
            layout.spaces[0].tabs[0].root,
            CoreNode::Terminal("box/t1".into())
        );
        assert_eq!(
            connected_machines(&json!({"machines": [
                {"name": "zed", "state": "connected"},
                {"name": "box", "state": "connected"},
                {"name": "down", "state": "connecting"},
                {"name": "sandbox", "state": "connected", "temporary": true},
            ]})),
            vec!["box".to_owned(), "zed".to_owned()]
        );
        assert_eq!(split_machine("box/spR"), (Some("box"), "spR"));
        assert_eq!(split_machine("spL"), (None, "spL"));

        let mut adopted = CoreLayout { spaces: Vec::new() };
        adopt_labeled_terminals(
            &mut adopted,
            &json!({"terminals": [{"address": "box/t2", "state": "live",
                "labels": {"herdr.space": "spR", "herdr.tab": "tbN"}}]}),
        );
        assert_eq!(adopted.spaces[0].id, "box/spR");
        assert_eq!(adopted.spaces[0].tabs[0].id, "box/tbN");
    }

    #[test]
    fn app_ids_have_the_app_shape_and_differ() {
        let first = new_app_id("tb");
        let second = new_app_id("tb");
        assert_eq!(first.len(), 34);
        assert!(first.starts_with("tb"));
        assert!(first[2..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn stack_shows_its_active_child() {
        let node = parse_node(
            &json!({"id": "s", "type": "stack", "active": "b", "children": [
                {"id": "a", "type": "block", "content": {"terminal": "t1"}},
                {"id": "b", "type": "block", "content": {"terminal": "t2"}},
            ]}),
        )
        .unwrap();
        assert_eq!(node, CoreNode::Terminal("t2".into()));
    }

    #[test]
    fn status_events_map_to_reports() {
        let status = statuses_from_notification(&json!({"jsonrpc": "2.0", "method": "event",
            "params": {"seq": 4, "type": "status", "terminal": "t7",
                "data": {"status": {"app": "claude-code", "state": "working", "msg": "Editing"},
                    "records": {}, "reported": true}}}));
        assert_eq!(
            status,
            vec![CoreStatus {
                terminal: "t7".into(),
                app: Some("claude-code".into()),
                state: Some("working".into()),
                message: Some("Editing".into()),
            }]
        );
        let ended = statuses_from_notification(&json!({"jsonrpc": "2.0", "method": "event",
            "params": {"seq": 5, "type": "finished", "terminal": "t7", "data": {}}}));
        assert_eq!(ended[0].state, None);
        assert!(statuses_from_notification(
            &json!({"jsonrpc": "2.0", "method": "heartbeat", "params": {"seq": 5}})
        )
        .is_empty());
    }
}
