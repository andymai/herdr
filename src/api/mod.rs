pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub(crate) use server::start_server_with_stop_control;
pub use server::{start_server_with_capabilities, ServerHandle};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::ServerReloadAgentManifests(_)
            | Method::NotificationShow(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceMove(_)
            | Method::WorkspaceMoveBlock(_)
            | Method::WorkspaceReportMetadata(_)
            | Method::WorkspaceClose(_)
            | Method::GroupCreate(_)
            | Method::GroupRename(_)
            | Method::GroupAssign(_)
            | Method::GroupUnassign(_)
            | Method::GroupRemove(_)
            | Method::GroupClose(_)
            | Method::GroupSetCollapsed(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeRemove(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabMove(_)
            | Method::TabClose(_)
            | Method::LayoutApply(_)
            | Method::LayoutSetSplitRatio(_)
            | Method::AgentRename(_)
            | Method::AgentViewSet(_)
            | Method::AgentViewClear(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentPrompt(_)
            | Method::AgentSendKeys(_)
            | Method::PaneSplit(_)
            | Method::PaneSwap(_)
            | Method::PaneMove(_)
            | Method::PaneZoom(_)
            | Method::PaneFocusDirection(_)
            | Method::PaneResize(_)
            | Method::PaneFocus(_)
            | Method::PaneInputSet(_)
            | Method::PaneRename(_)
            | Method::PaneGraphicsSet(_)
            | Method::PaneGraphicsClear(_)
            | Method::PaneGraphicsStream(_)
            | Method::PaneGraphicsStreamSet(_)
            | Method::PaneGraphicsStreamDirect(_)
            | Method::PaneGraphicsStreamOpen(_)
            | Method::PaneGraphicsStreamClose(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
            | Method::PopupClose(_)
            | Method::PluginUnlink(_)
            | Method::PluginDisable(_)
            | Method::PluginActionInvoke(_)
            | Method::PluginPaneOpen(_)
            | Method::PluginPaneFocus(_)
            | Method::PluginPaneClose(_)
    )
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    pub stream_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod request_changes_ui_tests {
    use super::*;
    use crate::api::schema::{
        EmptyParams, GroupCreateParams, GroupSetCollapsedParams, GroupTarget,
    };

    fn request(method: Method) -> Request {
        Request {
            id: "test".into(),
            method,
        }
    }

    #[test]
    fn group_mutations_mark_the_ui_dirty() {
        let target = || GroupTarget {
            group_id: "g1".into(),
        };
        for method in [
            Method::GroupCreate(GroupCreateParams {
                name: "Client".into(),
                workspace_ids: vec!["w1".into()],
            }),
            Method::GroupRemove(target()),
            Method::GroupClose(target()),
            Method::GroupSetCollapsed(GroupSetCollapsedParams {
                group_id: "g1".into(),
                collapsed: true,
            }),
        ] {
            assert!(
                request_changes_ui(&request(method.clone())),
                "{method:?} must trigger a client re-render"
            );
        }
    }

    #[test]
    fn group_reads_do_not_mark_the_ui_dirty() {
        assert!(!request_changes_ui(&request(Method::GroupList(
            EmptyParams::default()
        ))));
        assert!(!request_changes_ui(&request(Method::GroupGet(
            GroupTarget {
                group_id: "g1".into(),
            }
        ))));
    }
}
