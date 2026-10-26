use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use crossbeam_utils::atomic::AtomicConsume;
use parking_lot::Mutex;

use crate::core::OperationDirection;
use crate::core::Rect;
use crate::core::Sizing;
use crate::core::WindowContainerBehaviour;

use crate::DefaultLayout;
use crate::FLOATING_APPLICATIONS;
use crate::HIDDEN_HWNDS;
use crate::Layout;
use crate::Notification;
use crate::NotificationEvent;
use crate::REGEX_IDENTIFIERS;
use crate::TRAY_AND_MULTI_WINDOW_IDENTIFIERS;
use crate::Window;
use crate::border_manager;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_WIDTH;
use crate::notify_subscribers;
use crate::splash;
use crate::splash::mdm_enrollment;
use crate::stackbar_manager;
use crate::state::State;
use crate::transparency_manager;
use crate::window::RuleDebug;
use crate::window::should_act;
use crate::window_manager::WindowManager;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::workspace::WorkspaceLayer;

fn should_skip_focus_change(foreground_hwnd: Option<isize>, window_hwnd: isize) -> bool {
    matches!(foreground_hwnd, Some(hwnd) if hwnd != window_hwnd)
}

/// After a managed window is destroyed, Windows may promote no successor, leaving
/// the foreground null with no FocusChange to reconcile from. The selection then
/// never regains focus: every border renders unfocused and keystrokes fall through
/// to nothing until the user refocuses by hand. Refocus only in that case, otherwise
/// foreground successor is left to FocusChange so we don't fight the OS or desync
/// focus.
fn should_refocus_after_destroy(foreground_hwnd: Option<isize>) -> bool {
    matches!(foreground_hwnd, None | Some(0))
}

#[cfg(test)]
mod focus_change_tests {
    use super::should_skip_focus_change;

    #[test]
    fn skips_focus_change_from_background_window() {
        assert!(should_skip_focus_change(Some(1), 2));
    }

    #[test]
    fn allows_focus_change_for_foreground_window() {
        assert!(!should_skip_focus_change(Some(1), 1));
    }

    #[test]
    fn allows_focus_change_when_foreground_unknown() {
        assert!(!should_skip_focus_change(None, 1));
    }
}

#[cfg(test)]
mod destroy_focus_tests {
    use super::should_refocus_after_destroy;

    #[test]
    fn refocuses_when_foreground_is_null() {
        assert!(should_refocus_after_destroy(Some(0)));
    }

    #[test]
    fn refocuses_when_foreground_is_unavailable() {
        assert!(should_refocus_after_destroy(None));
    }

    #[test]
    fn defers_when_a_successor_took_the_foreground() {
        assert!(!should_refocus_after_destroy(Some(12345)));
    }
}

#[tracing::instrument]
pub fn listen_for_events(wm: Arc<Mutex<WindowManager>>) {
    let receiver = wm.lock().incoming_events.clone();

    std::thread::spawn(|| {
        loop {
            if let Ok((mdm, server)) = mdm_enrollment() {
                #[allow(clippy::collapsible_if)]
                if mdm && splash::should().map(|f| f.into()).unwrap_or(true) {
                    let mut args = vec!["splash".to_string()];
                    if let Some(server) = server {
                        if !server.trim().is_empty() {
                            args.push(server);
                        }
                    }

                    let _ = Command::new("komorebic").args(&args).spawn();
                }
            }

            std::thread::sleep(std::time::Duration::from_secs(14400));
        }
    });

    std::thread::spawn(move || {
        tracing::info!("listening");
        loop {
            if let Ok(event) = receiver.recv() {
                let mut guard = wm.lock();
                match guard.process_event(event) {
                    Ok(()) => {}
                    Err(error) => {
                        if cfg!(debug_assertions) {
                            tracing::error!("{:?}", error)
                        } else {
                            tracing::error!("{}", error)
                        }
                    }
                }
            }
        }
    });
}

