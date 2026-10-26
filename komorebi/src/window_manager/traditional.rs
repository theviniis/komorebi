//! Traditional per-monitor workspace behavior, retained from before native desktop integration.
use super::*;
use crate::{MatchingRule, should_act_individual};

impl EnforceWorkspaceRuleOp {
    const fn is_origin(&self, monitor_idx: usize, workspace_idx: usize) -> bool {
        self.origin_monitor_idx == monitor_idx && self.origin_workspace_idx == workspace_idx
    }

    const fn is_target(&self, monitor_idx: usize, workspace_idx: usize) -> bool {
        self.target_monitor_idx == monitor_idx && self.target_workspace_idx == workspace_idx
    }

    const fn is_enforced(&self) -> bool {
        (self.origin_monitor_idx == self.target_monitor_idx)
            && (self.origin_workspace_idx == self.target_workspace_idx)
    }
}

#[derive(Debug, Clone, Copy)]
struct EnforceWorkspaceRuleOp {
    hwnd: isize,
    origin_monitor_idx: usize,
    origin_workspace_idx: usize,
    target_monitor_idx: usize,
    target_workspace_idx: usize,
    floating: bool,
}

impl WindowManager {
    pub(super) fn traditional_apply_state(&mut self, state: State) -> eyre::Result<()> {
        let mut can_apply = true;

        let state_monitors_len = state.monitors.elements().len();
        let current_monitors_len = self.monitors.elements().len();
        if state_monitors_len != current_monitors_len {
            tracing::warn!(
                "cannot apply state from {}; state file has {state_monitors_len} monitors, but only {current_monitors_len} are currently connected",
                temp_dir().join("komorebi.state.json").to_string_lossy()
            );

            return Ok(());
        }

        for monitor in state.monitors.elements() {
            for workspace in monitor.workspaces() {
                for container in workspace.containers() {
                    for window in container.windows() {
                        if window.exe().is_err() {
                            can_apply = false;
                            break;
                        }
                    }
                }

                if let Some(window) = workspace.maximized_window
                    && window.exe().is_err()
                {
                    can_apply = false;
                    break;
                }

                if let Some(container) = &workspace.monocle_container {
                    for window in container.windows() {
                        if window.exe().is_err() {
                            can_apply = false;
                            break;
                        }
                    }
                }

                for window in workspace.floating_windows() {
                    if window.exe().is_err() {
                        can_apply = false;
                        break;
                    }
                }
            }
        }

        if can_apply {
            tracing::info!(
                "applying state from {}",
                temp_dir().join("komorebi.state.json").to_string_lossy()
            );

            let offset = self.work_area_offset;
            let mouse_follows_focus = self.mouse_follows_focus;
            for (monitor_idx, monitor) in self.monitors_mut().iter_mut().enumerate() {
                let mut focused_workspace = 0;
                if let Some(state_monitor) = state.monitors.elements().get(monitor_idx) {
                    monitor
                        .workspaces_mut()
                        .resize(state_monitor.workspaces().len(), Workspace::default());

                    for (workspace_idx, workspace) in
                        monitor.workspaces_mut().iter_mut().enumerate()
                    {
                        if let Some(state_workspace) = state_monitor.workspaces().get(workspace_idx)
                        {
                            // to make sure padding and layout_options changes get applied for users after a quick restart
                            let container_padding = workspace.container_padding;
                            let workspace_padding = workspace.workspace_padding;
                            let layout_options = workspace.layout_options;

                            *workspace = state_workspace.clone();

                            workspace.container_padding = container_padding;
                            workspace.workspace_padding = workspace_padding;
                            workspace.layout_options = layout_options;

                            if state_monitor.focused_workspace_idx() == workspace_idx {
                                focused_workspace = workspace_idx;
                            }
                        }
                    }
                }

                if let Err(error) = monitor.focus_workspace(focused_workspace) {
                    tracing::warn!(
                        "cannot focus workspace '{focused_workspace}' on monitor '{monitor_idx}' from {}: {}",
                        temp_dir().join("komorebi.state.json").to_string_lossy(),
                        error,
                    );
                }

                if let Err(error) = monitor.load_focused_workspace(mouse_follows_focus) {
                    tracing::warn!(
                        "cannot load focused workspace '{focused_workspace}' on monitor '{monitor_idx}' from {}: {}",
                        temp_dir().join("komorebi.state.json").to_string_lossy(),
                        error,
                    );
                }

                if let Err(error) = monitor.update_focused_workspace(offset) {
                    tracing::warn!(
                        "cannot update workspace '{focused_workspace}' on monitor '{monitor_idx}' from {}: {}",
                        temp_dir().join("komorebi.state.json").to_string_lossy(),
                        error,
                    );
                }
            }

            let focused_monitor_idx = state.monitors.focused_idx();
            let focused_workspace_idx = state
                .monitors
                .elements()
                .get(focused_monitor_idx)
                .map(|m| m.focused_workspace_idx())
                .unwrap_or_default();

            if let Err(error) = self.focus_monitor(focused_monitor_idx) {
                tracing::warn!(
                    "cannot focus monitor '{focused_monitor_idx}' from {}: {}",
                    temp_dir().join("komorebi.state.json").to_string_lossy(),
                    error,
                );
            }

            if let Err(error) = self.focus_workspace(focused_workspace_idx) {
                tracing::warn!(
                    "cannot focus workspace '{focused_workspace_idx}' on monitor '{focused_monitor_idx}' from {}: {}",
                    temp_dir().join("komorebi.state.json").to_string_lossy(),
                    error,
                );
            }

            if let Err(error) = self.update_focused_workspace(true, true) {
                tracing::warn!(
                    "cannot update focused workspace '{focused_workspace_idx}' on monitor '{focused_monitor_idx}' from {}: {}",
                    temp_dir().join("komorebi.state.json").to_string_lossy(),
                    error,
                );
            }
        } else {
            tracing::warn!(
                "cannot apply state from {}; some windows referenced in the state file no longer exist",
                temp_dir().join("komorebi.state.json").to_string_lossy()
            );
        }
        self.update_known_hwnds();
        Ok(())
    }

