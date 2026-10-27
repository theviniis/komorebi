//! Traditional per-monitor workspace behavior, retained from before native desktop integration.
use super::*;

impl EnforceWorkspaceRuleOp {
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
    initial_only: bool,
}

impl WindowManager {
    /// Build the restored model before changing visibility. Match physical displays by
    /// identity and retain windows discovered since the snapshot was written.
    fn traditional_restored_monitors(&self, state: &State) -> eyre::Result<Ring<Monitor>> {
        if state.monitors.elements().len() != self.monitors().len() {
            bail!("saved state monitor count differs from connected monitors");
        }
        let mut restored = self.monitors.clone();
        let mut used = HashSet::new();
        let mut seen = HashSet::new();
        for monitor in restored.elements_mut().iter_mut() {
            let candidates = state
                .monitors
                .elements()
                .iter()
                .enumerate()
                .filter(|(_, saved)| crate::monitor_reconciliator::same_device(monitor, saved))
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            if candidates.len() != 1 || !used.insert(candidates[0]) {
                bail!("saved state monitor identity differs from connected monitors");
            }
            let saved_idx = candidates[0];
            let saved = &state.monitors.elements()[saved_idx];
            monitor.ensure_workspace_count(saved.workspaces().len().max(1));
            for (idx, source) in saved.workspaces().iter().enumerate() {
                let target = &mut monitor.workspaces_mut()[idx];
                let (padding, workspace_padding, options) = (
                    target.container_padding,
                    target.workspace_padding,
                    target.layout_options,
                );
                *target = source.clone();
                target.container_padding = padding;
                target.workspace_padding = workspace_padding;
                target.layout_options = options;
                for hwnd in Self::workspace_hwnds(target) {
                    if !self.window_api.is_window(hwnd) {
                        bail!("saved state references a window that no longer exists");
                    }
                    seen.insert(hwnd);
                }
            }
            for target in monitor
                .workspaces_mut()
                .iter_mut()
                .skip(saved.workspaces().len())
            {
                target.containers_mut().clear();
                target.floating_windows_mut().clear();
                target.monocle_container = None;
                target.maximized_window = None;
                target.resize_dimensions.clear();
            }
            monitor.set_focused_workspace_idx(
                saved
                    .focused_workspace_idx()
                    .min(monitor.workspaces().len().saturating_sub(1)),
            );
        }
        // The startup enumeration includes new windows absent from the saved file.
        // Preserve their current monitor/workspace and avoid duplicating saved HWNDs.
        for (m, current) in self.monitors().iter().enumerate() {
            for (d, source) in current.workspaces().iter().enumerate() {
                restored.elements_mut()[m].ensure_workspace_count(d + 1);
                let target = &mut restored.elements_mut()[m].workspaces_mut()[d];
                for source_container in source
                    .containers()
                    .iter()
                    .chain(source.monocle_container.iter())
                {
                    let mut container = source_container.clone();
                    let selected = container.focused_window().map(|w| w.hwnd);
                    container
                        .windows_mut()
                        .retain(|w| self.window_api.is_window(w.hwnd) && seen.insert(w.hwnd));
                    if !container.windows().is_empty() {
                        let selected_idx = selected
                            .and_then(|hwnd| {
                                container.windows().iter().position(|w| w.hwnd == hwnd)
                            })
                            .unwrap_or_default();
                        container.focus_window(selected_idx);
                        target.add_container_to_back(container);
                    }
                }
                for &window in source
                    .floating_windows()
                    .iter()
                    .chain(source.maximized_window.iter())
                {
                    if self.window_api.is_window(window.hwnd) && seen.insert(window.hwnd) {
                        target.floating_windows_mut().push_back(window);
                    }
                }
            }
        }
        if let Some(saved) = state.monitors.focused() {
            if let Some(idx) = restored
                .elements()
                .iter()
                .position(|current| crate::monitor_reconciliator::same_device(current, saved))
            {
                restored.focus(idx);
            }
        }
        Ok(restored)
    }

    pub(super) fn traditional_apply_state(&mut self, state: State) -> eyre::Result<()> {
        self.monitors = self.traditional_restored_monitors(&state)?;
        let offset = self.work_area_offset;
        let mouse_follows_focus = self.mouse_follows_focus;
        for monitor in self.monitors_mut() {
            monitor.load_focused_workspace(false)?;
            monitor.update_focused_workspace(offset)?;
        }
        self.update_known_hwnds();
        self.update_focused_workspace(mouse_follows_focus, true)
    }

