use super::*;

use crate::core_attach::{CoreAttachConfig, CoreAttachHandle};

impl PaneRuntime {
    pub(crate) fn core_terminal(&self) -> Option<&str> {
        match &self.io {
            PaneRuntimeIo::Core(core) => Some(core.terminal()),
            _ => None,
        }
    }

    pub(crate) fn set_core_visible(&self, visible: bool) {
        if let PaneRuntimeIo::Core(core) = &self.io {
            core.set_visible(visible);
        }
    }

    /// A pane that shows a herdr-core terminal. The program belongs to the
    /// core, so closing the pane detaches instead of signalling anything, and
    /// agent state comes from the core rather than local process detection.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_core(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        core_terminal: &str,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<(Self, Option<std::path::PathBuf>)> {
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let (response_tx, _response_rx) = mpsc::channel::<Bytes>(1);
        let mut terminal = crate::ghostty::Terminal::new(cols, rows, scrollback_limit_bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if crate::kitty_graphics::is_enabled() {
            terminal
                .enable_kitty_graphics()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let pane_terminal = GhosttyPaneTerminal::new(terminal, response_tx.clone())?;
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
        let terminal = Arc::new(PaneTerminal::new(pane_terminal));
        let compression = TerminalCompressionTask::spawn(pane_id, terminal.clone());
        let content_seq = Arc::new(AtomicU64::new(0));
        let content_write_lock = Arc::new(Mutex::new(()));
        let reported_cwd = Arc::new(Mutex::new(None));
        let attach_ended = Arc::new(AtomicBool::new(false));

        let on_read = {
            let terminal = terminal.clone();
            let response_writer = response_tx.clone();
            let render_notify = render_notify.clone();
            let render_dirty = render_dirty.clone();
            let content_seq = content_seq.clone();
            let content_write_lock = content_write_lock.clone();
            let events = events.clone();
            let reported_cwd = reported_cwd.clone();
            let compression_wake = compression.notifier();
            let rt = tokio::runtime::Handle::current();
            Box::new(move |bytes: &[u8]| {
                let content_write_guard = match content_write_lock.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                content_seq.fetch_add(1, Ordering::AcqRel);
                let result = terminal.process_pty_bytes(pane_id, 0, bytes, &response_writer);
                content_seq.fetch_add(1, Ordering::Release);
                drop(content_write_guard);
                compression_wake.wake();
                publish_terminal_bells(pane_id, result.terminal_bells, &events);
                let title_requested =
                    result.terminal_title_changed && render_dirty.request_terminal_title(pane_id);
                let render_requested = result.request_render && render_dirty.request_pty(pane_id);
                if title_requested || render_requested {
                    render_notify.notify_one();
                }
                if let Some(delay) = result.render_delay {
                    let render_notify = render_notify.clone();
                    let render_dirty = render_dirty.clone();
                    rt.spawn(async move {
                        tokio::time::sleep(delay).await;
                        if render_dirty.request_pty(pane_id) {
                            render_notify.notify_one();
                        }
                    });
                }
                if let Some(cwd) = result.reported_cwd.clone() {
                    publish_reported_cwd(pane_id, cwd, &reported_cwd, &events);
                }
                for content in result.clipboard_writes {
                    if let Err(err) = events.try_send(AppEvent::ClipboardWrite { content }) {
                        warn!(
                            pane = pane_id.raw(),
                            err = %err,
                            "failed to send OSC 52 clipboard write"
                        );
                    }
                }
                PtyReadResult {
                    terminal_responses: result.terminal_responses,
                }
            })
        };

        let on_end = {
            let events = events.clone();
            let attach_ended = attach_ended.clone();
            Box::new(move |_end| {
                attach_ended.store(true, Ordering::Release);
                if let Err(e) = events.blocking_send(AppEvent::PaneDied {
                    pane_id,
                    exit_reason: crate::platform::ChildExitReason::Exited,
                }) {
                    error!(pane = pane_id.raw(), err = %e, "failed to send PaneDied event");
                }
            })
        };

        let start = CoreAttachHandle::spawn(CoreAttachConfig {
            pane_id: pane_id.raw(),
            terminal: core_terminal.to_owned(),
            rows,
            cols,
            on_read,
            on_end,
        })
        .inspect_err(
            |err| error!(pane = pane_id.raw(), err = %err, "failed to attach core terminal"),
        )?;

        Ok((
            Self {
                pane_id,
                terminal,
                io: PaneRuntimeIo::Core(start.handle),
                current_size: Cell::new((rows, cols, 0, 0)),
                child_pid: Arc::new(AtomicU32::new(0)),
                reported_cwd,
                persistence_cwd: Mutex::new(None),
                cwd_process_exited: attach_ended.clone(),
                child_wait_completed: Some(attach_ended),
                kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
                content_seq,
                content_write_lock,
                detection_content_seq: Arc::new(AtomicU64::new(0)),
                full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
                self_reported_agent_active: Arc::new(AtomicBool::new(false)),
                detect_reset_notify: Arc::new(Notify::new()),
                pending_release: Arc::new(Mutex::new(None)),
                preserve_processes_on_drop: false,
                compression,
                detect_handle: None,
            },
            start.cwd,
        ))
    }
}