impl WindowManager {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    #[tracing::instrument(skip(self, event), fields(event = event.title(), winevent = event.winevent(), hwnd = event.hwnd()))]
    pub fn process_event(&mut self, event: WindowManagerEvent) -> eyre::Result<()> {
        if self.is_paused {
            tracing::trace!("ignoring while paused");
            return Ok(());
        }

        let initial_state = State::from(self.as_ref());
        let result = self.process_event_inner(event);
        // Every exit, including errors after partial mutations, refreshes derived caches.
        self.update_known_hwnds();
        if initial_state.has_been_modified(self.as_ref()) || matches!(&result, Ok(true)) {
            let published = self.publish_event_state(event, &initial_state);
            if let Err(error) = result {
                if let Err(notification_error) = published {
                    tracing::error!("failed publishing partial event state: {notification_error}");
                }
                return Err(error);
            }
            published?;
        } else {
            result?;
        }
        Ok(())
    }

    fn process_event_inner(&mut self, event: WindowManagerEvent) -> eyre::Result<bool> {
        // Handle desktop events first — they are global and don't require a managed window
        match event {
            WindowManagerEvent::WindowDesktopChanged { hwnd } => {
                self.sync_desktop_count()?;
                self.reconcile_window_desktop(hwnd)?;
                return Ok(true);
            }
            WindowManagerEvent::DesktopChanged { .. }
            | WindowManagerEvent::DesktopCountChanged
            | WindowManagerEvent::DesktopMoved { .. }
            | WindowManagerEvent::DesktopRenamed { .. } => {
                // Events are hints. Re-read current identities to tolerate delayed or repeated events.
                self.sync_desktop_count()?;
                return Ok(true);
            }
            _ => {}
        }

        let mut rule_debug = RuleDebug::default();

        let should_manage =
            self.window_api
                .should_manage(event.window(), event, &mut rule_debug)?;

        // All event handlers below this point should only be processed if the event is
        // related to a window that should be managed by the WindowManager.
        if !should_manage {
            let mut transparency_override = false;

            if transparency_manager::TRANSPARENCY_ENABLED.load_consume() {
                for m in self.monitors() {
                    for w in m.workspaces() {
                        let event_hwnd = event.window().hwnd;

                        let visible_hwnds = w
                            .visible_windows()
                            .iter()
                            .flatten()
                            .map(|w| w.hwnd)
                            .collect::<Vec<_>>();

                        let contains_managed_window = w.contains_managed_window(event_hwnd);

                        // this is for an old stackbar clicking fix
                        if contains_managed_window && !visible_hwnds.contains(&event_hwnd) {
                            transparency_override = true;
                        }

                        // but we always want to handle a minimize event when transparency overrides
                        // are applied
                        if !transparency_override
                            && contains_managed_window
                            && matches!(event, WindowManagerEvent::Minimize(_, _))
                        {
                            transparency_override = true;
                        }
                    }
                }
            }

            if !transparency_override {
                if rule_debug.matches_ignore_identifier.is_some() {
                    border_manager::send_notification(Option::from(event.hwnd()));
                }

                return Ok(false);
            }
        }

        // Sync desktop count at the start of event processing to handle
        // native desktop changes that may have occurred while we were unfocused.
        self.sync_desktop_count()?;

        if let WindowManagerEvent::MoveResizeStart(_, window)
        | WindowManagerEvent::MoveResizeEnd(_, window) = event
        {
            if self.managed_window_location(window.hwnd).is_some() {
                self.reconcile_window_desktop(window.hwnd)?;
            }
        }

        if let WindowManagerEvent::Show(_, window)
        | WindowManagerEvent::Manage(window)
        | WindowManagerEvent::Uncloak(_, window) = event
        {
            self.manage_desktop_window(window, event)?;
            return Ok(true);
        }

        // Make sure we have the most recently focused monitor from any event
        match event {
            WindowManagerEvent::FocusChange(_, window)
            | WindowManagerEvent::Show(_, window)
            | WindowManagerEvent::MoveResizeEnd(_, window) => {
                if let Some(monitor_idx) = self.monitor_idx_from_window(window) {
                    // This is a hidden window apparently associated with COM support mechanisms (based
                    // on a post from http://www.databaseteam.org/1-ms-sql-server/a5bb344836fb889c.htm)
                    //
                    // The hidden window, OLEChannelWnd, associated with this class (spawned by
                    // explorer.exe), after some debugging, is observed to always be tied to the primary
                    // display monitor, or (usually) monitor 0 in the WindowManager state.
                    //
                    // Due to this, at least one user in the Discord has witnessed behaviour where, when
                    // a MonitorPoll event is triggered by OLEChannelWnd, the focused monitor index gets
                    // set repeatedly to 0, regardless of where the current foreground window is actually
                    // located.
                    //
                    // This check ensures that we only update the focused monitor when the window
                    // triggering monitor reconciliation is known to not be tied to a specific monitor.
                    if let Ok(class) = window.class()
                        && class != "OleMainThreadWndClass"
                        && self.focused_monitor_idx() != monitor_idx
                    {
                        self.focus_monitor(monitor_idx)?;
                    }
                }
            }
            _ => {}
        }

        self.enforce_workspace_rules()?;

        if matches!(event, WindowManagerEvent::MouseCapture(..)) {
            tracing::trace!(
                "only reaping orphans and enforcing workspace rules for mouse capture event"
            );
            return Ok(false);
        }

        match event {
            WindowManagerEvent::Raise(window) => {
                window.focus(false)?;
                self.has_pending_raise_op = false;
            }
            WindowManagerEvent::Destroy(_, window) | WindowManagerEvent::Unmanage(window) => {
                if self.focused_workspace()?.contains_window(window.hwnd) {
                    self.focused_workspace_mut()?.remove_window(window.hwnd)?;

                    // With refocus true, update_focused_workspace focuses the surviving
                    // selection (maximized/monocle/tiling); with refocus false it only
                    // re-tiles.
                    let refocus =
                        should_refocus_after_destroy(WindowsApi::foreground_window().ok());
                    self.update_focused_workspace(refocus, refocus)?;

                    let mut already_moved_window_handles = self.already_moved_window_handles.lock();

                    already_moved_window_handles.remove(&window.hwnd);
                }
            }
            WindowManagerEvent::Minimize(_, window) => {
                // During transient display connection changes (e.g. monitor
                // briefly disconnecting and reconnecting), Windows may fire
                // SystemMinimizeStart for windows on the affected monitor.
                // We must not treat these OS-initiated minimizes as user
                // actions, otherwise the window gets removed from the
                // workspace and the reconciliator cannot restore it.
                if crate::monitor_reconciliator::display_change_in_progress(
                    std::time::Duration::from_secs(10),
                ) {
                    tracing::debug!(
                        "ignoring minimize during display connection change for hwnd: {}",
                        window.hwnd
                    );
                } else {
                    let mut hide = false;

                    {
                        let programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
                        if !programmatically_hidden_hwnds.contains(&window.hwnd) {
                            hide = true;
                        }
                    }

                    if hide {
                        self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                        self.update_focused_workspace(false, false)?;
                    }
                }
            }
            WindowManagerEvent::Hide(_, window) => {
                let mut hide = false;
                // Some major applications unfortunately send the HIDE signal when they are being
                // minimized or destroyed. Applications that close to the tray also do the same,
                // and will have is_window() return true, as the process is still running even if
                // the window is not visible.
                {
                    let tray_and_multi_window_identifiers =
                        TRAY_AND_MULTI_WINDOW_IDENTIFIERS.lock();
                    let regex_identifiers = REGEX_IDENTIFIERS.lock();

                    let title = &window.title()?;
                    let exe_name = &window.exe()?;
                    let class = &window.class()?;
                    let path = &window.path()?;

                    // We don't want to purge windows that have been deliberately hidden by us, eg. when
                    // they are not on the top of a container stack.
                    let programmatically_hidden_hwnds = HIDDEN_HWNDS.lock();
                    let should_act = should_act(
                        title,
                        exe_name,
                        class,
                        path,
                        &tray_and_multi_window_identifiers,
                        &regex_identifiers,
                    )
                    .is_some();

                    if !window.is_window()
                        || (should_act && !programmatically_hidden_hwnds.contains(&window.hwnd))
                    {
                        hide = true;
                    }
                }

                if hide {
                    self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                    self.update_focused_workspace(false, false)?;
                }

                let mut already_moved_window_handles = self.already_moved_window_handles.lock();

                already_moved_window_handles.remove(&window.hwnd);
            }
            WindowManagerEvent::FocusChange(_, window) => {
                if should_skip_focus_change(WindowsApi::foreground_window().ok(), window.hwnd) {
                    tracing::debug!(
                        "ignoring stale focus change for hwnd {} as it is not the foreground window",
                        window.hwnd
                    );
                    return Ok(false);
                }
                // don't want to trigger the full workspace updates when there are no managed
                // containers - this makes floating windows on empty workspaces go into very
                // annoying focus change loops which prevents users from interacting with them
                if !matches!(
                    self.focused_workspace()?.layout,
                    Layout::Default(DefaultLayout::Scrolling)
                ) && !self.focused_workspace()?.containers().is_empty()
                {
                    self.update_focused_workspace(self.mouse_follows_focus, false)?;
                }

                let workspace = self.focused_workspace_mut()?;
                let floating_window_idx = workspace
                    .floating_windows()
                    .iter()
                    .position(|w| w.hwnd == window.hwnd);

                match floating_window_idx {
                    None => {
                        if let Some(w) = &workspace.maximized_window
                            && w.hwnd == window.hwnd
                        {
                            return Ok(false);
                        }

                        if let Some(monocle) = &workspace.monocle_container {
                            if let Some(window) = monocle.focused_window() {
                                window.focus(false)?;
                            }
                        } else {
                            workspace.focus_container_by_window(window.hwnd)?;
                        }

                        workspace.layer = WorkspaceLayer::Tiling;

                        if matches!(
                            self.focused_workspace()?.layout,
                            Layout::Default(DefaultLayout::Scrolling)
                        ) && !self.focused_workspace()?.containers().is_empty()
                        {
                            self.update_focused_workspace(self.mouse_follows_focus, false)?;
                        }
                    }
                    Some(idx) => {
                        if let Some(_window) = workspace.floating_windows().get(idx) {
                            workspace.layer = WorkspaceLayer::Floating;
                        }
                    }
                }
            }
            WindowManagerEvent::Show(..)
            | WindowManagerEvent::Manage(..)
            | WindowManagerEvent::Uncloak(..) => {}
            WindowManagerEvent::MoveResizeStart(_, window) => {
                let monitor_idx = self.focused_monitor_idx();
                let workspace_idx = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor with this idx")?
                    .focused_workspace_idx();

                WindowsApi::bring_window_to_top(window.hwnd)?;

                let pending_move_op = Arc::make_mut(&mut self.pending_move_op);
                *pending_move_op = Option::from((monitor_idx, workspace_idx, window.hwnd));
            }
            WindowManagerEvent::MoveResizeEnd(_, window) => {
                // We need this because if the event ends on a different monitor,
                // that monitor will already have been focused and updated in the state
                let pending = *self.pending_move_op;
                // Always consume the pending move op whenever this event is handled
                let pending_move_op = Arc::make_mut(&mut self.pending_move_op);
                *pending_move_op = None;

                // If the window handles don't match then something went wrong and the pending move
                // is not related to this current move, if so abort this operation.
                if let Some((_, _, w_hwnd)) = pending
                    && w_hwnd != window.hwnd
                {
                    color_eyre::eyre::bail!(
                        "window handles for move operation don't match: {} != {}",
                        w_hwnd,
                        window.hwnd
                    );
                }

                let target_monitor_idx = self
                    .monitor_idx_from_current_pos()
                    .ok_or_eyre("cannot get monitor idx from current position")?;

                let focused_monitor_idx = self.focused_monitor_idx();
                let focused_workspace_idx = self.focused_workspace_idx().unwrap_or_default();
                let window_management_behaviour =
                    self.window_management_behaviour(focused_monitor_idx, focused_workspace_idx);

                let workspace = self.focused_workspace_mut()?;
                let focused_container_idx = workspace.focused_container_idx();
                let new_position = WindowsApi::window_rect(window.hwnd)?;
                let old_position = *workspace
                    .latest_layout
                    .get(focused_container_idx)
                    // If the move was to another monitor with an empty workspace, the
                    // workspace here will refer to that empty workspace, which won't
                    // have any latest layout set. We fall back to a Default for Rect
                    // which allows us to make a reasonable guess that the drag has taken
                    // place across a monitor boundary to an empty workspace
                    .unwrap_or(&Rect::default());

                // This will be true if we have moved to another monitor
                let mut moved_across_monitors = false;

                if let Some((m_idx, _)) = self.known_hwnds.get(&window.hwnd)
                    && *m_idx != target_monitor_idx
                {
                    moved_across_monitors = true;
                }

                if let Some((origin_monitor_idx, origin_workspace_idx, _)) = pending {
                    // If we didn't move to another monitor with an empty workspace, it is
                    // still possible that we moved to another monitor with a populated workspace
                    if !moved_across_monitors {
                        // So we'll check if the origin monitor index and the target monitor index
                        // are different, if they are, we can set the override
                        moved_across_monitors = origin_monitor_idx != target_monitor_idx;

                        if moved_across_monitors {
                            // Want to make sure that we exclude unmanaged windows from cross-monitor
                            // moves with a mouse, otherwise the currently focused idx container will
                            // be moved when we just want to drag an unmanaged window
                            let origin_workspace = self
                                .monitors()
                                .get(origin_monitor_idx)
                                .ok_or_eyre("cannot get monitor idx")?
                                .workspaces()
                                .get(origin_workspace_idx)
                                .ok_or_eyre("cannot get workspace idx")?;

                            let managed_window = origin_workspace.contains_window(window.hwnd);

                            if !managed_window {
                                moved_across_monitors = false;
                            }
                        }
                    }
                }

                let workspace = self.focused_workspace_mut()?;
                if (workspace.tile && workspace.contains_managed_window(window.hwnd))
                    || moved_across_monitors
                {
                    let resize = Rect {
                        left: new_position.left - old_position.left,
                        top: new_position.top - old_position.top,
                        right: new_position.right - old_position.right,
                        bottom: new_position.bottom - old_position.bottom,
                    };

                    // If we have moved across the monitors, use that override, otherwise determine
                    // if a move has taken place by ruling out a resize
                    let right_bottom_constant = 0;

                    let is_move = moved_across_monitors
                        || resize.right.abs() == right_bottom_constant
                            && resize.bottom.abs() == right_bottom_constant;

                    if is_move {
                        tracing::info!("moving with mouse");

                        if moved_across_monitors {
                            if let Some((origin_monitor_idx, origin_workspace_idx, w_hwnd)) =
                                pending
                            {
                                let target_workspace_idx = self
                                    .monitors()
                                    .get(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?
                                    .focused_workspace_idx();

                                let target_container_idx = self
                                    .monitors()
                                    .get(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?
                                    .focused_workspace()
                                    .ok_or_eyre("there is no focused workspace for this monitor")?
                                    .container_idx_from_current_point()
                                    // Default to 0 in the case of an empty workspace
                                    .unwrap_or(0);

                                let origin = (origin_monitor_idx, origin_workspace_idx, w_hwnd);
                                let target = (
                                    target_monitor_idx,
                                    target_workspace_idx,
                                    target_container_idx,
                                );
                                self.transfer_window(origin, target)?;

                                // We want to make sure both the origin and target monitors are updated,
                                // so that we don't have ghost tiles until we force an interaction on
                                // the origin monitor's focused workspace
                                self.focus_monitor(origin_monitor_idx)?;
                                let origin_monitor = self
                                    .monitors_mut()
                                    .get_mut(origin_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?;
                                origin_monitor.focus_workspace(origin_workspace_idx)?;
                                self.update_focused_workspace(false, false)?;

                                self.focus_monitor(target_monitor_idx)?;
                                let target_monitor = self
                                    .monitors_mut()
                                    .get_mut(target_monitor_idx)
                                    .ok_or_eyre("there is no monitor at this idx")?;
                                target_monitor.focus_workspace(target_workspace_idx)?;
                                self.update_focused_workspace(false, false)?;

                                // Make sure to give focus to the moved window again
                                window.focus(self.mouse_follows_focus)?;
                            }
                        } else if window_management_behaviour.float_override {
                            workspace.floating_windows_mut().push_back(window);
                            self.update_focused_workspace(false, false)?;
                        } else {
                            match window_management_behaviour.current_behaviour {
                                WindowContainerBehaviour::Create => {
                                    match workspace.container_idx_from_current_point() {
                                        Some(target_idx) => {
                                            workspace
                                                .swap_containers(focused_container_idx, target_idx);
                                            self.update_focused_workspace(false, false)?;
                                        }
                                        None => {
                                            self.update_focused_workspace(
                                                self.mouse_follows_focus,
                                                false,
                                            )?;
                                        }
                                    }
                                }
                                WindowContainerBehaviour::Append => {
                                    match workspace.container_idx_from_current_point() {
                                        Some(target_idx) => {
                                            workspace.move_window_to_container(target_idx)?;
                                            self.update_focused_workspace(false, false)?;
                                        }
                                        None => {
                                            self.update_focused_workspace(
                                                self.mouse_follows_focus,
                                                false,
                                            )?;
                                        }
                                    }

                                    stackbar_manager::send_notification();
                                }
                            }
                        }
                    } else {
                        tracing::info!("resizing with mouse");
                        let mut ops = vec![];

                        macro_rules! resize_op {
                            ($coordinate:expr, $comparator:tt, $direction:expr) => {{
                                let adjusted = $coordinate * 2;
                                let sizing = if adjusted $comparator 0 {
                                    Sizing::Decrease
                                } else {
                                    Sizing::Increase
                                };

                                ($direction, sizing, adjusted.abs())
                            }};
                        }

                        if resize.left != 0 {
                            ops.push(resize_op!(resize.left, >, OperationDirection::Left));
                        }

                        if resize.top != 0 {
                            ops.push(resize_op!(resize.top, >, OperationDirection::Up));
                        }

                        // TODO: Determine if this is still needed
                        let top_left_constant = BORDER_WIDTH.load(Ordering::SeqCst)
                            + BORDER_OFFSET.load(Ordering::SeqCst);

                        if resize.right != 0
                            && (resize.left == top_left_constant || resize.left == 0)
                        {
                            ops.push(resize_op!(resize.right, <, OperationDirection::Right));
                        }

                        if resize.bottom != 0
                            && (resize.top == top_left_constant || resize.top == 0)
                        {
                            ops.push(resize_op!(resize.bottom, <, OperationDirection::Down));
                        }

                        for (edge, sizing, delta) in ops {
                            self.resize_window(edge, sizing, delta, true)?;
                        }

                        self.update_focused_workspace(false, false)?;
                    }
                }
            }
            WindowManagerEvent::MouseCapture(..)
            | WindowManagerEvent::Cloak(..)
            | WindowManagerEvent::TitleUpdate(..)
            | WindowManagerEvent::DesktopChanged { .. }
            | WindowManagerEvent::DesktopCountChanged
            | WindowManagerEvent::WindowDesktopChanged { .. }
            | WindowManagerEvent::DesktopMoved { .. }
            | WindowManagerEvent::DesktopRenamed { .. } => {}
        };

        // If we unmanaged a window, it shouldn't be immediately hidden behind managed windows
        if let WindowManagerEvent::Unmanage(mut window) = event {
            window.center(&self.focused_monitor_work_area()?, true)?;
        }

        Ok(true)
    }

    fn publish_event_state(
        &mut self,
        event: WindowManagerEvent,
        initial_state: &State,
    ) -> eyre::Result<()> {
        notify_subscribers(
            Notification {
                event: NotificationEvent::WindowManager(event),
                state: self.as_ref().into(),
            },
            initial_state.has_been_modified(self.as_ref()),
        )?;
        border_manager::send_notification(Some(event.hwnd()));
        transparency_manager::send_notification();
        stackbar_manager::send_notification();
        Ok(())
    }

    /// Shared by Show/Manage/Uncloak on both focused and background desktops.
    fn manage_desktop_window(
        &mut self,
        window: Window,
        event: WindowManagerEvent,
    ) -> eyre::Result<()> {
        let is_new = self.managed_window_location(window.hwnd).is_none();
        let mut center_placement = None;
        if is_new {
            let desktop_idx = self.desktop_idx_for_new_window(window)?;
            let monitor_idx = self
                .monitor_idx_from_window(window)
                .unwrap_or(self.focused_monitor_idx());
            // Validate explicit rules before creating associations or performing native effects.
            let rule = self.window_rule_destination(window)?;
            let (target_m, target_d) = rule.map_or((monitor_idx, desktop_idx), |(m, d, _)| (m, d));
            self.validate_window_destination(monitor_idx, desktop_idx)?;
            let behaviour = self.window_management_behaviour(target_m, target_d);
            let should_float = !matches!(event, WindowManagerEvent::Manage(_)) && {
                let rules = FLOATING_APPLICATIONS.lock().clone();
                !rules.is_empty()
                    && self.window_api.identifiers(window).is_ok_and(|ids| {
                        should_act(
                            &ids.title,
                            &ids.exe,
                            &ids.class,
                            &ids.path,
                            &rules,
                            &REGEX_IDENTIFIERS.lock(),
                        )
                        .is_some()
                    })
            };
            let window_api = self.window_api.clone();
            let workspace = &mut self.monitors_mut()[monitor_idx].workspaces_mut()[desktop_idx];
            if behaviour.float_override || behaviour.floating_layer_override || should_float {
                center_placement = Some(if behaviour.floating_layer_override {
                    behaviour.floating_layer_placement
                } else if behaviour.float_override {
                    behaviour.float_override_placement
                } else {
                    behaviour.float_rule_placement
                });
                workspace.floating_windows_mut().push_back(window);
                workspace.layer = WorkspaceLayer::Floating;
            } else if (monitor_idx, desktop_idx) == (target_m, target_d)
                && matches!(
                    behaviour.current_behaviour,
                    WindowContainerBehaviour::Append
                )
                && workspace.focused_container().is_some()
            {
                window_api.append_to_stack(
                    workspace
                        .focused_container_mut()
                        .ok_or_eyre("there is no focused container")?,
                    window,
                );
            } else {
                workspace.new_container_for_window(window);
            }
            self.update_known_hwnds();
        }
        self.apply_window_workspace_rule(window)?;
        let (monitor_idx, desktop_idx) = self
            .managed_window_location(window.hwnd)
            .ok_or_eyre("managed window disappeared")?;
        if let Some(placement) = center_placement {
            let offset = self.work_area_offset;
            self.monitors_mut()[monitor_idx].update_workspace_globals(desktop_idx, offset);
            let workspace = &self.monitors()[monitor_idx].workspaces()[desktop_idx];
            if placement.should_center() && workspace.tile {
                self.window_api.center_window(
                    window,
                    &workspace.globals.work_area,
                    placement.should_resize(),
                )?;
            }
        }
        // Show/Uncloak alone do not authorize focus changes. Mirror a foreground
        // selection Windows already made only on the globally current desktop.
        if desktop_idx == self.focused_desktop_idx
            && self.window_api.foreground_window().ok() == Some(window.hwnd)
        {
            self.focus_monitor(monitor_idx)?;
            let api = self.window_api.clone();
            let workspace = &mut self.monitors_mut()[monitor_idx].workspaces_mut()[desktop_idx];
            if let Some(idx) = workspace
                .floating_windows()
                .iter()
                .position(|w| w.hwnd == window.hwnd)
            {
                workspace.floating_windows.focus(idx);
                workspace.layer = WorkspaceLayer::Floating;
            } else if let Some(container) = &mut workspace.monocle_container
                && let Some(idx) = container.idx_for_window(window.hwnd)
            {
                container.focus_window(idx);
                api.refresh_stack(container);
            } else if let Some(idx) = workspace.container_idx_for_window(window.hwnd) {
                if workspace.monocle_container.is_some() {
                    for container in workspace.containers() {
                        if let Some(window) = container.focused_window() {
                            api.restore_window(*window);
                        }
                    }
                    for window in workspace.floating_windows() {
                        api.restore_window(*window);
                    }
                    workspace.reintegrate_monocle_container()?;
                }
                // Reintegration may have changed container indices.
                let idx = workspace
                    .container_idx_for_window(window.hwnd)
                    .unwrap_or(idx);
                workspace.focus_container(idx);
                let container = &mut workspace.containers_mut()[idx];
                let window_idx = container
                    .idx_for_window(window.hwnd)
                    .ok_or_eyre("there is no stack window")?;
                container.focus_window(window_idx);
                api.refresh_stack(container);
                workspace.layer = WorkspaceLayer::Tiling;
            }
        }
        self.update_desktop_workspace(monitor_idx, desktop_idx)
    }
}