    fn traditional_foreground_to_restore(&self, foreground: Option<isize>) -> Option<Window> {
        if let Some(hwnd) = foreground.filter(|hwnd| self.window_api.is_window(*hwnd)) {
            let hidden = self
                .managed_window_location(hwnd)
                .is_some_and(|(m, d)| self.monitors()[m].focused_workspace_idx() != d);
            if !hidden {
                return Some(Window::from(hwnd));
            }
        }
        let workspace = self.focused_workspace().ok()?;
        workspace
            .maximized_window
            .or_else(|| {
                workspace
                    .monocle_container
                    .as_ref()?
                    .focused_window()
                    .copied()
            })
            .or_else(|| {
                if workspace.layer == WorkspaceLayer::Floating {
                    workspace.focused_floating_window().copied()
                } else {
                    None
                }
            })
            .or_else(|| self.focused_window().ok().copied())
    }

    fn traditional_prepare_rule_ops(
        &self,
        mut ops: Vec<EnforceWorkspaceRuleOp>,
    ) -> eyre::Result<Vec<EnforceWorkspaceRuleOp>> {
        ops.retain(|op| !op.is_enforced());
        for op in &ops {
            self.monitors()
                .get(op.target_monitor_idx)
                .ok_or_eyre("there is no monitor with that index")?;
        }
        Ok(ops)
    }

    fn traditional_rule_destination(
        &self,
        window: Window,
    ) -> eyre::Result<Option<(usize, usize, bool)>> {
        let rules = WORKSPACE_MATCHING_RULES.lock().clone();
        let Ok(ids) = self.window_api.identifiers(window) else {
            return Ok(None);
        };
        let already_applied = self
            .already_moved_window_handles
            .lock()
            .contains(&window.hwnd);
        let regex = REGEX_IDENTIFIERS.lock();
        let destination = rules
            .iter()
            .rev()
            .find(|rule| {
                !(rule.initial_only && already_applied)
                    && crate::window::should_act(
                        &ids.title,
                        &ids.exe,
                        &ids.class,
                        &ids.path,
                        std::slice::from_ref(&rule.matching_rule),
                        &regex,
                    )
                    .is_some()
            })
            .map(|rule| (rule.monitor_index, rule.workspace_index, rule.initial_only));
        drop(regex);
        if let Some((m, _, _)) = destination {
            // Traditional rules are allowed to create a workspace, but never a monitor.
            self.monitors()
                .get(m)
                .ok_or_eyre("there is no monitor with that index")?;
        }
        Ok(destination)
    }

    fn traditional_collect_rule_ops(&self) -> eyre::Result<Vec<EnforceWorkspaceRuleOp>> {
        let mut ops = Vec::new();
        let mut seen = HashSet::new();
        for (m, monitor) in self.monitors().iter().enumerate() {
            for (d, workspace) in monitor.workspaces().iter().enumerate() {
                for window in workspace.visible_windows().into_iter().flatten() {
                    if !seen.insert(window.hwnd) {
                        continue;
                    }
                    if let Some((target_monitor_idx, target_workspace_idx, initial_only)) =
                        self.traditional_rule_destination(*window)?
                    {
                        ops.push(EnforceWorkspaceRuleOp {
                            hwnd: window.hwnd,
                            origin_monitor_idx: m,
                            origin_workspace_idx: d,
                            target_monitor_idx,
                            target_workspace_idx,
                            initial_only,
                            floating: workspace.floating_windows().contains(window),
                        });
                    }
                }
            }
        }
        Ok(ops)
    }