    pub(super) fn traditional_enforce_workspace_rules(&mut self) -> eyre::Result<()> {
        let mut to_move = vec![];

        let focused_monitor_idx = self.focused_monitor_idx();
        let focused_workspace_idx = self
            .monitors()
            .get(focused_monitor_idx)
            .ok_or_eyre("there is no monitor with that index")?
            .focused_workspace_idx();

        // scope mutex locks to avoid deadlock if should_update_focused_workspace evaluates to true
        // at the end of this function
        {
            let workspace_matching_rules = WORKSPACE_MATCHING_RULES.lock();
            let regex_identifiers = REGEX_IDENTIFIERS.lock();
            // Go through all the monitors and workspaces
            for (i, monitor) in self.monitors().iter().enumerate() {
                for (j, workspace) in monitor.workspaces().iter().enumerate() {
                    // And all the visible windows (at the top of a container)
                    for window in workspace.visible_windows().into_iter().flatten() {
                        let mut already_moved_window_handles =
                            self.already_moved_window_handles.lock();

                        if let (Ok(exe_name), Ok(title), Ok(class), Ok(path)) =
                            (window.exe(), window.title(), window.class(), window.path())
                        {
                            for rule in &*workspace_matching_rules {
                                let matched = match &rule.matching_rule {
                                    MatchingRule::Simple(r) => should_act_individual(
                                        &title,
                                        &exe_name,
                                        &class,
                                        &path,
                                        r,
                                        &regex_identifiers,
                                    ),
                                    MatchingRule::Composite(r) => {
                                        let mut composite_results = vec![];
                                        for identifier in r {
                                            composite_results.push(should_act_individual(
                                                &title,
                                                &exe_name,
                                                &class,
                                                &path,
                                                identifier,
                                                &regex_identifiers,
                                            ));
                                        }

                                        composite_results.iter().all(|&x| x)
                                    }
                                };

                                if matched {
                                    let floating = workspace.floating_windows().contains(window);

                                    if rule.initial_only {
                                        if !already_moved_window_handles.contains(&window.hwnd) {
                                            already_moved_window_handles.insert(window.hwnd);

                                            self.add_window_handle_to_move_based_on_workspace_rule(
                                                &window.title()?,
                                                window.hwnd,
                                                i,
                                                j,
                                                rule.monitor_index,
                                                rule.workspace_index,
                                                floating,
                                                &mut to_move,
                                            );
                                        }
                                    } else {
                                        self.add_window_handle_to_move_based_on_workspace_rule(
                                            &window.title()?,
                                            window.hwnd,
                                            i,
                                            j,
                                            rule.monitor_index,
                                            rule.workspace_index,
                                            floating,
                                            &mut to_move,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Only retain operations where the target is not the current workspace
        to_move.retain(|op| !op.is_target(focused_monitor_idx, focused_workspace_idx));
        // Only retain operations where the rule has not already been enforced
        to_move.retain(|op| !op.is_enforced());

        let mut should_update_focused_workspace = false;

        // Parse the operation and remove any windows that are not placed according to their rules
        for op in &to_move {
            let target_area = self
                .monitors_mut()
                .get_mut(op.target_monitor_idx)
                .ok_or_eyre("there is no monitor with that index")?
                .work_area_size;

            let origin_monitor = self
                .monitors_mut()
                .get_mut(op.origin_monitor_idx)
                .ok_or_eyre("there is no monitor with that index")?;

            let origin_area = origin_monitor.work_area_size;

            let origin_workspace = origin_monitor
                .workspaces_mut()
                .get_mut(op.origin_workspace_idx)
                .ok_or_eyre("there is no workspace with that index")?;

            let mut window = Window::from(op.hwnd);

            // If it is a floating window move it to the target area
            if op.floating {
                window.move_to_area(&origin_area, &target_area)?;
            }

            // Hide the window we are about to remove if it is on the currently focused workspace
            if op.is_origin(focused_monitor_idx, focused_workspace_idx) {
                window.hide();
                should_update_focused_workspace = true;
            }

            origin_workspace.remove_window(op.hwnd)?;
        }

        // Parse the operation again and associate those removed windows with the workspace that
        // their rules have defined for them
        for op in &to_move {
            let target_monitor = self
                .monitors_mut()
                .get_mut(op.target_monitor_idx)
                .ok_or_eyre("there is no monitor with that index")?;

            // The very first time this fn is called, the workspace might not even exist yet
            if target_monitor
                .workspaces()
                .get(op.target_workspace_idx)
                .is_none()
            {
                // If it doesn't, let's make sure it does for the next step
                target_monitor.ensure_workspace_count(op.target_workspace_idx + 1);
            }

            let target_workspace = target_monitor
                .workspaces_mut()
                .get_mut(op.target_workspace_idx)
                .ok_or_eyre("there is no workspace with that index")?;

            if op.floating {
                target_workspace
                    .floating_windows_mut()
                    .push_back(Window::from(op.hwnd));
            } else {
                //TODO(alex-ds13): should this take into account the target workspace
                //`window_container_behaviour`?
                //In the case above a floating window should always be moved as floating,
                //because it was set as so either manually by the user or by a
                //`floating_applications` rule so it should stay that way. But a tiled window
                //when moving to another workspace by a `workspace_rule` should honor that
                //workspace `window_container_behaviour` in my opinion! Maybe this should be done
                //on the `new_container_for_window` function instead.
                target_workspace.new_container_for_window(Window::from(op.hwnd));
            }
        }

        // Only re-tile the focused workspace if we need to
        if should_update_focused_workspace {
            self.update_focused_workspace(false, false)?;
        }

        Ok(())
    }

    pub(super) fn traditional_swap_monitor_workspaces(
        &mut self,
        first_idx: usize,
        second_idx: usize,
    ) -> eyre::Result<()> {
        tracing::info!("swaping monitors");
        if first_idx == second_idx {
            return Ok(());
        }
        let mouse_follows_focus = self.mouse_follows_focus;
        let offset = self.work_area_offset;
        let first_focused_workspace = {
            let first_monitor = self
                .monitors()
                .get(first_idx)
                .ok_or_eyre("There is no monitor")?;
            first_monitor.focused_workspace_idx()
        };

        let second_focused_workspace = {
            let second_monitor = self
                .monitors()
                .get(second_idx)
                .ok_or_eyre("There is no monitor")?;
            second_monitor.focused_workspace_idx()
        };

        // Swap workspaces between the first and second monitors

        let first_workspaces = self
            .monitors_mut()
            .get_mut(first_idx)
            .ok_or_eyre("There is no monitor")?
            .remove_workspaces();

        let second_workspaces = self
            .monitors_mut()
            .get_mut(second_idx)
            .ok_or_eyre("There is no monitor")?
            .remove_workspaces();

        self.monitors_mut()
            .get_mut(first_idx)
            .ok_or_eyre("There is no monitor")?
            .workspaces_mut()
            .extend(second_workspaces);

        self.monitors_mut()
            .get_mut(second_idx)
            .ok_or_eyre("There is no monitor")?
            .workspaces_mut()
            .extend(first_workspaces);

        // Set the focused workspaces for the first and second monitors
        if let Some(first_monitor) = self.monitors_mut().get_mut(first_idx) {
            first_monitor.update_workspaces_globals(offset);
            first_monitor.focus_workspace(second_focused_workspace)?;
            first_monitor.load_focused_workspace(mouse_follows_focus)?;
        }

        if let Some(second_monitor) = self.monitors_mut().get_mut(second_idx) {
            second_monitor.update_workspaces_globals(offset);
            second_monitor.focus_workspace(first_focused_workspace)?;
            second_monitor.load_focused_workspace(mouse_follows_focus)?;
        }

        self.update_focused_workspace_by_monitor_idx(second_idx)?;
        self.update_focused_workspace_by_monitor_idx(first_idx)
    }

    pub(super) fn traditional_move_container_to_monitor(
        &mut self,
        monitor_idx: usize,
        workspace_idx: Option<usize>,
        follow: bool,
        move_direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("moving container");

        let focused_monitor_idx = self.focused_monitor_idx();

        if focused_monitor_idx == monitor_idx
            && let Some(workspace_idx) = workspace_idx
        {
            return self.move_container_to_workspace(workspace_idx, follow, None);
        }

        let offset = self.work_area_offset;
        let mouse_follows_focus = self.mouse_follows_focus;

        let monitor = self
            .focused_monitor_mut()
            .ok_or_eyre("there is no monitor")?;

        let current_area = monitor.work_area_size;

        let workspace = monitor
            .focused_workspace_mut()
            .ok_or_eyre("there is no workspace")?;

        if workspace.maximized_window.is_some() {
            bail!("cannot move native maximized window to another monitor or workspace");
        }

        let foreground_hwnd = WindowsApi::foreground_window()?;
        let floating_window_index = workspace
            .floating_windows()
            .iter()
            .position(|w| w.hwnd == foreground_hwnd);

        let floating_window =
            floating_window_index.and_then(|idx| workspace.floating_windows_mut().remove(idx));
        let container = if floating_window_index.is_none() {
            Some(
                workspace
                    .remove_focused_container()
                    .ok_or_eyre("there is no container")?,
            )
        } else {
            None
        };
        monitor.update_focused_workspace(offset)?;

        let target_monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let mut should_load_workspace = false;
        if let Some(workspace_idx) = workspace_idx
            && workspace_idx != target_monitor.focused_workspace_idx()
        {
            target_monitor.focus_workspace(workspace_idx)?;
            should_load_workspace = true;
        }
        let target_workspace = target_monitor
            .focused_workspace_mut()
            .ok_or_eyre("there is no focused workspace on target monitor")?;

        if target_workspace.monocle_container.is_some() {
            for container in target_workspace.containers_mut() {
                container.restore();
            }

            for window in target_workspace.floating_windows_mut() {
                window.restore();
            }

            target_workspace.reintegrate_monocle_container()?;
        }

        if let Some(window) = floating_window {
            target_workspace.floating_windows_mut().push_back(window);
            target_workspace.layer = WorkspaceLayer::Floating;
            Window::from(window.hwnd)
                .move_to_area(&current_area, &target_monitor.work_area_size)?;
        } else if let Some(container) = container {
            let container_hwnds = container
                .windows()
                .iter()
                .map(|w| w.hwnd)
                .collect::<Vec<_>>();

            target_workspace.layer = WorkspaceLayer::Tiling;

            if let Some(direction) = move_direction {
                target_monitor.add_container_with_direction(container, workspace_idx, direction)?;
            } else {
                target_monitor.add_container(container, workspace_idx)?;
            }

            if let Some(workspace) = target_monitor.focused_workspace()
                && !workspace.tile
            {
                for hwnd in container_hwnds {
                    Window::from(hwnd)
                        .move_to_area(&current_area, &target_monitor.work_area_size)?;
                }
            }
        } else {
            bail!("failed to find a window to move");
        }

        if should_load_workspace {
            target_monitor.load_focused_workspace(mouse_follows_focus)?;
        }
        target_monitor.update_focused_workspace(offset)?;

        // this second one is for DPI changes when the target is another monitor
        // if we don't do this the layout on the other monitor could look funny
        // until it is interacted with again
        target_monitor.update_focused_workspace(offset)?;

        if follow {
            self.focus_monitor(monitor_idx)?;
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)?;

        Ok(())
    }

    pub(super) fn traditional_move_container_to_workspace(
        &mut self,
        idx: usize,
        follow: bool,
        direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("moving container");

        let mouse_follows_focus = self.mouse_follows_focus;
        let monitor = self
            .focused_monitor_mut()
            .ok_or_eyre("there is no monitor")?;

        monitor.move_container_to_workspace(idx, follow, direction)?;
        monitor.load_focused_workspace(mouse_follows_focus)?;

        self.update_focused_workspace(mouse_follows_focus, true)?;

        Ok(())
    }

    pub(super) fn traditional_remove_focused_workspace(&mut self) -> Option<Workspace> {
        let focused_monitor: &mut Monitor = self.focused_monitor_mut()?;
        let focused_workspace_idx = focused_monitor.focused_workspace_idx();
        let workspace = focused_monitor.remove_workspace_by_idx(focused_workspace_idx);
        if let Err(error) = focused_monitor.focus_workspace(focused_workspace_idx.saturating_sub(1))
        {
            tracing::error!(
                "Error focusing previous workspace while removing the focused workspace: {}",
                error
            );
        }
        workspace
    }

    pub(super) fn traditional_move_workspace_to_monitor(&mut self, idx: usize) -> eyre::Result<()> {
        tracing::info!("moving workspace");
        let mouse_follows_focus = self.mouse_follows_focus;
        let offset = self.work_area_offset;
        let workspace = self
            .remove_focused_workspace()
            .ok_or_eyre("there is no workspace")?;

        {
            let target_monitor: &mut Monitor = self
                .monitors_mut()
                .get_mut(idx)
                .ok_or_eyre("there is no monitor")?;

            target_monitor.workspaces_mut().push_back(workspace);
            target_monitor.update_workspaces_globals(offset);
            target_monitor.focus_workspace(target_monitor.workspaces().len().saturating_sub(1))?;
            target_monitor.load_focused_workspace(mouse_follows_focus)?;
        }

        self.focus_monitor(idx)?;
        self.update_focused_workspace(mouse_follows_focus, true)
    }

    pub(super) fn traditional_ensure_workspaces_for_monitor(
        &mut self,
        monitor_idx: usize,
        workspace_count: usize,
    ) -> eyre::Result<()> {
        tracing::info!("ensuring workspace count");

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        monitor.ensure_workspace_count(workspace_count);

        Ok(())
    }

    pub(super) fn traditional_ensure_named_workspaces_for_monitor(
        &mut self,
        monitor_idx: usize,
        names: &Vec<String>,
    ) -> eyre::Result<()> {
        tracing::info!("ensuring workspace count");

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        monitor.ensure_workspace_count(names.len());

        for (workspace_idx, name) in names.iter().enumerate() {
            if let Some(workspace) = monitor.workspaces_mut().get_mut(workspace_idx) {
                workspace.name = Option::from(name.clone());
            }
        }

        Ok(())
    }

    pub(super) fn traditional_set_workspace_name(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        name: String,
    ) -> eyre::Result<()> {
        tracing::info!("setting workspace name");

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.name = Option::from(name.clone());
        monitor.workspace_names.insert(workspace_idx, name);

        Ok(())
    }

    pub(super) fn traditional_focused_workspace_idx(&self) -> eyre::Result<usize> {
        Ok(self
            .focused_monitor()
            .ok_or_eyre("there is no monitor")?
            .focused_workspace_idx())
    }

    pub(super) fn traditional_focused_workspace(&self) -> eyre::Result<&Workspace> {
        self.focused_monitor()
            .ok_or_eyre("there is no monitor")?
            .focused_workspace()
            .ok_or_eyre("there is no workspace")
    }

    pub(super) fn traditional_focused_workspace_mut(&mut self) -> eyre::Result<&mut Workspace> {
        self.focused_monitor_mut()
            .ok_or_eyre("there is no monitor")?
            .focused_workspace_mut()
            .ok_or_eyre("there is no workspace")
    }

    pub(super) fn traditional_focused_workspace_idx_for_monitor_idx(
        &self,
        idx: usize,
    ) -> eyre::Result<usize> {
        Ok(self
            .monitors()
            .get(idx)
            .ok_or_eyre("there is no monitor at this index")?
            .focused_workspace_idx())
    }

    pub(super) fn traditional_focus_workspace(&mut self, idx: usize) -> eyre::Result<()> {
        tracing::info!("focusing workspace");

        let mouse_follows_focus = self.mouse_follows_focus;
        let monitor = self
            .focused_monitor_mut()
            .ok_or_eyre("there is no workspace")?;

        monitor.focus_workspace(idx)?;
        monitor.load_focused_workspace(mouse_follows_focus)?;

        self.update_focused_workspace(false, true)
    }

    pub(super) fn traditional_monitor_workspace_index_by_name(
        &mut self,
        name: &str,
    ) -> Option<(usize, usize)> {
        tracing::info!("looking up workspace by name");

        for (monitor_idx, monitor) in self.monitors().iter().enumerate() {
            for (workspace_idx, workspace) in monitor.workspaces().iter().enumerate() {
                if let Some(workspace_name) = &workspace.name
                    && workspace_name == name
                {
                    return Option::from((monitor_idx, workspace_idx));
                }
            }
        }

        None
    }

    pub(super) fn traditional_new_workspace(&mut self) -> eyre::Result<()> {
        tracing::info!("adding new workspace");

        let mouse_follows_focus = self.mouse_follows_focus;
        let monitor = self
            .focused_monitor_mut()
            .ok_or_eyre("there is no workspace")?;

        monitor.focus_workspace(monitor.new_workspace_idx())?;
        monitor.load_focused_workspace(mouse_follows_focus)?;

        self.update_focused_workspace(self.mouse_follows_focus, false)
    }

    fn add_window_handle_to_move_based_on_workspace_rule(
        &self,
        window_title: &String,
        hwnd: isize,
        origin_monitor_idx: usize,
        origin_workspace_idx: usize,
        target_monitor_idx: usize,
        target_workspace_idx: usize,
        floating: bool,
        to_move: &mut Vec<EnforceWorkspaceRuleOp>,
    ) {
        tracing::trace!(
            "{} should be on monitor {}, workspace {}",
            window_title,
            target_monitor_idx,
            target_workspace_idx
        );

        // Create an operation outline and save it for later in the fn
        to_move.push(EnforceWorkspaceRuleOp {
            hwnd,
            origin_monitor_idx,
            origin_workspace_idx,
            target_monitor_idx,
            target_workspace_idx,
            floating,
        });
    }
}