    pub(super) fn traditional_enforce_workspace_rules(&mut self) -> eyre::Result<()> {
        let matched = self.traditional_collect_rule_ops()?;
        let to_move = self.traditional_prepare_rule_ops(matched.clone())?;
        // Already being at the selected destination also satisfies an initial rule.
        for op in matched
            .iter()
            .filter(|op| op.is_enforced() && op.initial_only)
        {
            self.already_moved_window_handles.lock().insert(op.hwnd);
        }
        let foreground = self.window_api.foreground_window().ok();
        if to_move.is_empty() {
            return Ok(());
        }
        let mut affected_monitors = HashSet::new();

        let window_api = self.window_api.clone();
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
            let origin_visible = origin_monitor.focused_workspace_idx() == op.origin_workspace_idx;

            let origin_workspace = origin_monitor
                .workspaces_mut()
                .get_mut(op.origin_workspace_idx)
                .ok_or_eyre("there is no workspace with that index")?;

            let window = Window::from(op.hwnd);

            // If it is a floating window move it to the target area
            if op.floating {
                window_api.move_window_to_area(window, &origin_area, &target_area)?;
            }

            // A monitor's selected workspace is visible even when another monitor has focus.
            window.hide();
            if origin_visible {
                affected_monitors.insert(op.origin_monitor_idx);
            }
            affected_monitors.insert(op.target_monitor_idx);

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

        let offset = self.work_area_offset;
        let result: eyre::Result<()> = (|| {
            for idx in affected_monitors {
                let monitor = &mut self.monitors_mut()[idx];
                monitor.load_focused_workspace(false)?;
                monitor.update_focused_workspace(offset)?;
            }
            Ok(())
        })();
        // Workspace::restore focuses a window even when mouse-follow is false.
        // Rule enforcement must preserve the user's foreground window.
        let focus_result = if let Some(window) = self.traditional_foreground_to_restore(foreground)
        {
            self.window_api.focus_window(window, false)
        } else {
            Ok(())
        };
        result?;
        for op in to_move.iter().filter(|op| op.initial_only) {
            self.already_moved_window_handles.lock().insert(op.hwnd);
        }
        focus_result
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
        let target = self
            .monitors()
            .get(monitor_idx)
            .ok_or_eyre("there is no monitor")?;
        let target_workspace_idx = workspace_idx.unwrap_or(target.focused_workspace_idx());
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

        let foreground_hwnd = self.window_api.foreground_window()?;
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
        let target_monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        target_monitor.ensure_workspace_count(target_workspace_idx + 1);
        if follow {
            target_monitor.focus_workspace(target_workspace_idx)?;
        }
        let target_workspace = &mut target_monitor.workspaces_mut()[target_workspace_idx];

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
                target_monitor.add_container_with_direction(
                    container,
                    Some(target_workspace_idx),
                    direction,
                )?;
            } else {
                target_monitor.add_container(container, Some(target_workspace_idx))?;
            }

            if let Some(workspace) = target_monitor.workspaces().get(target_workspace_idx)
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

        let result: eyre::Result<()> = (|| {
            let target_monitor = &mut self.monitors_mut()[monitor_idx];
            target_monitor.load_focused_workspace(follow && mouse_follows_focus)?;
            target_monitor.update_focused_workspace(offset)?;
            // Repeat for the destination DPI before returning focus to the origin.
            target_monitor.update_focused_workspace(offset)?;
            if follow {
                self.focus_monitor(monitor_idx)?;
            }
            self.update_focused_workspace(self.mouse_follows_focus, true)
        })();
        if !follow {
            let focus_result = if let Some(window) =
                self.traditional_foreground_to_restore(Some(foreground_hwnd))
            {
                self.window_api.focus_window(window, false)
            } else {
                Ok(())
            };
            return result.and(focus_result);
        }
        result
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
        self.monitors().get(idx).ok_or_eyre("there is no monitor")?;
        let origin_idx = self.focused_monitor_idx();
        if origin_idx == idx {
            return Ok(());
        }
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

        let origin = &mut self.monitors_mut()[origin_idx];
        origin.load_focused_workspace(false)?;
        origin.update_focused_workspace(offset)?;

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
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    fn manager() -> WindowManager {
        let (_, incoming) = crossbeam_channel::unbounded();
        let path =
            std::env::temp_dir().join(format!("traditional-review-{}.sock", uuid::Uuid::new_v4()));
        let mut wm = WindowManager::new_with_window_api(
            incoming,
            Some(path.clone()),
            Arc::new(crate::windows_api::FakeWindowApi::default()),
        )
        .unwrap();
        // The listener owns its handle; the test does not need to connect to the socket.
        let _ = std::fs::remove_file(path);
        wm.virtual_desktop_id = None;
        for idx in 0..2 {
            wm.monitors_mut().push_back(crate::monitor::new(
                idx,
                Rect::default(),
                Rect::default(),
                format!("Display{idx}"),
                format!("Device{idx}"),
                format!("Id{idx}"),
                None,
            ));
        }
        wm
    }

    fn add(workspace: &mut Workspace, hwnd: isize) {
        let mut container = Container::default();
        container.windows_mut().push_back(Window::from(hwnd));
        workspace.add_container_to_back(container);
    }

    struct RuleScope(Vec<crate::WorkspaceMatchingRule>);
    impl Drop for RuleScope {
        fn drop(&mut self) {
            *WORKSPACE_MATCHING_RULES.lock() = self.0.clone();
        }
    }

    fn rule(m: usize, d: usize, initial_only: bool) -> crate::WorkspaceMatchingRule {
        crate::WorkspaceMatchingRule {
            monitor_index: m,
            workspace_index: d,
            initial_only,
            matching_rule: crate::MatchingRule::Simple(crate::IdWithIdentifier {
                kind: crate::core::ApplicationIdentifier::Exe,
                id: "review-test.exe".into(),
                matching_strategy: Some(crate::MatchingStrategy::Equals),
            }),
        }
    }

    fn rule_api(fail_area_move: bool) -> crate::windows_api::FakeWindowApi {
        crate::windows_api::FakeWindowApi {
            identifiers: Some(crate::windows_api::WindowIdentifiers {
                exe: "review-test.exe".into(),
                ..Default::default()
            }),
            fail_area_move,
            ..Default::default()
        }
    }

    #[test]
    fn matching_rules_plan_one_move_using_last_eligible_destination() {
        let mut wm = manager();
        wm.window_api = Arc::new(rule_api(false));
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 42);
        let _restore = RuleScope(WORKSPACE_MATCHING_RULES.lock().clone());
        *WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, true), rule(1, 4, true)];
        let ops = wm.traditional_collect_rule_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(
            (ops[0].target_monitor_idx, ops[0].target_workspace_idx),
            (1, 4)
        );
        assert!(!wm.already_moved_window_handles.lock().contains(&42));
    }

    #[test]
    fn failed_initial_rule_can_retry_and_is_marked_only_after_success() {
        let mut wm = manager();
        wm.window_api = Arc::new(rule_api(true));
        wm.monitors_mut()[0].workspaces_mut()[0]
            .floating_windows_mut()
            .push_back(Window::from(42));
        let _restore = RuleScope(WORKSPACE_MATCHING_RULES.lock().clone());
        *WORKSPACE_MATCHING_RULES.lock() = vec![rule(9, 0, true)];
        assert!(wm.traditional_enforce_workspace_rules().is_err());
        assert!(!wm.already_moved_window_handles.lock().contains(&42));
        *WORKSPACE_MATCHING_RULES.lock() = vec![rule(1, 1, true)];
        assert!(wm.traditional_enforce_workspace_rules().is_err());
        assert!(!wm.already_moved_window_handles.lock().contains(&42));
        assert!(wm.monitors()[0].workspaces()[0].contains_window(42));
        // The retry finds the initial rule again. Already residing at its new
        // destination counts as a successful application without any Win32 call.
        *WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 0, true)];
        wm.traditional_enforce_workspace_rules().unwrap();
        assert!(wm.already_moved_window_handles.lock().contains(&42));
        assert!(wm.traditional_collect_rule_ops().unwrap().is_empty());
    }

    #[test]
    fn invalid_monitor_move_keeps_original_container_and_workspace() {
        let mut wm = manager();
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 42);
        let before = wm.monitors.clone();
        assert!(
            wm.traditional_move_container_to_monitor(9, None, false, None)
                .is_err()
        );
        assert_eq!(wm.monitors, before);
        assert!(wm.traditional_move_workspace_to_monitor(9).is_err());
        assert_eq!(wm.monitors, before);
    }

    #[test]
    fn restored_state_matches_display_identity_and_preserves_new_windows_once() {
        let mut wm = manager();
        wm.monitors_mut()[0].ensure_workspace_count(2);
        add(&mut wm.monitors_mut()[0].workspaces_mut()[1], 10);
        add(&mut wm.monitors_mut()[1].workspaces_mut()[0], 20);
        wm.monitors_mut()[0].focus_workspace(1).unwrap();
        let saved = State::from(&wm);
        // Startup discovers the saved windows and two newly opened windows.
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 30);
        // Only the new member of this startup stack survives deduplication.
        let stack = &mut wm.monitors_mut()[0].workspaces_mut()[0].containers_mut()[0];
        stack.windows_mut().push_front(Window::from(10));
        stack.focus_window(1);
        wm.monitors_mut()[1].ensure_workspace_count(3);
        add(&mut wm.monitors_mut()[1].workspaces_mut()[2], 40);
        wm.monitors.elements_mut().swap(0, 1);
        let restored = wm.traditional_restored_monitors(&saved).unwrap();
        assert_eq!(restored.focused_idx(), 1);
        assert!(restored.elements()[1].workspaces()[1].contains_window(10));
        assert_eq!(restored.elements()[1].focused_workspace_idx(), 1);
        assert!(restored.elements()[0].workspaces()[0].contains_window(20));
        assert!(restored.elements()[1].workspaces()[0].contains_window(30));
        assert!(restored.elements()[0].workspaces()[2].contains_window(40));
        let mut handles = restored
            .elements()
            .iter()
            .flat_map(|m| m.workspaces())
            .flat_map(WindowManager::workspace_hwnds)
            .collect::<Vec<_>>();
        handles.sort_unstable();
        assert_eq!(handles, vec![10, 20, 30, 40]);
        assert_eq!(
            restored.elements()[1].workspaces()[0].containers()[0]
                .focused_window()
                .map(|w| w.hwnd),
            Some(30)
        );
    }

    #[test]
    fn sending_to_background_workspace_preserves_destination_selection_and_foreground() {
        let mut wm = manager();
        let api = Arc::new(crate::windows_api::FakeWindowApi {
            foreground_hwnd: 42,
            ..Default::default()
        });
        wm.window_api = api.clone();
        wm.unmanaged_window_operation_behaviour = OperationBehaviour::Op;
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 42);
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 43);
        wm.monitors_mut()[1].ensure_workspace_count(2);
        // Real HWND operations may fail for these synthetic handles. The transfer
        // and final focus restoration must still be coherent on that error path.
        let _ = wm.traditional_move_container_to_monitor(1, Some(1), false, None);
        assert_eq!(wm.monitors()[1].focused_workspace_idx(), 0);
        assert!(wm.monitors()[1].workspaces()[1].contains_window(43));
        assert!(!wm.monitors()[0].workspaces()[0].contains_window(43));
        assert_eq!(wm.focused_monitor_idx(), 0);
        assert_eq!(api.effects.lock().last(), Some(&("focus", 42)));
    }

    #[test]
    fn rule_planning_keeps_moves_to_visible_target_and_validates_all_destinations() {
        let wm = manager();
        let moving = EnforceWorkspaceRuleOp {
            hwnd: 42,
            origin_monitor_idx: 1,
            origin_workspace_idx: 0,
            target_monitor_idx: 0,
            target_workspace_idx: 0,
            floating: false,
            initial_only: false,
        };
        let enforced = EnforceWorkspaceRuleOp {
            hwnd: 43,
            origin_monitor_idx: 0,
            ..moving
        };
        let planned = wm
            .traditional_prepare_rule_ops(vec![moving, enforced])
            .unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].hwnd, 42);
        assert!(
            wm.traditional_prepare_rule_ops(vec![
                moving,
                EnforceWorkspaceRuleOp {
                    target_monitor_idx: 9,
                    ..moving
                }
            ])
            .is_err()
        );
    }

    #[test]
    fn moved_foreground_on_hidden_workspace_is_not_refocused() {
        let mut wm = manager();
        add(&mut wm.monitors_mut()[0].workspaces_mut()[0], 42);
        wm.monitors_mut()[1].ensure_workspace_count(2);
        add(&mut wm.monitors_mut()[1].workspaces_mut()[1], 43);
        assert_eq!(
            wm.traditional_foreground_to_restore(Some(43))
                .map(|w| w.hwnd),
            Some(42)
        );
        assert_eq!(
            wm.traditional_foreground_to_restore(Some(99))
                .map(|w| w.hwnd),
            Some(99)
        );
    }

    #[test]
    fn restored_state_rejects_replaced_display_before_mutation() {
        let mut wm = manager();
        let saved = State::from(&wm);
        wm.monitors_mut()[1].device_id = "replacement".into();
        let before = wm.monitors.clone();
        assert!(wm.traditional_restored_monitors(&saved).is_err());
        assert_eq!(wm.monitors, before);
    }
}
