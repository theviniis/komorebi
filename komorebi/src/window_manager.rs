use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::net::Shutdown;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use color_eyre::eyre;
use color_eyre::eyre::OptionExt;
use color_eyre::eyre::bail;
use crossbeam_channel::Receiver;
use hotwatch::EventKind;
use hotwatch::Hotwatch;
use hotwatch::notify::ErrorKind as NotifyErrorKind;
use parking_lot::Mutex;
use uds_windows::UnixListener;
use uds_windows::UnixStream;

use crate::animation::ANIMATION_ENABLED_GLOBAL;
use crate::animation::ANIMATION_ENABLED_PER_ANIMATION;
use crate::animation::AnimationEngine;
use crate::core::Arrangement;
use crate::core::Axis;
use crate::core::BorderImplementation;
use crate::core::CustomLayout;
use crate::core::CycleDirection;
use crate::core::DefaultLayout;
use crate::core::FocusFollowsMouseImplementation;
use crate::core::Layout;
use crate::core::MonocleFocusBehaviour;
use crate::core::MoveBehaviour;
use crate::core::OperationBehaviour;
use crate::core::OperationDirection;
use crate::core::Rect;
use crate::core::Sizing;
use crate::core::WindowContainerBehaviour;
use crate::core::WindowManagementBehaviour;

use crate::CrossBoundaryBehaviour;
use crate::DATA_DIR;
use crate::HOME_DIR;
use crate::NO_TITLEBAR;
use crate::REGEX_IDENTIFIERS;
use crate::SUBSCRIPTION_SOCKETS;
use crate::WORKSPACE_MATCHING_RULES;
use crate::border_manager;
use crate::border_manager::BORDER_OFFSET;
use crate::border_manager::BORDER_WIDTH;
use crate::container::Container;
use crate::load_configuration;
use crate::monitor::Monitor;
use crate::ring::Ring;
use crate::should_act;
use crate::state::State;
use crate::static_config::StaticConfig;
use crate::transparency_manager;
use crate::virtual_desktop;
use crate::window::Window;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::winevent_listener;
use crate::workspace::Workspace;
use crate::workspace::WorkspaceLayer;

#[derive(Debug)]
pub struct WindowManager {
    pub window_api: Arc<dyn crate::windows_api::WindowApi>,
    pub monitors: Ring<Monitor>,
    pub monitor_usr_idx_map: HashMap<usize, usize>,
    pub incoming_events: Receiver<WindowManagerEvent>,
    pub command_listener: UnixListener,
    pub is_paused: bool,
    pub work_area_offset: Option<Rect>,
    pub resize_delta: i32,
    pub window_management_behaviour: WindowManagementBehaviour,
    pub cross_monitor_move_behaviour: MoveBehaviour,
    pub cross_boundary_behaviour: CrossBoundaryBehaviour,
    pub monocle_focus_behaviour: MonocleFocusBehaviour,
    pub unmanaged_window_operation_behaviour: OperationBehaviour,
    pub focus_follows_mouse: Option<FocusFollowsMouseImplementation>,
    pub mouse_follows_focus: bool,
    pub hotwatch: Hotwatch,
    pub has_pending_raise_op: bool,
    pub pending_move_op: Arc<Option<(usize, usize, isize)>>,
    pub already_moved_window_handles: Arc<Mutex<HashSet<isize>>>,
    pub known_hwnds: HashMap<isize, (usize, usize)>,
    /// Track which native desktop each window belongs to.
    /// Cache derived from managed workspaces; native window events reconcile it.
    pub window_desktops: HashMap<isize, usize>,
    /// The index of the currently focused native desktop (global across all monitors).
    ///
    /// Invariant: every monitor has `workspaces().len() == desktop_count`, and
    /// every monitor's `focused_workspace_idx()` equals `focused_desktop_idx`.
    pub focused_desktop_idx: usize,
    /// The total number of native virtual desktops, as reported by the desktop backend.
    ///
    /// Invariant: every monitor has `workspaces().len() == desktop_count`, and
    /// every monitor's `focused_workspace_idx()` equals `focused_desktop_idx`.
    pub desktop_count: usize,
    /// Ordered native identities corresponding to every monitor workspace ring.
    pub desktop_ids: Vec<virtual_desktop::DesktopId>,
    /// Global workspace names (synced with native desktop names).
    /// Key is the workspace/desktop index, value is the name.
    pub workspace_names: HashMap<usize, String>,
}

impl AsRef<Self> for WindowManager {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl_ring_elements!(WindowManager, Monitor);

impl WindowManager {
    #[tracing::instrument]
    pub fn new(
        incoming: Receiver<WindowManagerEvent>,
        custom_socket_path: Option<PathBuf>,
    ) -> eyre::Result<Self> {
        Self::new_with_window_api(
            incoming,
            custom_socket_path,
            Arc::new(crate::windows_api::RealWindowsApi),
        )
    }

    pub fn new_with_window_api(
        incoming: Receiver<WindowManagerEvent>,
        custom_socket_path: Option<PathBuf>,
        window_api: Arc<dyn crate::windows_api::WindowApi>,
    ) -> eyre::Result<Self> {
        let socket = custom_socket_path.unwrap_or_else(|| DATA_DIR.join("komorebi.sock"));

        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) => match error.kind() {
                // Doing this because ::exists() doesn't work reliably on Windows via IntelliJ
                ErrorKind::NotFound => {}
                _ => {
                    return Err(error.into());
                }
            },
        };

        let listener = UnixListener::bind(&socket)?;

        Ok(Self {
            window_api,
            monitors: Ring::default(),
            monitor_usr_idx_map: HashMap::new(),
            incoming_events: incoming,
            command_listener: listener,
            is_paused: false,
            work_area_offset: None,
            window_management_behaviour: WindowManagementBehaviour::default(),
            cross_monitor_move_behaviour: MoveBehaviour::Swap,
            cross_boundary_behaviour: CrossBoundaryBehaviour::Monitor,
            monocle_focus_behaviour: MonocleFocusBehaviour::default(),
            unmanaged_window_operation_behaviour: OperationBehaviour::Op,
            resize_delta: 50,
            focus_follows_mouse: None,
            mouse_follows_focus: true,
            hotwatch: Hotwatch::new()?,
            has_pending_raise_op: false,
            pending_move_op: Arc::new(None),
            already_moved_window_handles: Arc::new(Mutex::new(HashSet::new())),
            known_hwnds: HashMap::new(),
            window_desktops: HashMap::new(),
            focused_desktop_idx: 0,
            desktop_count: 1,
            desktop_ids: Vec::new(),
            workspace_names: HashMap::new(),
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn init(&mut self) -> eyre::Result<()> {
        tracing::info!("initialising");
        WindowsApi::load_monitor_information(self)?;
        self.sync_desktop_count()?;
        self.discover_windows()
    }

    pub(crate) fn discover_windows(&mut self) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        for hwnd in self.window_api.enum_window_handles()? {
            self.process_event(WindowManagerEvent::Manage(Window::from(hwnd)))?;
        }
        self.update_known_hwnds();
        Ok(())
    }

    #[tracing::instrument(skip(self, state))]
    pub fn apply_state(&mut self, state: State) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        if state.monitors.elements().len() != self.monitors().len() {
            bail!("saved state monitor count differs from connected monitors");
        }
        let snapshot = virtual_desktop::backend().snapshot()?;
        let positions = snapshot
            .desktops
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id, i))
            .collect::<HashMap<_, _>>();
        let mut resize_by_id = HashMap::new();
        for monitors in [&self.monitors, &state.monitors] {
            for monitor in monitors.elements() {
                for workspace in monitor.workspaces() {
                    for (i, container) in workspace.containers().iter().enumerate() {
                        resize_by_id.insert(
                            container.id.clone(),
                            workspace.resize_dimensions.get(i).copied().flatten(),
                        );
                    }
                }
            }
        }
        let saved_indices = self
            .monitors()
            .iter()
            .enumerate()
            .map(|(i, current)| {
                let candidates = state
                    .monitors
                    .elements()
                    .iter()
                    .enumerate()
                    .filter(|(_, saved)| crate::monitor_reconciliator::same_device(current, saved))
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                if candidates.len() == 1 {
                    candidates[0]
                } else {
                    i
                }
            })
            .collect::<Vec<_>>();
        let mut restored = self.monitors.clone();
        // Restore layouts only where the saved slot/identity still exists. Current
        // configuration padding/options and the native names/IDs remain authoritative.
        for (m, monitor) in restored.elements_mut().iter_mut().enumerate() {
            let saved = &state.monitors.elements()[saved_indices[m]];
            for (old_idx, source) in saved.workspaces().iter().enumerate() {
                let target_idx = source
                    .native_desktop_id
                    .and_then(|id| positions.get(&id).copied())
                    .or_else(|| {
                        source
                            .native_desktop_id
                            .is_none()
                            .then_some(old_idx)
                            .filter(|i| *i < snapshot.desktops.len())
                    });
                if let Some(d) = target_idx {
                    let target = &mut monitor.workspaces_mut()[d];
                    let (padding, workspace_padding, options, id, name) = (
                        target.container_padding,
                        target.workspace_padding,
                        target.layout_options,
                        target.native_desktop_id,
                        target.name.clone(),
                    );
                    let config = target.workspace_config.clone();
                    let defaults = target.layout_defaults_cache.clone();
                    *target = source.clone();
                    if config.is_some() {
                        target.workspace_config = config;
                    }
                    target.layout_defaults_cache = defaults;
                    target.container_padding = padding;
                    target.workspace_padding = workspace_padding;
                    target.layout_options = options;
                    target.native_desktop_id = id;
                    target.name = name;
                }
            }
            for workspace in monitor.workspaces_mut() {
                workspace.containers_mut().clear();
                workspace.floating_windows_mut().clear();
                workspace.monocle_container = None;
                workspace.maximized_window = None;
                workspace.monocle_container_restore_idx = None;
                workspace.maximized_window_restore_idx = None;
                workspace.resize_dimensions.clear();
                workspace.preselected_container_idx = None;
            }
        }
        let mut destinations = HashMap::new();
        // Query all native destinations before replacing anything. Include newly
        // discovered windows absent from the saved state, so restoration cannot lose them.
        for source_monitors in [&state.monitors, &self.monitors] {
            for (m, monitor) in source_monitors.elements().iter().enumerate() {
                for workspace in monitor.workspaces() {
                    for hwnd in Self::workspace_hwnds(workspace) {
                        if !self.window_api.is_window(hwnd) {
                            continue;
                        }
                        let id = virtual_desktop::backend().desktop_id_for_window(hwnd)?;
                        let d = *positions
                            .get(&id)
                            .ok_or_eyre("saved window desktop is outside the native snapshot")?;
                        let hmonitor = self.window_api.monitor_from_window(hwnd);
                        let target_m = self
                            .monitors()
                            .iter()
                            .position(|mon| mon.id == hmonitor)
                            .unwrap_or(m);
                        destinations.insert(hwnd, (target_m, d));
                    }
                }
            }
        }
        if virtual_desktop::backend().snapshot()? != snapshot {
            bail!("native desktop list changed during state restoration; retry with current state");
        }
        let mut seen = HashSet::new();
        let mut container_ids = HashSet::new();
        for source_monitors in [&state.monitors, &self.monitors] {
            for monitor in source_monitors.elements() {
                for source in monitor.workspaces() {
                    for (container, monocle) in source
                        .containers()
                        .iter()
                        .map(|c| (c, false))
                        .chain(source.monocle_container.iter().map(|c| (c, true)))
                    {
                        let selected = container.focused_window().map(|w| w.hwnd);
                        let mut groups: HashMap<(usize, usize), Container> = HashMap::new();
                        for &window in container.windows() {
                            if let Some(&pair) = destinations.get(&window.hwnd)
                                && seen.insert(window.hwnd)
                            {
                                let group = groups.entry(pair).or_default();
                                group.locked = container.locked;
                                group.add_window_without_visibility_changes(window);
                            }
                        }
                        let single = groups.len() == 1;
                        for ((m, d), mut group) in groups {
                            if single && container_ids.insert(container.id.clone()) {
                                group.id = container.id.clone();
                            }
                            if let Some(i) = selected.and_then(|hwnd| group.idx_for_window(hwnd)) {
                                group.focus_window(i);
                            }
                            let target = &mut restored.elements_mut()[m].workspaces_mut()[d];
                            if monocle && target.monocle_container.is_none() {
                                target.monocle_container = Some(group);
                                target.monocle_container_restore_idx = Some(
                                    source
                                        .monocle_container_restore_idx
                                        .unwrap_or(0)
                                        .min(target.containers().len()),
                                );
                            } else {
                                Self::adopt_desktop_container(target, group);
                            }
                        }
                    }
                    for &window in source.floating_windows() {
                        if let Some(&(m, d)) = destinations.get(&window.hwnd)
                            && seen.insert(window.hwnd)
                        {
                            restored.elements_mut()[m].workspaces_mut()[d]
                                .floating_windows_mut()
                                .push_back(window);
                        }
                    }
                    if let Some(window) = source.maximized_window
                        && let Some(&(m, d)) = destinations.get(&window.hwnd)
                        && seen.insert(window.hwnd)
                    {
                        let target = &mut restored.elements_mut()[m].workspaces_mut()[d];
                        if target.maximized_window.is_none() {
                            target.maximized_window = Some(window);
                            target.maximized_window_restore_idx = Some(
                                source
                                    .maximized_window_restore_idx
                                    .unwrap_or(0)
                                    .min(target.containers().len()),
                            );
                        } else {
                            target.floating_windows_mut().push_back(window);
                        }
                    }
                }
            }
        }
        for monitor in restored.elements_mut() {
            for workspace in monitor.workspaces_mut() {
                workspace.resize_dimensions = workspace
                    .containers()
                    .iter()
                    .map(|c| resize_by_id.get(&c.id).copied().flatten())
                    .collect();
            }
        }
        self.monitors = restored;
        self.sync_desktop_count()?;
        let api = self.window_api.clone();
        let focused = self.focused_desktop_idx;
        for m in 0..self.monitors().len() {
            for container in self.monitors_mut()[m].workspaces_mut()[focused].containers_mut() {
                if container.windows().len() > 1 {
                    api.refresh_stack(container);
                }
            }
            for d in 0..self.desktop_count {
                self.update_desktop_workspace(m, d)?;
            }
        }
        self.update_known_hwnds();
        Ok(())
    }

    pub(crate) fn preserve_windows_after_monitor_change(
        &mut self,
        previous: &State,
    ) -> eyre::Result<()> {
        let mut combined = State::from(self.as_ref());
        let mut seen = combined
            .monitors
            .elements()
            .iter()
            .flat_map(|monitor| monitor.workspaces().iter())
            .flat_map(Self::workspace_hwnds)
            .collect::<HashSet<_>>();
        let target = combined
            .monitors
            .elements_mut()
            .front_mut()
            .and_then(|m| m.workspaces_mut().front_mut())
            .ok_or_eyre("no attached monitor available for native windows")?;
        for monitor in previous.monitors.elements() {
            for workspace in monitor.workspaces() {
                for container in workspace
                    .containers()
                    .iter()
                    .chain(workspace.monocle_container.iter())
                {
                    let mut group = container.clone();
                    group.windows_mut().retain(|w| seen.insert(w.hwnd));
                    if !group.windows().is_empty() {
                        target.add_container_to_back(group);
                    }
                }
                for &window in workspace
                    .floating_windows()
                    .iter()
                    .chain(workspace.maximized_window.iter())
                {
                    if seen.insert(window.hwnd) {
                        target.floating_windows_mut().push_back(window);
                    }
                }
            }
        }
        self.apply_state(combined)
    }

    pub(crate) fn focus_managed_window(&mut self, hwnd: isize) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        let (m, d) = self
            .managed_window_location(hwnd)
            .ok_or_eyre("window is not managed")?;
        if d != self.focused_desktop_idx {
            self.focus_desktop(d)?;
        }
        self.focus_monitor(m)?;
        let api = self.window_api.clone();
        let workspace = &mut self.monitors_mut()[m].workspaces_mut()[d];
        if let Some(i) = workspace
            .floating_windows()
            .iter()
            .position(|w| w.hwnd == hwnd)
        {
            workspace.floating_windows.focus(i);
            workspace.layer = WorkspaceLayer::Floating;
        } else if let Some(container) = &mut workspace.monocle_container
            && let Some(i) = container.idx_for_window(hwnd)
        {
            container.focus_window(i);
            api.refresh_stack(container);
        } else if workspace.container_idx_for_window(hwnd).is_some() {
            if workspace.monocle_container.is_some() {
                workspace.reintegrate_monocle_container()?;
            }
            let i = workspace
                .container_idx_for_window(hwnd)
                .ok_or_eyre("window container disappeared")?;
            workspace.focus_container(i);
            let container = &mut workspace.containers_mut()[i];
            let w = container
                .idx_for_window(hwnd)
                .ok_or_eyre("window disappeared")?;
            container.focus_window(w);
            if container.windows().len() > 1 {
                api.refresh_stack(container);
            }
            workspace.layer = WorkspaceLayer::Tiling;
        }
        api.focus_window(Window::from(hwnd), self.mouse_follows_focus)?;
        self.update_desktop_workspace(m, d)
    }

    #[tracing::instrument]
    pub fn reload_configuration() {
        tracing::info!("reloading configuration");
        std::thread::spawn(|| load_configuration().expect("could not load configuration"));
    }

    #[tracing::instrument(skip(self))]
    pub fn reload_static_configuration(&mut self, pathbuf: &PathBuf) -> eyre::Result<()> {
        tracing::info!("reloading static configuration");
        StaticConfig::reload(pathbuf, self)
    }

    pub fn window_management_behaviour(
        &self,
        monitor_idx: usize,
        workspace_idx: usize,
    ) -> WindowManagementBehaviour {
        if let Some(monitor) = self.monitors().get(monitor_idx)
            && let Some(workspace) = monitor.workspaces().get(workspace_idx)
        {
            let current_behaviour = if let Some(behaviour) = workspace.window_container_behaviour {
                if workspace.containers().is_empty()
                    && matches!(behaviour, WindowContainerBehaviour::Append)
                {
                    // You can't append to an empty workspace
                    WindowContainerBehaviour::Create
                } else {
                    behaviour
                }
            } else if workspace.containers().is_empty()
                && matches!(
                    self.window_management_behaviour.current_behaviour,
                    WindowContainerBehaviour::Append
                )
            {
                // You can't append to an empty workspace
                WindowContainerBehaviour::Create
            } else {
                self.window_management_behaviour.current_behaviour
            };

            let float_override = if let Some(float_override) = workspace.float_override {
                float_override
            } else {
                self.window_management_behaviour.float_override
            };

            let floating_layer_behaviour =
                if let Some(behaviour) = workspace.floating_layer_behaviour {
                    behaviour
                } else {
                    monitor
                        .floating_layer_behaviour
                        .unwrap_or(self.window_management_behaviour.floating_layer_behaviour)
                };

            // If the workspace layer is `Floating` and the floating layer behaviour should
            // float then change floating_layer_override to true so that new windows spawn
            // as floating
            let floating_layer_override = matches!(workspace.layer, WorkspaceLayer::Floating)
                && floating_layer_behaviour.should_float();

            return WindowManagementBehaviour {
                current_behaviour,
                float_override,
                floating_layer_override,
                floating_layer_behaviour,
                toggle_float_placement: self.window_management_behaviour.toggle_float_placement,
                floating_layer_placement: self.window_management_behaviour.floating_layer_placement,
                float_override_placement: self.window_management_behaviour.float_override_placement,
                float_rule_placement: self.window_management_behaviour.float_rule_placement,
            };
        }

        WindowManagementBehaviour {
            current_behaviour: WindowContainerBehaviour::Create,
            float_override: self.window_management_behaviour.float_override,
            floating_layer_override: self.window_management_behaviour.floating_layer_override,
            floating_layer_behaviour: self.window_management_behaviour.floating_layer_behaviour,
            toggle_float_placement: self.window_management_behaviour.toggle_float_placement,
            floating_layer_placement: self.window_management_behaviour.floating_layer_placement,
            float_override_placement: self.window_management_behaviour.float_override_placement,
            float_rule_placement: self.window_management_behaviour.float_rule_placement,
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn watch_configuration(&mut self, enable: bool) -> eyre::Result<()> {
        let config_pwsh = HOME_DIR.join("komorebi.ps1");
        let config_ahk = HOME_DIR.join("komorebi.ahk");

        if config_pwsh.exists() {
            self.configure_watcher(enable, config_pwsh)?;
        } else if config_ahk.exists() {
            self.configure_watcher(enable, config_ahk)?;
        }

        Ok(())
    }

    fn configure_watcher(&mut self, enable: bool, config: PathBuf) -> eyre::Result<()> {
        if enable {
            tracing::info!("watching configuration for changes: {}", config.display());
            // Always make absolutely sure that there isn't an already existing watch, because
            // hotwatch allows multiple watches to be registered for the same path
            match self.hotwatch.unwatch(&config) {
                Ok(()) => {}
                Err(error) => match error {
                    hotwatch::Error::Notify(ref notify_error) => match notify_error.kind {
                        NotifyErrorKind::WatchNotFound => {}
                        _ => return Err(error.into()),
                    },
                    error @ hotwatch::Error::Io(_) => return Err(error.into()),
                },
            }

            self.hotwatch.watch(config, |event| match event.kind {
                // Editing in Notepad sends a NoticeWrite while editing in (Neo)Vim sends
                // a NoticeRemove, presumably because of the use of swap files?
                EventKind::Modify(_) | EventKind::Remove(_) => {
                    std::thread::spawn(|| {
                        load_configuration().expect("could not load configuration");
                    });
                }
                _ => {}
            })?;
        } else {
            tracing::info!(
                "no longer watching configuration for changes: {}",
                config.display()
            );

            self.hotwatch.unwatch(config)?;
        };

        Ok(())
    }

    pub fn monitor_idx_in_direction(&self, direction: OperationDirection) -> Option<usize> {
        let current_monitor_size = self.focused_monitor_size().ok()?;

        for (idx, monitor) in self.monitors.elements().iter().enumerate() {
            match direction {
                OperationDirection::Left => {
                    if monitor.size.left + monitor.size.right == current_monitor_size.left {
                        return Option::from(idx);
                    }
                }
                OperationDirection::Right => {
                    if current_monitor_size.right + current_monitor_size.left == monitor.size.left {
                        return Option::from(idx);
                    }
                }
                OperationDirection::Up => {
                    if monitor.size.top + monitor.size.bottom == current_monitor_size.top {
                        return Option::from(idx);
                    }
                }
                OperationDirection::Down => {
                    if current_monitor_size.top + current_monitor_size.bottom == monitor.size.top {
                        return Option::from(idx);
                    }
                }
            }
        }

        None
    }

    /// Calculates the direction of a move across monitors given a specific monitor index
    pub fn direction_from_monitor_idx(
        &self,
        target_monitor_idx: usize,
    ) -> Option<OperationDirection> {
        let current_monitor_idx = self.focused_monitor_idx();
        if current_monitor_idx == target_monitor_idx {
            return None;
        }

        let current_monitor_size = self.focused_monitor_size().ok()?;
        let target_monitor_size = self.monitors().get(target_monitor_idx)?.size;

        if target_monitor_size.left + target_monitor_size.right == current_monitor_size.left {
            return Some(OperationDirection::Left);
        }
        if current_monitor_size.right + current_monitor_size.left == target_monitor_size.left {
            return Some(OperationDirection::Right);
        }
        if target_monitor_size.top + target_monitor_size.bottom == current_monitor_size.top {
            return Some(OperationDirection::Up);
        }
        if current_monitor_size.top + current_monitor_size.bottom == target_monitor_size.top {
            return Some(OperationDirection::Down);
        }

        None
    }

    /// Locate the managed association even when a preceding operation has not rebuilt caches yet.
    pub(crate) fn managed_window_location(&self, hwnd: isize) -> Option<(usize, usize)> {
        self.monitors().iter().enumerate().find_map(|(m, monitor)| {
            monitor
                .workspaces()
                .iter()
                .position(|ws| ws.contains_window(hwnd))
                .map(|d| (m, d))
        })
    }

    pub(crate) fn validate_window_destination(
        &self,
        monitor_idx: usize,
        desktop_idx: usize,
    ) -> eyre::Result<()> {
        if desktop_idx >= self.desktop_count {
            bail!(
                "workspace rule desktop {desktop_idx} is out of range (desktop_count: {})",
                self.desktop_count
            );
        }
        self.monitors()
            .get(monitor_idx)
            .and_then(|m| m.workspaces().get(desktop_idx))
            .ok_or_eyre("workspace rule destination monitor/workspace does not exist")?;
        Ok(())
    }

    /// Last matching eligible rule wins. Mark initial-only rules only after success.
    pub(crate) fn window_rule_destination(
        &self,
        window: Window,
    ) -> eyre::Result<Option<(usize, usize, bool)>> {
        let rules = WORKSPACE_MATCHING_RULES.lock().clone();
        if rules.is_empty() {
            return Ok(None);
        }
        let Ok(ids) = self.window_api.identifiers(window) else {
            return Ok(None);
        };
        let already_applied = self
            .already_moved_window_handles
            .lock()
            .contains(&window.hwnd);
        let regex = REGEX_IDENTIFIERS.lock();
        let rule = rules.iter().rev().find(|rule| {
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
        });
        let destination =
            rule.map(|rule| (rule.monitor_index, rule.workspace_index, rule.initial_only));
        drop(regex);
        if let Some((m, d, _)) = destination {
            self.validate_window_destination(m, d)?;
        }
        Ok(destination)
    }

    pub(crate) fn apply_window_workspace_rule(&mut self, window: Window) -> eyre::Result<()> {
        let Some((target_m, target_d, initial_only)) = self.window_rule_destination(window)? else {
            return Ok(());
        };
        let Some((origin_m, origin_d)) = self.managed_window_location(window.hwnd) else {
            return Ok(());
        };
        if (origin_m, origin_d) != (target_m, target_d) {
            if origin_d != target_d {
                if let Err(error) = virtual_desktop::backend().move_window_to_desktop_id(
                    window.hwnd,
                    *self
                        .desktop_ids
                        .get(target_d)
                        .ok_or_eyre("native identities are not synchronized")?,
                ) {
                    let reconciliation = self.reconcile_window_desktop(window.hwnd);
                    self.update_known_hwnds();
                    bail!(
                        "workspace rule failed moving window {}: {error}; reconciliation: {reconciliation:?}",
                        window.hwnd
                    );
                }
            }
            let floating = self.monitors()[origin_m].workspaces()[origin_d]
                .floating_windows()
                .iter()
                .any(|w| w.hwnd == window.hwnd);
            if floating && origin_m != target_m {
                let origin_area = self.monitors()[origin_m].work_area_size;
                let target_area = self.monitors()[target_m].work_area_size;
                if let Err(error) =
                    self.window_api
                        .move_window_to_area(window, &origin_area, &target_area)
                {
                    // The desktop may already have changed. Keep the actual monitor until
                    // geometry succeeds, and reconcile the native desktop before returning.
                    let reconciliation = self.reconcile_window_desktop(window.hwnd);
                    self.update_known_hwnds();
                    bail!(
                        "workspace rule failed changing monitor: {error}; reconciliation: {reconciliation:?}"
                    );
                }
            }
            let (detached, floating) = self.monitors_mut()[origin_m].workspaces_mut()[origin_d]
                .detach_window_for_desktop_move(window.hwnd)
                .ok_or_eyre("workspace rule source window disappeared")?;
            let behaviour = self.window_management_behaviour(target_m, target_d);
            let window_api = self.window_api.clone();
            let target = &mut self.monitors_mut()[target_m].workspaces_mut()[target_d];
            if floating {
                target.floating_windows_mut().push_back(detached);
            } else if matches!(
                behaviour.current_behaviour,
                WindowContainerBehaviour::Append
            ) && let Some(container) = target.focused_container_mut()
            {
                window_api.append_to_stack(container, detached);
            } else {
                target.new_container_for_window(detached);
            }
            self.update_known_hwnds();
            self.update_desktop_workspace(origin_m, origin_d)?;
            self.update_desktop_workspace(target_m, target_d)?;
        }
        if initial_only {
            self.already_moved_window_handles.lock().insert(window.hwnd);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), level = "debug")]
    pub fn enforce_workspace_rules(&mut self) -> eyre::Result<()> {
        if WORKSPACE_MATCHING_RULES.lock().is_empty() {
            return Ok(());
        }
        self.sync_desktop_count()?;
        self.update_known_hwnds();
        let mut hwnds = self.known_hwnds.keys().copied().collect::<Vec<_>>();
        hwnds.sort_unstable();
        for hwnd in hwnds {
            self.apply_window_workspace_rule(Window::from(hwnd))?;
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn retile_all(&mut self, preserve_resize_dimensions: bool) -> eyre::Result<()> {
        let offset = self.work_area_offset;

        for monitor in self.monitors_mut() {
            let offset = if monitor.work_area_offset.is_some() {
                monitor.work_area_offset
            } else {
                offset
            };

            let focused_workspace_idx = monitor.focused_workspace_idx();
            monitor.update_workspace_globals(focused_workspace_idx, offset);

            let hmonitor = monitor.id;
            let monitor_wp = monitor.wallpaper.clone();
            let workspace = monitor
                .focused_workspace_mut()
                .ok_or_eyre("there is no workspace")?;

            // Reset any resize adjustments if we want to force a retile
            if !preserve_resize_dimensions {
                workspace.resize_dimensions.fill(None);
            }

            if (workspace.wallpaper.is_some() || monitor_wp.is_some())
                && let Err(error) = workspace.apply_wallpaper(hmonitor, &monitor_wp)
            {
                tracing::error!("failed to apply wallpaper: {}", error);
            }

            workspace.update()?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn manage_focused_window(&mut self) -> eyre::Result<()> {
        let hwnd = WindowsApi::foreground_window()?;
        let event = WindowManagerEvent::Manage(Window::from(hwnd));
        Ok(winevent_listener::event_tx().send(event)?)
    }

    #[tracing::instrument(skip(self))]
    pub fn unmanage_focused_window(&mut self) -> eyre::Result<()> {
        let hwnd = WindowsApi::foreground_window()?;
        let event = WindowManagerEvent::Unmanage(Window::from(hwnd));
        Ok(winevent_listener::event_tx().send(event)?)
    }

    #[tracing::instrument(skip(self))]
    pub fn raise_window_at_cursor_pos(&mut self) -> eyre::Result<()> {
        let mut hwnd = None;

        let workspace = self.focused_workspace()?;
        // first check the focused workspace
        if let Some(container_idx) = workspace.container_idx_from_current_point()
            && let Some(container) = workspace.containers().get(container_idx)
            && let Some(window) = container.focused_window()
        {
            hwnd = Some(window.hwnd);
        }

        // then check all workspaces
        if hwnd.is_none() {
            for monitor in self.monitors() {
                for ws in monitor.workspaces() {
                    if let Some(container_idx) = ws.container_idx_from_current_point()
                        && let Some(container) = ws.containers().get(container_idx)
                        && let Some(window) = container.focused_window()
                    {
                        hwnd = Some(window.hwnd);
                    }
                }
            }
        }

        // finally try matching the other way using a hwnd returned from the cursor pos
        if hwnd.is_none() {
            let cursor_pos_hwnd = WindowsApi::window_at_cursor_pos()?;

            for monitor in self.monitors() {
                for ws in monitor.workspaces() {
                    if ws.container_for_window(cursor_pos_hwnd).is_some() {
                        hwnd = Some(cursor_pos_hwnd);
                    }
                }
            }
        }

        if let Some(hwnd) = hwnd {
            if self.has_pending_raise_op
                    || self.focused_window()?.hwnd == hwnd
                    // Sometimes we need this check, because the focus may have been given by a click
                    // to a non-window such as the taskbar or system tray, and komorebi doesn't know that
                    // the focused window of the workspace is not actually focused by the OS at that point
                    || WindowsApi::foreground_window()? == hwnd
            {
                return Ok(());
            }

            let event = WindowManagerEvent::Raise(Window::from(hwnd));
            self.has_pending_raise_op = true;
            winevent_listener::event_tx().send(event)?;
        } else {
            tracing::debug!(
                "not raising unknown window: {}",
                Window::from(WindowsApi::window_at_cursor_pos()?)
            );
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn transfer_window(
        &mut self,
        origin: (usize, usize, isize),
        target: (usize, usize, usize),
    ) -> eyre::Result<()> {
        let (origin_monitor_idx, origin_workspace_idx, w_hwnd) = origin;
        let (target_monitor_idx, target_workspace_idx, target_container_idx) = target;

        if origin_workspace_idx != target_workspace_idx {
            bail!("cross-desktop transfers must use native desktop movement");
        }
        self.validate_window_destination(origin_monitor_idx, origin_workspace_idx)?;
        self.validate_window_destination(target_monitor_idx, target_workspace_idx)?;

        let origin_workspace = self
            .monitors_mut()
            .get_mut(origin_monitor_idx)
            .ok_or_eyre("cannot get monitor idx")?
            .workspaces_mut()
            .get_mut(origin_workspace_idx)
            .ok_or_eyre("cannot get workspace idx")?;

        let origin_container_idx = origin_workspace
            .container_for_window(w_hwnd)
            .and_then(|c| origin_workspace.containers().iter().position(|cc| cc == c));

        if let Some(origin_container_idx) = origin_container_idx {
            // Moving normal container window
            self.transfer_container(
                (
                    origin_monitor_idx,
                    origin_workspace_idx,
                    origin_container_idx,
                ),
                (
                    target_monitor_idx,
                    target_workspace_idx,
                    target_container_idx,
                ),
            )?;
        } else if let Some(idx) = origin_workspace
            .floating_windows()
            .iter()
            .position(|w| w.hwnd == w_hwnd)
        {
            // Moving floating window
            // There is no need to physically move the floating window between areas with
            // `move_to_area` because the user already did that, so we only need to transfer the
            // window to the target `floating_windows`
            if let Some(floating_window) = origin_workspace.floating_windows_mut().remove(idx) {
                let target_workspace = self
                    .monitors_mut()
                    .get_mut(target_monitor_idx)
                    .ok_or_eyre("there is no monitor at this idx")?
                    .focused_workspace_mut()
                    .ok_or_eyre("there is no focused workspace for this monitor")?;

                target_workspace
                    .floating_windows_mut()
                    .push_back(floating_window);
            }
        } else if origin_workspace
            .monocle_container
            .as_ref()
            .and_then(|monocle| monocle.focused_window().map(|w| w.hwnd == w_hwnd))
            .unwrap_or_default()
        {
            // Moving monocle container
            if let Some(monocle_idx) = origin_workspace.monocle_container_restore_idx {
                let origin_workspace = self
                    .monitors_mut()
                    .get_mut(origin_monitor_idx)
                    .ok_or_eyre("there is no monitor at this idx")?
                    .workspaces_mut()
                    .get_mut(origin_workspace_idx)
                    .ok_or_eyre("there is no workspace for this monitor")?;
                for container in origin_workspace.containers_mut() {
                    container.restore();
                }
                origin_workspace.reintegrate_monocle_container()?;

                self.transfer_container(
                    (origin_monitor_idx, origin_workspace_idx, monocle_idx),
                    (
                        target_monitor_idx,
                        target_workspace_idx,
                        target_container_idx,
                    ),
                )?;
            }
        } else if origin_workspace
            .maximized_window
            .as_ref()
            .map(|max| max.hwnd == w_hwnd)
            .unwrap_or_default()
        {
            // Moving maximized_window
            if let Some(maximized_idx) = origin_workspace.maximized_window_restore_idx {
                self.focus_monitor(origin_monitor_idx)?;
                let origin_monitor = self
                    .focused_monitor_mut()
                    .ok_or_eyre("there is no origin monitor")?;
                origin_monitor.focus_workspace(origin_workspace_idx)?;
                self.unmaximize_window()?;
                self.focus_monitor(target_monitor_idx)?;
                let target_monitor = self
                    .focused_monitor_mut()
                    .ok_or_eyre("there is no target monitor")?;
                target_monitor.focus_workspace(target_workspace_idx)?;

                self.transfer_container(
                    (origin_monitor_idx, origin_workspace_idx, maximized_idx),
                    (
                        target_monitor_idx,
                        target_workspace_idx,
                        target_container_idx,
                    ),
                )?;
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn transfer_container(
        &mut self,
        origin: (usize, usize, usize),
        target: (usize, usize, usize),
    ) -> eyre::Result<()> {
        let (origin_monitor_idx, origin_workspace_idx, origin_container_idx) = origin;
        let (target_monitor_idx, target_workspace_idx, target_container_idx) = target;

        if origin_workspace_idx != target_workspace_idx {
            bail!("cross-desktop transfers must use native desktop movement");
        }
        self.validate_window_destination(origin_monitor_idx, origin_workspace_idx)?;
        self.validate_window_destination(target_monitor_idx, target_workspace_idx)?;

        let origin_container = self
            .monitors_mut()
            .get_mut(origin_monitor_idx)
            .ok_or_eyre("there is no monitor at this index")?
            .workspaces_mut()
            .get_mut(origin_workspace_idx)
            .ok_or_eyre("there is no workspace at this index")?
            .remove_container(origin_container_idx)
            .ok_or_eyre("there is no container at this index")?;

        let target_workspace = self
            .monitors_mut()
            .get_mut(target_monitor_idx)
            .ok_or_eyre("there is no monitor at this index")?
            .workspaces_mut()
            .get_mut(target_workspace_idx)
            .ok_or_eyre("there is no workspace at this index")?;

        target_workspace
            .containers_mut()
            .insert(target_container_idx, origin_container);

        target_workspace.focus_container(target_container_idx);

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn swap_containers(
        &mut self,
        origin: (usize, usize, usize),
        target: (usize, usize, usize),
    ) -> eyre::Result<()> {
        let (origin_monitor_idx, origin_workspace_idx, origin_container_idx) = origin;
        let (target_monitor_idx, target_workspace_idx, target_container_idx) = target;

        if origin_workspace_idx != target_workspace_idx {
            bail!("cross-desktop transfers must use native desktop movement");
        }
        self.validate_window_destination(origin_monitor_idx, origin_workspace_idx)?;
        self.validate_window_destination(target_monitor_idx, target_workspace_idx)?;

        let origin_container_is_valid = self
            .monitors_mut()
            .get_mut(origin_monitor_idx)
            .ok_or_eyre("there is no monitor at this index")?
            .workspaces_mut()
            .get_mut(origin_workspace_idx)
            .ok_or_eyre("there is no workspace at this index")?
            .containers()
            .get(origin_container_idx)
            .is_some();

        let target_container_is_valid = self
            .monitors_mut()
            .get_mut(target_monitor_idx)
            .ok_or_eyre("there is no monitor at this index")?
            .workspaces_mut()
            .get_mut(target_workspace_idx)
            .ok_or_eyre("there is no workspace at this index")?
            .containers()
            .get(origin_container_idx)
            .is_some();

        if origin_container_is_valid && target_container_is_valid {
            let origin_container = self
                .monitors_mut()
                .get_mut(origin_monitor_idx)
                .ok_or_eyre("there is no monitor at this index")?
                .workspaces_mut()
                .get_mut(origin_workspace_idx)
                .ok_or_eyre("there is no workspace at this index")?
                .remove_container(origin_container_idx)
                .ok_or_eyre("there is no container at this index")?;

            let target_container = self
                .monitors_mut()
                .get_mut(target_monitor_idx)
                .ok_or_eyre("there is no monitor at this index")?
                .workspaces_mut()
                .get_mut(target_workspace_idx)
                .ok_or_eyre("there is no workspace at this index")?
                .remove_container(target_container_idx);

            self.monitors_mut()
                .get_mut(target_monitor_idx)
                .ok_or_eyre("there is no monitor at this index")?
                .workspaces_mut()
                .get_mut(target_workspace_idx)
                .ok_or_eyre("there is no workspace at this index")?
                .containers_mut()
                .insert(target_container_idx, origin_container);

            if let Some(target_container) = target_container {
                self.monitors_mut()
                    .get_mut(origin_monitor_idx)
                    .ok_or_eyre("there is no monitor at this index")?
                    .workspaces_mut()
                    .get_mut(origin_workspace_idx)
                    .ok_or_eyre("there is no workspace at this index")?
                    .containers_mut()
                    .insert(origin_container_idx, target_container);
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn update_focused_workspace(
        &mut self,
        follow_focus: bool,
        trigger_focus: bool,
    ) -> eyre::Result<()> {
        tracing::info!("updating");

        let offset = self.work_area_offset;

        self.focused_monitor_mut()
            .ok_or_eyre("there is no monitor")?
            .update_focused_workspace(offset)?;

        if follow_focus {
            if let Some(window) = self.focused_workspace()?.maximized_window {
                if trigger_focus {
                    window.focus(self.mouse_follows_focus)?;
                }
            } else if let Some(container) = &self.focused_workspace()?.monocle_container {
                if let Some(window) = container.focused_window()
                    && trigger_focus
                {
                    window.focus(self.mouse_follows_focus)?;
                }
            } else if let Ok(window) = self.focused_window_mut() {
                if trigger_focus {
                    window.focus(self.mouse_follows_focus)?;
                }
            } else {
                let desktop_window = Window::from(WindowsApi::desktop_window()?);

                let rect = self.focused_monitor_size()?;
                WindowsApi::center_cursor_in_rect(&rect)?;

                match WindowsApi::raise_and_focus_window(desktop_window.hwnd) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!("{} {}:{}", error, file!(), line!());
                    }
                }
            }
        } else {
            if self.focused_workspace()?.is_empty() {
                let desktop_window = Window::from(WindowsApi::desktop_window()?);

                match WindowsApi::raise_and_focus_window(desktop_window.hwnd) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!("{} {}:{}", error, file!(), line!());
                    }
                }
            }

            // if we passed false for follow_focus and there is a container on the workspace
            if self.focused_container_mut().is_ok() {
                // and we have a stack with >1 windows
                if self.focused_container_mut()?.windows().len() > 1
                    // and we don't have a maxed window
                    && self.focused_workspace()?.maximized_window.is_none()
                    // and we don't have a monocle container
                    && self.focused_workspace()?.monocle_container.is_none()
                    // and we don't have any floating windows that should show on top
                    && self.focused_workspace()?.floating_windows().is_empty()
                    && let Ok(window) = self.focused_window_mut()
                        && trigger_focus
                {
                    window.focus(self.mouse_follows_focus)?;
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn resize_window(
        &mut self,
        direction: OperationDirection,
        sizing: Sizing,
        delta: i32,
        update: bool,
    ) -> eyre::Result<()> {
        let mouse_follows_focus = self.mouse_follows_focus;
        let mut focused_monitor_work_area = self.focused_monitor_work_area()?;
        let workspace = self.focused_workspace_mut()?;

        match workspace.layer {
            WorkspaceLayer::Floating => {
                let workspace = self.focused_workspace()?;
                let focused_hwnd = self.window_api.foreground_window()?;

                let border_offset = BORDER_OFFSET.load(Ordering::SeqCst);
                let border_width = BORDER_WIDTH.load(Ordering::SeqCst);
                focused_monitor_work_area.left += border_offset;
                focused_monitor_work_area.left += border_width;
                focused_monitor_work_area.top += border_offset;
                focused_monitor_work_area.top += border_width;
                focused_monitor_work_area.right -= border_offset * 2;
                focused_monitor_work_area.right -= border_width * 2;
                focused_monitor_work_area.bottom -= border_offset * 2;
                focused_monitor_work_area.bottom -= border_width * 2;

                for window in workspace.floating_windows().iter() {
                    if window.hwnd == focused_hwnd {
                        let mut rect = WindowsApi::window_rect(window.hwnd)?;
                        match (direction, sizing) {
                            (OperationDirection::Left, Sizing::Increase) => {
                                if rect.left - delta < focused_monitor_work_area.left {
                                    rect.left = focused_monitor_work_area.left;
                                } else {
                                    rect.left -= delta;
                                }
                            }
                            (OperationDirection::Left, Sizing::Decrease) => {
                                rect.left += delta;
                            }
                            (OperationDirection::Right, Sizing::Increase) => {
                                if rect.left + rect.right + delta * 2
                                    > focused_monitor_work_area.left
                                        + focused_monitor_work_area.right
                                {
                                    rect.right = focused_monitor_work_area.left
                                        + focused_monitor_work_area.right
                                        - rect.left;
                                } else {
                                    rect.right += delta * 2;
                                }
                            }
                            (OperationDirection::Right, Sizing::Decrease) => {
                                rect.right -= delta * 2;
                            }
                            (OperationDirection::Up, Sizing::Increase) => {
                                if rect.top - delta < focused_monitor_work_area.top {
                                    rect.top = focused_monitor_work_area.top;
                                } else {
                                    rect.top -= delta;
                                }
                            }
                            (OperationDirection::Up, Sizing::Decrease) => {
                                rect.top += delta;
                            }
                            (OperationDirection::Down, Sizing::Increase) => {
                                if rect.top + rect.bottom + delta * 2
                                    > focused_monitor_work_area.top
                                        + focused_monitor_work_area.bottom
                                {
                                    rect.bottom = focused_monitor_work_area.top
                                        + focused_monitor_work_area.bottom
                                        - rect.top;
                                } else {
                                    rect.bottom += delta * 2;
                                }
                            }
                            (OperationDirection::Down, Sizing::Decrease) => {
                                rect.bottom -= delta * 2;
                            }
                        }

                        WindowsApi::position_window(window.hwnd, &rect, false, true)?;
                        if mouse_follows_focus {
                            WindowsApi::center_cursor_in_rect(&rect)?;
                        }

                        break;
                    }
                }
            }
            WorkspaceLayer::Tiling => {
                match workspace.layout {
                    Layout::Default(layout) => {
                        tracing::info!("resizing window");
                        let len = NonZeroUsize::new(workspace.containers().len())
                            .ok_or_eyre("there must be at least one container")?;
                        let focused_idx = workspace.focused_container_idx();
                        let focused_idx_resize = workspace
                            .resize_dimensions
                            .get(focused_idx)
                            .ok_or_eyre("there is no resize adjustment for this container")?;

                        if direction
                            .destination(
                                workspace.layout.as_boxed_direction().as_ref(),
                                workspace.layout_flip,
                                focused_idx,
                                len,
                                workspace.layout_options,
                            )
                            .is_some()
                        {
                            let unaltered = layout.calculate(
                                &focused_monitor_work_area,
                                len,
                                workspace.container_padding,
                                workspace.layout_flip,
                                &[],
                                workspace.focused_container_idx(),
                                workspace.layout_options,
                                &workspace.latest_layout,
                            );

                            let mut direction = direction;

                            // We only ever want to operate on the unflipped Rect positions when resizing, then we
                            // can flip them however they need to be flipped once the resizing has been done
                            if let Some(flip) = workspace.layout_flip {
                                match flip {
                                    Axis::Horizontal => {
                                        if matches!(direction, OperationDirection::Left)
                                            || matches!(direction, OperationDirection::Right)
                                        {
                                            direction = direction.opposite();
                                        }
                                    }
                                    Axis::Vertical => {
                                        if matches!(direction, OperationDirection::Up)
                                            || matches!(direction, OperationDirection::Down)
                                        {
                                            direction = direction.opposite();
                                        }
                                    }
                                    Axis::HorizontalAndVertical => direction = direction.opposite(),
                                }
                            }

                            let resize = layout.resize(
                                unaltered
                                    .get(focused_idx)
                                    .ok_or_eyre("there is no last layout")?,
                                focused_idx_resize,
                                direction,
                                sizing,
                                delta,
                            );

                            workspace.resize_dimensions[focused_idx] = resize;

                            return if update {
                                self.update_focused_workspace(false, false)
                            } else {
                                Ok(())
                            };
                        }

                        tracing::warn!("cannot resize container in this direction");
                    }
                    Layout::Custom(_) => {
                        tracing::warn!("containers cannot be resized when using custom layouts");
                    }
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn stop(&mut self, ignore_restore: bool) -> eyre::Result<()> {
        tracing::info!(
            "received stop command, restoring all hidden windows and terminating process"
        );

        let state = &State::from(&*self);
        std::fs::write(
            temp_dir().join("komorebi.state.json"),
            serde_json::to_string_pretty(&state)?,
        )?;

        ANIMATION_ENABLED_PER_ANIMATION.lock().clear();
        ANIMATION_ENABLED_GLOBAL.store(false, Ordering::SeqCst);
        self.restore_all_windows(ignore_restore)?;
        AnimationEngine::wait_for_all_animations();

        if WindowsApi::focus_follows_mouse()? {
            WindowsApi::disable_focus_follows_mouse()?;
        }

        let sockets = SUBSCRIPTION_SOCKETS.lock();
        for path in (*sockets).values() {
            if let Ok(stream) = UnixStream::connect(path) {
                stream.shutdown(Shutdown::Both)?;
            }
        }

        let socket = DATA_DIR.join("komorebi.sock");
        let _ = std::fs::remove_file(socket);

        std::process::exit(0)
    }

    #[tracing::instrument(skip(self))]
    pub fn restore_all_windows(&mut self, ignore_restore: bool) -> eyre::Result<()> {
        tracing::info!("restoring all hidden windows on focused workspaces");

        let no_titlebar = NO_TITLEBAR.lock();
        let regex_identifiers = REGEX_IDENTIFIERS.lock();
        let known_transparent_hwnds = transparency_manager::known_hwnds();
        let border_implementation = border_manager::IMPLEMENTATION.load();

        for monitor in self.monitors_mut() {
            let focused_idx = monitor.focused_workspace_idx();
            if let Some(workspace) = monitor.workspaces_mut().get_mut(focused_idx) {
                if let Some(monocle) = &workspace.monocle_container {
                    for window in monocle.windows() {
                        if matches!(border_implementation, BorderImplementation::Windows) {
                            window.remove_accent()?;
                        }
                    }
                }

                for containers in workspace.containers_mut() {
                    for window in containers.windows_mut() {
                        let should_remove_titlebar_for_window = should_act(
                            &window.title().unwrap_or_default(),
                            &window.exe().unwrap_or_default(),
                            &window.class().unwrap_or_default(),
                            &window.path().unwrap_or_default(),
                            &no_titlebar,
                            &regex_identifiers,
                        )
                        .is_some();

                        if should_remove_titlebar_for_window {
                            window.add_title_bar()?;
                        }

                        if known_transparent_hwnds.contains(&window.hwnd) {
                            window.opaque()?;
                        }

                        if matches!(border_implementation, BorderImplementation::Windows) {
                            window.remove_accent()?;
                        }

                        if !ignore_restore {
                            window.restore();
                        }
                    }
                }

                if let Some(window) = workspace.maximized_window {
                    let should_remove_titlebar_for_window = should_act(
                        &window.title().unwrap_or_default(),
                        &window.exe().unwrap_or_default(),
                        &window.class().unwrap_or_default(),
                        &window.path().unwrap_or_default(),
                        &no_titlebar,
                        &regex_identifiers,
                    )
                    .is_some();

                    if should_remove_titlebar_for_window {
                        window.add_title_bar()?;
                    }

                    if known_transparent_hwnds.contains(&window.hwnd) {
                        window.opaque()?;
                    }

                    if matches!(border_implementation, BorderImplementation::Windows) {
                        window.remove_accent()?;
                    }

                    if !ignore_restore {
                        window.restore();
                    }
                }

                for window in workspace.floating_windows_mut() {
                    let should_remove_titlebar_for_window = should_act(
                        &window.title().unwrap_or_default(),
                        &window.exe().unwrap_or_default(),
                        &window.class().unwrap_or_default(),
                        &window.path().unwrap_or_default(),
                        &no_titlebar,
                        &regex_identifiers,
                    )
                    .is_some();

                    if should_remove_titlebar_for_window {
                        window.add_title_bar()?;
                    }

                    if known_transparent_hwnds.contains(&window.hwnd) {
                        window.opaque()?;
                    }

                    if matches!(border_implementation, BorderImplementation::Windows) {
                        window.remove_accent()?;
                    }

                    if !ignore_restore {
                        window.restore();
                    }
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn remove_all_accents(&mut self) -> eyre::Result<()> {
        tracing::info!("removing all window accents");

        for monitor in self.monitors() {
            for workspace in monitor.workspaces() {
                if let Some(monocle) = &workspace.monocle_container {
                    for window in monocle.windows() {
                        window.remove_accent()?
                    }
                }

                for containers in workspace.containers() {
                    for window in containers.windows() {
                        window.remove_accent()?;
                    }
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    fn handle_unmanaged_window_behaviour(&self) -> eyre::Result<()> {
        if matches!(
            self.unmanaged_window_operation_behaviour,
            OperationBehaviour::NoOp
        ) {
            let workspace = self.focused_workspace()?;
            let focused_hwnd = WindowsApi::foreground_window()?;
            if !workspace.contains_managed_window(focused_hwnd) {
                bail!("ignoring commands while active window is not managed by komorebi");
            }
        }

        Ok(())
    }

    /// Check for an existing wallpaper definition on the workspace/monitor index pair and apply it
    /// if it exists
    #[tracing::instrument(skip(self))]
    pub fn apply_wallpaper_for_monitor_workspace(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
    ) -> eyre::Result<()> {
        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let hmonitor = monitor.id;
        let monitor_wp = monitor.wallpaper.clone();

        let workspace = monitor
            .workspaces()
            .get(workspace_idx)
            .ok_or_eyre("there is no workspace")?;

        workspace.apply_wallpaper(hmonitor, &monitor_wp)
    }

    pub fn update_focused_workspace_by_monitor_idx(&mut self, idx: usize) -> eyre::Result<()> {
        let offset = self.work_area_offset;

        self.monitors_mut()
            .get_mut(idx)
            .ok_or_eyre("there is no monitor")?
            .update_focused_workspace(offset)
    }

    #[tracing::instrument(skip(self))]
    pub fn swap_monitor_workspaces(
        &mut self,
        first_idx: usize,
        second_idx: usize,
    ) -> eyre::Result<()> {
        let _ = (first_idx, second_idx);
        bail!("swapping workspaces between monitors is not supported with native desktops")
    }

    #[tracing::instrument(skip(self))]
    pub fn swap_focused_monitor(&mut self, idx: usize) -> eyre::Result<()> {
        tracing::info!("swapping focused monitor");

        let focused_monitor_idx = self.focused_monitor_idx();
        let mouse_follows_focus = self.mouse_follows_focus;

        self.swap_monitor_workspaces(focused_monitor_idx, idx)?;

        self.update_focused_workspace(mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn move_container_to_monitor(
        &mut self,
        monitor_idx: usize,
        workspace_idx: Option<usize>,
        follow: bool,
        move_direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        self.move_container_to_location(
            monitor_idx,
            workspace_idx.unwrap_or(self.focused_desktop_idx),
            follow,
            move_direction,
        )
    }

    #[tracing::instrument(skip(self))]
    pub fn move_container_to_workspace(
        &mut self,
        idx: usize,
        follow: bool,
        direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        self.move_container_to_location(self.focused_monitor_idx(), idx, follow, direction)
    }

    fn move_container_to_location(
        &mut self,
        target_monitor_idx: usize,
        idx: usize,
        follow: bool,
        direction: Option<OperationDirection>,
    ) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        if idx >= self.desktop_count {
            bail!(
                "desktop index {idx} is out of range (desktop_count: {})",
                self.desktop_count
            );
        }
        let monitor_idx = self.focused_monitor_idx();
        let source_idx = self.focused_workspace_idx_for_monitor_idx(monitor_idx)?;
        self.validate_window_destination(target_monitor_idx, idx)?;
        if idx == source_idx && monitor_idx == target_monitor_idx {
            return Ok(());
        }
        let foreground = self.window_api.foreground_window()?;
        let source = self.focused_workspace()?;
        if matches!(
            self.unmanaged_window_operation_behaviour,
            OperationBehaviour::NoOp
        ) && !source.contains_window(foreground)
        {
            bail!("ignoring commands while active window is not managed by komorebi");
        }
        let floating_idx = source
            .floating_windows()
            .iter()
            .position(|w| w.hwnd == foreground);
        if floating_idx.is_none() && source.maximized_window.is_some() {
            bail!("cannot move native maximized window to another monitor or workspace");
        }
        // Capture the target once. No foreground queries are allowed after the OS moves.
        let monocle = floating_idx.is_none() && source.monocle_container.is_some();
        let container_idx = source.focused_container_idx();
        let selected_window = if let Some(float_idx) = floating_idx {
            source.floating_windows()[float_idx]
        } else {
            let container = if monocle {
                source.monocle_container.as_ref()
            } else {
                source.containers().get(container_idx)
            }
            .ok_or_eyre("there is no focused container")?;
            *container
                .focused_window()
                .ok_or_eyre("there is no focused window")?
        };
        let hwnds = if let Some(float_idx) = floating_idx {
            vec![source.floating_windows()[float_idx].hwnd]
        } else {
            let container = if monocle {
                source.monocle_container.as_ref()
            } else {
                source.containers().get(container_idx)
            }
            .ok_or_eyre("there is no focused container")?;
            container
                .windows()
                .iter()
                .map(|w| w.hwnd)
                .collect::<Vec<_>>()
        };
        // Validate the destination's internal structure before any native side effect.
        self.monitors()[monitor_idx]
            .workspaces()
            .get(idx)
            .ok_or_eyre("there is no destination workspace")?;
        let target_id = self.desktop_ids[idx];
        let backend = virtual_desktop::backend();
        for &hwnd in &hwnds {
            if idx == source_idx {
                break;
            }
            if let Err(error) = backend.move_window_to_desktop_id(hwnd, target_id) {
                // A backend may have moved a window even when it returns an error.
                // Query every member, including the failed and unattempted members.
                let mut reconciliation_errors = Vec::new();
                for &affected in &hwnds {
                    if let Err(reconcile_error) = self.reconcile_window_desktop(affected) {
                        reconciliation_errors.push(format!("{affected}: {reconcile_error}"));
                    }
                }
                self.update_known_hwnds();
                bail!(
                    "failed moving window {hwnd} to desktop {idx}: {error}; reconciliation errors: {reconciliation_errors:?}"
                );
            }
        }

        // Untiled workspaces do not position managed windows during layout updates.
        // Translate every stack member so later stack selections stay on this monitor.
        if monitor_idx != target_monitor_idx
            && (floating_idx.is_some()
                || !self.monitors()[target_monitor_idx].workspaces()[idx].tile)
        {
            let origin = self.monitors()[monitor_idx].work_area_size;
            let target = self.monitors()[target_monitor_idx].work_area_size;
            for &hwnd in &hwnds {
                if let Err(error) =
                    self.window_api
                        .move_window_to_area(Window::from(hwnd), &origin, &target)
                {
                    let mut reconciliation = Vec::new();
                    for &affected in &hwnds {
                        reconciliation.push(self.reconcile_window_desktop(affected));
                    }
                    self.update_known_hwnds();
                    bail!(
                        "monitor movement failed: {error}; native reconciliation: {reconciliation:?}"
                    );
                }
            }
        }
        let monitor = &mut self.monitors_mut()[monitor_idx];
        let source = &mut monitor.workspaces_mut()[source_idx];
        if let Some(float_idx) = floating_idx {
            let window = source
                .floating_windows_mut()
                .remove(float_idx)
                .ok_or_eyre("the selected floating window disappeared")?;
            let target = &mut self.monitors_mut()[target_monitor_idx].workspaces_mut()[idx];
            target.floating_windows_mut().push_back(window);
            target.layer = WorkspaceLayer::Floating;
        } else {
            let container = if monocle {
                source.monocle_container_restore_idx = None;
                source.monocle_container.take()
            } else {
                let container = source.remove_container_by_idx(container_idx);
                source.focus_previous_container();
                container
            }
            .ok_or_eyre("the selected container disappeared")?;
            // Adding to a monocled destination must not restore windows on a hidden desktop.
            // Keep its existing monocle selection; the new container joins the tiling list.
            let monitor = &mut self.monitors_mut()[target_monitor_idx];
            if let Some(direction) = direction {
                monitor.add_container_with_direction(container, Some(idx), direction)?;
            } else {
                monitor.workspaces_mut()[idx].add_container_to_back(container);
            }
        }
        // Publish coherent caches even if layout or focus subsequently fails.
        self.update_known_hwnds();
        if monocle {
            // Monocle hid the other source containers and floats. Restore only each
            // stack's selected window, without focusing it or selecting a desktop.
            // Do this before following the moved container to its destination.
            let source = &self.monitors()[monitor_idx].workspaces()[source_idx];
            for window in source
                .containers()
                .iter()
                .filter_map(|container| container.focused_window())
                .chain(source.floating_windows().iter())
            {
                self.window_api.restore_window(*window);
            }
        }
        self.update_desktop_workspace(monitor_idx, source_idx)?;
        self.update_desktop_workspace(target_monitor_idx, idx)?;
        if follow {
            self.focus_monitor(target_monitor_idx)?;
            self.focus_desktop_by_id(target_id)?;
            if monitor_idx != target_monitor_idx {
                self.window_api
                    .focus_window(selected_window, self.mouse_follows_focus)?;
            }
        } else {
            self.sync_desktop_count()?;
        }
        Ok(())
    }

    /// Update geometry without selecting a desktop, monitor, or foreground window.
    pub(crate) fn update_desktop_workspace(
        &mut self,
        monitor_idx: usize,
        desktop_idx: usize,
    ) -> eyre::Result<()> {
        let offset = self.work_area_offset;
        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;
        monitor.update_workspace_globals(desktop_idx, offset);
        monitor
            .workspaces_mut()
            .get_mut(desktop_idx)
            .ok_or_eyre("there is no workspace")?
            .update()
    }

    /// Reconcile a native window movement without moving the window back or changing focus.
    /// Failed native queries leave the existing association intact and return an explicit error.
    pub(crate) fn reconcile_window_desktop(&mut self, hwnd: isize) -> eyre::Result<()> {
        let locations = self
            .monitors()
            .iter()
            .enumerate()
            .flat_map(|(m, monitor)| {
                monitor
                    .workspaces()
                    .iter()
                    .enumerate()
                    .filter_map(move |(d, ws)| ws.contains_window(hwnd).then_some((m, d)))
            })
            .collect::<Vec<_>>();
        let Some(&(monitor_idx, _)) = locations.first() else {
            // A desktop event does not make an unmanaged window eligible for management.
            self.window_desktops.remove(&hwnd);
            return Ok(());
        };
        let desktop_id = virtual_desktop::backend().desktop_id_for_window(hwnd)?;
        let desktop_idx = self
            .desktop_ids
            .iter()
            .position(|id| *id == desktop_id)
            .ok_or_eyre("window reports a desktop outside the current snapshot")?;
        self.monitors()[monitor_idx]
            .workspaces()
            .get(desktop_idx)
            .ok_or_eyre("there is no destination workspace")?;
        if locations.len() == 1 && locations[0].1 == desktop_idx {
            self.update_known_hwnds();
            return Ok(());
        }
        let mut detached = None;
        for &(m, d) in &locations {
            // Removal changes which stack member should be visible, and removing
            // the last monocle member exposes the ordinary containers and floats.
            // Restore only newly exposed windows; restoring a native desktop's
            // windows does not select that desktop or change foreground focus.
            let visible_windows = |workspace: &Workspace| -> Vec<Window> {
                if let Some(container) = &workspace.monocle_container {
                    container.focused_window().copied().into_iter().collect()
                } else if let Some(window) = workspace.maximized_window {
                    vec![window]
                } else {
                    workspace
                        .containers()
                        .iter()
                        .filter_map(|container| container.focused_window().copied())
                        .chain(workspace.floating_windows().iter().copied())
                        .collect()
                }
            };
            let previously_visible = visible_windows(&self.monitors()[m].workspaces()[d]);
            if let Some(window) =
                self.monitors_mut()[m].workspaces_mut()[d].detach_window_for_desktop_move(hwnd)
            {
                detached.get_or_insert(window);
                for window in visible_windows(&self.monitors()[m].workspaces()[d]) {
                    if !previously_visible.iter().any(|old| old.hwnd == window.hwnd) {
                        self.window_api.restore_window(window);
                    }
                }
            }
        }
        if let Some((window, floating)) = detached {
            let target = &mut self.monitors_mut()[monitor_idx].workspaces_mut()[desktop_idx];
            if floating {
                target.floating_windows_mut().push_back(window);
            } else {
                target.new_container_for_window(window);
            }
        }
        self.update_known_hwnds();
        let mut touched = locations;
        touched.push((monitor_idx, desktop_idx));
        touched.sort_unstable();
        touched.dedup();
        for (m, d) in touched {
            self.update_desktop_workspace(m, d)?;
        }
        Ok(())
    }

    pub fn remove_focused_workspace(&mut self) -> Option<Workspace> {
        tracing::warn!("remove_focused_workspace is a no-op: remove desktops in Task View");
        None
    }

    #[tracing::instrument(skip(self))]
    pub fn move_workspace_to_monitor(&mut self, _idx: usize) -> eyre::Result<()> {
        bail!("moving workspaces between monitors is not supported with native desktops")
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_floating_window_in_direction(
        &mut self,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        let mouse_follows_focus = self.mouse_follows_focus;
        let focused_workspace = self.focused_workspace_mut()?;

        let mut target_idx = None;
        let len = focused_workspace.floating_windows().len();

        if len > 1 {
            let focused_hwnd = WindowsApi::foreground_window()?;
            let focused_rect = WindowsApi::window_rect(focused_hwnd)?;
            match direction {
                OperationDirection::Left => {
                    let mut windows_in_direction = focused_workspace
                        .floating_windows()
                        .iter()
                        .enumerate()
                        .flat_map(|(idx, w)| {
                            (w.hwnd != focused_hwnd)
                                .then_some(WindowsApi::window_rect(w.hwnd).ok().map(|r| (idx, r)))
                        })
                        .flatten()
                        .flat_map(|(idx, r)| {
                            (r.left < focused_rect.left)
                                .then_some((idx, i32::abs(r.left - focused_rect.left)))
                        })
                        .collect::<Vec<_>>();

                    // Sort by distance to focused
                    windows_in_direction.sort_by_key(|(_, d)| (*d as f32 * 1000.0).trunc() as i32);

                    if let Some((idx, _)) = windows_in_direction.first() {
                        target_idx = Some(*idx);
                    }
                }
                OperationDirection::Right => {
                    let mut windows_in_direction = focused_workspace
                        .floating_windows()
                        .iter()
                        .enumerate()
                        .flat_map(|(idx, w)| {
                            (w.hwnd != focused_hwnd)
                                .then_some(WindowsApi::window_rect(w.hwnd).ok().map(|r| (idx, r)))
                        })
                        .flatten()
                        .flat_map(|(idx, r)| {
                            (r.left > focused_rect.left)
                                .then_some((idx, i32::abs(r.left - focused_rect.left)))
                        })
                        .collect::<Vec<_>>();

                    // Sort by distance to focused
                    windows_in_direction.sort_by_key(|(_, d)| (*d as f32 * 1000.0).trunc() as i32);

                    if let Some((idx, _)) = windows_in_direction.first() {
                        target_idx = Some(*idx);
                    }
                }
                OperationDirection::Up => {
                    let mut windows_in_direction = focused_workspace
                        .floating_windows()
                        .iter()
                        .enumerate()
                        .flat_map(|(idx, w)| {
                            (w.hwnd != focused_hwnd)
                                .then_some(WindowsApi::window_rect(w.hwnd).ok().map(|r| (idx, r)))
                        })
                        .flatten()
                        .flat_map(|(idx, r)| {
                            (r.top < focused_rect.top)
                                .then_some((idx, i32::abs(r.top - focused_rect.top)))
                        })
                        .collect::<Vec<_>>();

                    // Sort by distance to focused
                    windows_in_direction.sort_by_key(|(_, d)| (*d as f32 * 1000.0).trunc() as i32);

                    if let Some((idx, _)) = windows_in_direction.first() {
                        target_idx = Some(*idx);
                    }
                }
                OperationDirection::Down => {
                    let mut windows_in_direction = focused_workspace
                        .floating_windows()
                        .iter()
                        .enumerate()
                        .flat_map(|(idx, w)| {
                            (w.hwnd != focused_hwnd)
                                .then_some(WindowsApi::window_rect(w.hwnd).ok().map(|r| (idx, r)))
                        })
                        .flatten()
                        .flat_map(|(idx, r)| {
                            (r.top > focused_rect.top)
                                .then_some((idx, i32::abs(r.top - focused_rect.top)))
                        })
                        .collect::<Vec<_>>();

                    // Sort by distance to focused
                    windows_in_direction.sort_by_key(|(_, d)| (*d as f32 * 1000.0).trunc() as i32);

                    if let Some((idx, _)) = windows_in_direction.first() {
                        target_idx = Some(*idx);
                    }
                }
            };
        }

        if let Some(idx) = target_idx {
            focused_workspace.floating_windows.focus(idx);
            if let Some(window) = focused_workspace.floating_windows().get(idx) {
                window.focus(mouse_follows_focus)?;
            }
            return Ok(());
        }

        let mut cross_monitor_monocle_or_max = false;

        let workspace_idx = self.focused_workspace_idx()?;

        // this is for when we are scrolling across workspaces like PaperWM
        if matches!(
            self.cross_boundary_behaviour,
            CrossBoundaryBehaviour::Workspace
        ) && matches!(
            direction,
            OperationDirection::Left | OperationDirection::Right
        ) {
            let workspace_count = if let Some(monitor) = self.focused_monitor() {
                monitor.workspaces().len()
            } else {
                1
            };

            let next_idx = match direction {
                OperationDirection::Left => match workspace_idx {
                    0 => workspace_count - 1,
                    n => n - 1,
                },
                OperationDirection::Right => match workspace_idx {
                    n if n == workspace_count - 1 => 0,
                    n => n + 1,
                },
                _ => workspace_idx,
            };

            self.focus_workspace(next_idx)?;

            if let Ok(focused_workspace) = self.focused_workspace_mut()
                && focused_workspace.monocle_container.is_none()
            {
                match direction {
                    OperationDirection::Left => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(
                                focused_workspace.containers().len().saturating_sub(1),
                            );
                        }
                    },
                    OperationDirection::Right => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(0);
                        }
                    },
                    _ => {}
                };
            }

            return Ok(());
        }

        // if there is no floating_window in that direction for this workspace
        let monitor_idx = self
            .monitor_idx_in_direction(direction)
            .ok_or_eyre("there is no container or monitor in this direction")?;

        self.focus_monitor(monitor_idx)?;
        let mouse_follows_focus = self.mouse_follows_focus;

        if let Ok(focused_workspace) = self.focused_workspace_mut() {
            if let Some(window) = focused_workspace.maximized_window {
                window.focus(mouse_follows_focus)?;
                cross_monitor_monocle_or_max = true;
            } else if let Some(monocle) = &focused_workspace.monocle_container {
                if let Some(window) = monocle.focused_window() {
                    window.focus(mouse_follows_focus)?;
                    cross_monitor_monocle_or_max = true;
                }
            } else if focused_workspace.layer == WorkspaceLayer::Tiling {
                match direction {
                    OperationDirection::Left => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(
                                focused_workspace.containers().len().saturating_sub(1),
                            );
                        }
                    },
                    OperationDirection::Right => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(0);
                        }
                    },
                    _ => {}
                };
            }
        }

        if !cross_monitor_monocle_or_max {
            let ws = self.focused_workspace_mut()?;
            if ws.is_empty() {
                // This is to remove focus from the previous monitor
                let desktop_window = Window::from(WindowsApi::desktop_window()?);

                match WindowsApi::raise_and_focus_window(desktop_window.hwnd) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!("{} {}:{}", error, file!(), line!());
                    }
                }
            } else if ws.layer == WorkspaceLayer::Floating && !ws.floating_windows().is_empty() {
                if let Some(window) = ws.focused_floating_window() {
                    window.focus(self.mouse_follows_focus)?;
                }
            } else {
                ws.layer = WorkspaceLayer::Tiling;
                if let Ok(focused_window) = self.focused_window() {
                    focused_window.focus(self.mouse_follows_focus)?;
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn preselect_container_in_direction(
        &mut self,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        let workspace = self.focused_workspace_mut()?;
        let focused_idx = workspace.focused_container_idx();

        if matches!(workspace.layout, Layout::Default(DefaultLayout::Grid)) {
            tracing::warn!("preselection is not supported on the grid layout");
            return Ok(());
        }

        tracing::info!("preselecting container");

        let new_idx =
            if workspace.maximized_window.is_some() || workspace.monocle_container.is_some() {
                None
            } else {
                workspace.new_idx_for_direction(direction)
            };

        match new_idx {
            Some(new_idx) => {
                let adjusted_idx = match direction {
                    OperationDirection::Left | OperationDirection::Up => {
                        if focused_idx.abs_diff(new_idx) == 1 {
                            new_idx + 1
                        } else {
                            new_idx
                        }
                    }
                    _ => new_idx,
                };

                workspace.preselect_container_idx(adjusted_idx);
            }
            None => {
                tracing::debug!(
                    "this is not a valid preselection direction from the current position"
                )
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_container_in_direction(
        &mut self,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace()?;
        let workspace_idx = self.focused_workspace_idx()?;

        tracing::info!("focusing container");

        if workspace.monocle_container.is_some()
            && matches!(self.monocle_focus_behaviour, MonocleFocusBehaviour::Cycle)
        {
            let cycle_direction = match direction {
                OperationDirection::Left | OperationDirection::Down => CycleDirection::Previous,
                OperationDirection::Right | OperationDirection::Up => CycleDirection::Next,
            };
            return self.cycle_monocle(cycle_direction);
        }

        let new_idx =
            if workspace.maximized_window.is_some() || workspace.monocle_container.is_some() {
                None
            } else {
                workspace.new_idx_for_direction(direction)
            };

        let mut cross_monitor_monocle_or_max = false;

        // this is for when we are scrolling across workspaces like PaperWM
        if new_idx.is_none()
            && matches!(
                self.cross_boundary_behaviour,
                CrossBoundaryBehaviour::Workspace
            )
            && matches!(
                direction,
                OperationDirection::Left | OperationDirection::Right
            )
        {
            let workspace_count = if let Some(monitor) = self.focused_monitor() {
                monitor.workspaces().len()
            } else {
                1
            };

            let next_idx = match direction {
                OperationDirection::Left => match workspace_idx {
                    0 => workspace_count - 1,
                    n => n - 1,
                },
                OperationDirection::Right => match workspace_idx {
                    n if n == workspace_count - 1 => 0,
                    n => n + 1,
                },
                _ => workspace_idx,
            };

            self.focus_workspace(next_idx)?;

            if let Ok(focused_workspace) = self.focused_workspace_mut()
                && focused_workspace.monocle_container.is_none()
            {
                match direction {
                    OperationDirection::Left => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(
                                focused_workspace.containers().len().saturating_sub(1),
                            );
                        }
                    },
                    OperationDirection::Right => match focused_workspace.layout {
                        Layout::Default(layout) => {
                            let target_index = direction.cross_boundary_edge_index(
                                layout,
                                focused_workspace.containers().len(),
                                focused_workspace.layout_flip,
                            );
                            focused_workspace.focus_container(target_index);
                        }
                        Layout::Custom(_) => {
                            focused_workspace.focus_container(0);
                        }
                    },
                    _ => {}
                };
            }

            return Ok(());
        }

        // if there is no container in that direction for this workspace
        match new_idx {
            None => {
                let monitor_idx = self
                    .monitor_idx_in_direction(direction)
                    .ok_or_eyre("there is no container or monitor in this direction")?;

                self.focus_monitor(monitor_idx)?;
                let mouse_follows_focus = self.mouse_follows_focus;

                if let Ok(focused_workspace) = self.focused_workspace_mut() {
                    if let Some(window) = focused_workspace.maximized_window {
                        window.focus(mouse_follows_focus)?;
                        cross_monitor_monocle_or_max = true;
                    } else if let Some(monocle) = &focused_workspace.monocle_container {
                        if let Some(window) = monocle.focused_window() {
                            window.focus(mouse_follows_focus)?;
                            cross_monitor_monocle_or_max = true;
                        }
                    } else if focused_workspace.layer == WorkspaceLayer::Tiling {
                        match direction {
                            OperationDirection::Left => match focused_workspace.layout {
                                Layout::Default(layout) => {
                                    let target_index = direction.cross_boundary_edge_index(
                                        layout,
                                        focused_workspace.containers().len(),
                                        focused_workspace.layout_flip,
                                    );
                                    focused_workspace.focus_container(target_index);
                                }
                                Layout::Custom(_) => {
                                    focused_workspace.focus_container(
                                        focused_workspace.containers().len().saturating_sub(1),
                                    );
                                }
                            },
                            OperationDirection::Right => match focused_workspace.layout {
                                Layout::Default(layout) => {
                                    let target_index = direction.cross_boundary_edge_index(
                                        layout,
                                        focused_workspace.containers().len(),
                                        focused_workspace.layout_flip,
                                    );
                                    focused_workspace.focus_container(target_index);
                                }
                                Layout::Custom(_) => {
                                    focused_workspace.focus_container(0);
                                }
                            },
                            _ => {}
                        };
                    }
                }
            }
            Some(idx) => {
                let workspace = self.focused_workspace_mut()?;
                workspace.focus_container(idx);
            }
        }

        if !cross_monitor_monocle_or_max {
            let ws = self.focused_workspace_mut()?;
            if ws.is_empty() {
                // This is to remove focus from the previous monitor
                let desktop_window = Window::from(WindowsApi::desktop_window()?);

                match WindowsApi::raise_and_focus_window(desktop_window.hwnd) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!("{} {}:{}", error, file!(), line!());
                    }
                }
            } else if ws.layer == WorkspaceLayer::Floating && !ws.floating_windows().is_empty() {
                if let Some(window) = ws.focused_floating_window() {
                    window.focus(self.mouse_follows_focus)?;
                }
            } else {
                ws.layer = WorkspaceLayer::Tiling;
                if let Ok(focused_window) = self.focused_window() {
                    focused_window.focus(self.mouse_follows_focus)?;
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn move_floating_window_in_direction(
        &mut self,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        let mouse_follows_focus = self.mouse_follows_focus;

        let mut focused_monitor_work_area = self.focused_monitor_work_area()?;
        let border_offset = BORDER_OFFSET.load(Ordering::SeqCst);
        let border_width = BORDER_WIDTH.load(Ordering::SeqCst);
        focused_monitor_work_area.left += border_offset;
        focused_monitor_work_area.left += border_width;
        focused_monitor_work_area.top += border_offset;
        focused_monitor_work_area.top += border_width;
        focused_monitor_work_area.right -= border_offset * 2;
        focused_monitor_work_area.right -= border_width * 2;
        focused_monitor_work_area.bottom -= border_offset * 2;
        focused_monitor_work_area.bottom -= border_width * 2;

        let focused_workspace = self.focused_workspace()?;
        let delta = self.resize_delta;

        let focused_hwnd = WindowsApi::foreground_window()?;
        for window in focused_workspace.floating_windows().iter() {
            if window.hwnd == focused_hwnd {
                let mut rect = WindowsApi::window_rect(window.hwnd)?;
                match direction {
                    OperationDirection::Left => {
                        if rect.left - delta < focused_monitor_work_area.left {
                            rect.left = focused_monitor_work_area.left;
                        } else {
                            rect.left -= delta;
                        }
                    }
                    OperationDirection::Right => {
                        if rect.left + delta + rect.right
                            > focused_monitor_work_area.left + focused_monitor_work_area.right
                        {
                            rect.left = focused_monitor_work_area.left
                                + focused_monitor_work_area.right
                                - rect.right;
                        } else {
                            rect.left += delta;
                        }
                    }
                    OperationDirection::Up => {
                        if rect.top - delta < focused_monitor_work_area.top {
                            rect.top = focused_monitor_work_area.top;
                        } else {
                            rect.top -= delta;
                        }
                    }
                    OperationDirection::Down => {
                        if rect.top + delta + rect.bottom
                            > focused_monitor_work_area.top + focused_monitor_work_area.bottom
                        {
                            rect.top = focused_monitor_work_area.top
                                + focused_monitor_work_area.bottom
                                - rect.bottom;
                        } else {
                            rect.top += delta;
                        }
                    }
                }

                WindowsApi::position_window(window.hwnd, &rect, false, true)?;
                if mouse_follows_focus {
                    WindowsApi::center_cursor_in_rect(&rect)?;
                }

                break;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn move_container_in_direction(
        &mut self,
        direction: OperationDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace()?;
        let workspace_idx = self.focused_workspace_idx()?;

        // removing this messes up the monitor / container / window index somewhere
        // and results in the wrong window getting moved across the monitor boundary
        if workspace.is_focused_window_monocle_or_maximized()? {
            bail!("ignoring command while active window is in monocle mode or maximized");
        }

        tracing::info!("moving container");

        let origin_container_idx = workspace.focused_container_idx();
        let origin_monitor_idx = self.focused_monitor_idx();
        let target_container_idx = workspace.new_idx_for_direction(direction);

        // this is for when we are scrolling across workspaces like PaperWM
        if target_container_idx.is_none()
            && matches!(
                self.cross_boundary_behaviour,
                CrossBoundaryBehaviour::Workspace
            )
            && matches!(
                direction,
                OperationDirection::Left | OperationDirection::Right
            )
        {
            let workspace_count = if let Some(monitor) = self.focused_monitor() {
                monitor.workspaces().len()
            } else {
                1
            };

            let next_idx = match direction {
                OperationDirection::Left => match workspace_idx {
                    0 => workspace_count - 1,
                    n => n - 1,
                },
                OperationDirection::Right => match workspace_idx {
                    n if n == workspace_count - 1 => 0,
                    n => n + 1,
                },
                _ => workspace_idx,
            };

            // passing the direction here is how we handle whether to insert at the front
            // or the back of the container vecdeque in the target workspace
            self.move_container_to_workspace(next_idx, true, Some(direction))?;
            self.update_focused_workspace(self.mouse_follows_focus, true)?;

            return Ok(());
        }

        match target_container_idx {
            // If there is nowhere to move on the current workspace, try to move it onto the monitor
            // in that direction if there is one
            None => {
                // Don't do anything if the user has set the MoveBehaviour to NoOp
                if matches!(self.cross_monitor_move_behaviour, MoveBehaviour::NoOp) {
                    return Ok(());
                }

                let target_monitor_idx = self
                    .monitor_idx_in_direction(direction)
                    .ok_or_eyre("there is no container or monitor in this direction")?;

                {
                    // actually move the container to target monitor using the direction
                    self.move_container_to_monitor(
                        target_monitor_idx,
                        None,
                        true,
                        Some(direction),
                    )?;

                    // focus the target monitor
                    self.focus_monitor(target_monitor_idx)?;

                    // unset monocle container on target workspace if there is one
                    let mut target_workspace_has_monocle = false;
                    if let Ok(target_workspace) = self.focused_workspace()
                        && target_workspace.monocle_container.is_some()
                    {
                        target_workspace_has_monocle = true;
                    }

                    if target_workspace_has_monocle {
                        self.toggle_monocle()?;
                    }

                    // get a mutable ref to the focused workspace on the target monitor
                    let target_workspace = self.focused_workspace_mut()?;

                    // if there is only one container on the target workspace after the insertion
                    // it means that there won't be one swapped back, so we have to decrement the
                    // focused position
                    if target_workspace.containers().len() == 1 {
                        let origin_workspace =
                            self.focused_workspace_for_monitor_idx_mut(origin_monitor_idx)?;

                        origin_workspace.focus_container(
                            origin_workspace.focused_container_idx().saturating_sub(1),
                        );
                    }
                }

                // if our MoveBehaviour is Swap, let's try to send back the window container
                // whose position which just took over
                if matches!(self.cross_monitor_move_behaviour, MoveBehaviour::Swap) {
                    {
                        let target_workspace = self.focused_workspace_mut()?;

                        // if the target workspace doesn't have more than one container, this means it
                        // was previously empty, by only doing the second part of the swap when there is
                        // more than one container, we can fall back to a "move" if there is nothing to
                        // swap with on the target monitor
                        if target_workspace.containers().len() > 1 {
                            // remove the container from the target monitor workspace
                            let target_container = target_workspace
                                // this is now focused_container_idx + 1 because we have inserted our origin container
                                .remove_container_by_idx(
                                    target_workspace.focused_container_idx() + 1,
                                )
                                .ok_or_eyre("could not remove container at given target index")?;

                            let origin_workspace =
                                self.focused_workspace_for_monitor_idx_mut(origin_monitor_idx)?;

                            // insert the container from the target monitor workspace into the origin monitor workspace
                            // at the same position from which our origin container was removed
                            origin_workspace
                                .insert_container_at_idx(origin_container_idx, target_container);
                        }
                    }
                }

                // make sure to update the origin monitor workspace layout because it is no
                // longer focused so it won't get updated at the end of this fn
                let offset = self.work_area_offset;

                self.monitors_mut()
                    .get_mut(origin_monitor_idx)
                    .ok_or_eyre("there is no monitor at this index")?
                    .update_focused_workspace(offset)?;

                let a = self
                    .focused_monitor()
                    .ok_or_eyre("there is no monitor focused monitor")?
                    .id;
                let b = self
                    .monitors_mut()
                    .get_mut(origin_monitor_idx)
                    .ok_or_eyre("there is no monitor at this index")?
                    .id;

                if !WindowsApi::monitors_have_same_dpi(a, b)? {
                    self.update_focused_workspace(self.mouse_follows_focus, true)?;
                }
            }
            Some(new_idx) => {
                let workspace = self.focused_workspace_mut()?;
                workspace.swap_containers(origin_container_idx, new_idx);
                workspace.focus_container(new_idx);
            }
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_floating_window_in_cycle_direction(
        &mut self,
        direction: CycleDirection,
    ) -> eyre::Result<()> {
        let mouse_follows_focus = self.mouse_follows_focus;
        let focused_workspace = self.focused_workspace()?;

        let mut target_idx = None;
        let len = focused_workspace.floating_windows().len();

        if len > 1 {
            let focused_hwnd = WindowsApi::foreground_window()?;
            for (idx, window) in focused_workspace.floating_windows().iter().enumerate() {
                if window.hwnd == focused_hwnd {
                    match direction {
                        CycleDirection::Previous => {
                            if idx == 0 {
                                target_idx = Some(len - 1)
                            } else {
                                target_idx = Some(idx - 1)
                            }
                        }
                        CycleDirection::Next => {
                            if idx == len - 1 {
                                target_idx = Some(0)
                            } else {
                                target_idx = Some(idx - 1)
                            }
                        }
                    }
                }
            }

            if target_idx.is_none() {
                target_idx = Some(0);
            }
        }

        if let Some(idx) = target_idx
            && let Some(window) = focused_workspace.floating_windows().get(idx)
        {
            window.focus(mouse_follows_focus)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_container_in_cycle_direction(
        &mut self,
        direction: CycleDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("focusing container");
        let mut maximize_next = false;
        let mut monocle_next = false;

        if self.focused_workspace_mut()?.maximized_window.is_some() {
            maximize_next = true;
            self.unmaximize_window()?;
        }

        if self.focused_workspace_mut()?.monocle_container.is_some() {
            monocle_next = true;
            self.monocle_off()?;
        }

        let workspace = self.focused_workspace_mut()?;

        let new_idx = workspace
            .new_idx_for_cycle_direction(direction)
            .ok_or_eyre("this is not a valid direction from the current position")?;

        workspace.focus_container(new_idx);

        if maximize_next {
            self.toggle_maximize()?;
        } else if monocle_next {
            self.toggle_monocle()?;
        } else {
            self.focused_window_mut()?.focus(self.mouse_follows_focus)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn move_container_in_cycle_direction(
        &mut self,
        direction: CycleDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace_mut()?;
        if workspace.is_focused_window_monocle_or_maximized()? {
            bail!("ignoring command while active window is in monocle mode or maximized");
        }

        tracing::info!("moving container");

        let current_idx = workspace.focused_container_idx();
        let new_idx = workspace
            .new_idx_for_cycle_direction(direction)
            .ok_or_eyre("this is not a valid direction from the current position")?;

        workspace.swap_containers(current_idx, new_idx);
        workspace.focus_container(new_idx);
        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn cycle_container_window_in_direction(
        &mut self,
        direction: CycleDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("cycling container windows");

        let container =
            if let Some(container) = &mut self.focused_workspace_mut()?.monocle_container {
                container
            } else {
                self.focused_container_mut()?
            };

        let len = NonZeroUsize::new(container.windows().len())
            .ok_or_eyre("there must be at least one window in a container")?;

        if len.get() == 1 {
            bail!("there is only one window in this container");
        }

        let current_idx = container.focused_window_idx();
        let next_idx = direction.next_idx(current_idx, len);

        container.focus_window(next_idx);
        container.load_focused_window();

        if let Some(window) = container.focused_window() {
            window.focus(self.mouse_follows_focus)?;
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn cycle_container_window_index_in_direction(
        &mut self,
        direction: CycleDirection,
    ) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("cycling container window index");

        let container =
            if let Some(container) = &mut self.focused_workspace_mut()?.monocle_container {
                container
            } else {
                self.focused_container_mut()?
            };

        let len = NonZeroUsize::new(container.windows().len())
            .ok_or_eyre("there must be at least one window in a container")?;

        if len.get() == 1 {
            bail!("there is only one window in this container");
        }

        let current_idx = container.focused_window_idx();
        let next_idx = direction.next_idx(current_idx, len);
        container.windows_mut().swap(current_idx, next_idx);

        container.focus_window(next_idx);
        container.load_focused_window();

        if let Some(window) = container.focused_window() {
            window.focus(self.mouse_follows_focus)?;
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)
    }
    #[tracing::instrument(skip(self))]
    pub fn focus_container_window(&mut self, idx: usize) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("focusing container window at index {idx}");

        let container =
            if let Some(container) = &mut self.focused_workspace_mut()?.monocle_container {
                container
            } else {
                self.focused_container_mut()?
            };

        let len = NonZeroUsize::new(container.windows().len())
            .ok_or_eyre("there must be at least one window in a container")?;

        if len.get() == 1 && idx != 0 {
            bail!("there is only one window in this container");
        }

        if container.windows().get(idx).is_none() {
            bail!("there is no window in this container at index {idx}");
        }

        container.focus_window(idx);
        container.load_focused_window();

        if let Some(window) = container.focused_window() {
            window.focus(self.mouse_follows_focus)?;
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn stack_all(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;
        tracing::info!("stacking all windows on workspace");

        let workspace = self.focused_workspace_mut()?;

        let mut focused_hwnd = None;
        if let Some(container) = workspace.focused_container()
            && let Some(window) = container.focused_window()
        {
            focused_hwnd = Some(window.hwnd);
        }

        let workspace_hwnds =
            workspace
                .containers()
                .iter()
                .fold(VecDeque::new(), |mut hwnds, c| {
                    hwnds.extend(c.windows().clone());
                    hwnds
                });

        let mut container = Container::default();
        *container.windows_mut() = workspace_hwnds;
        *workspace.containers_mut() = VecDeque::from([container]);
        workspace.focus_container(0);

        if let Some(hwnd) = focused_hwnd
            && let Some(c) = workspace.focused_container_mut()
            && let Some(w_idx_to_focus) = c.idx_for_window(hwnd)
        {
            c.focus_window(w_idx_to_focus);
            c.load_focused_window();
        }
        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn unstack_all(&mut self, update_workspace: bool) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;
        tracing::info!("unstacking all windows in container");

        let workspace = self.focused_workspace_mut()?;

        let mut focused_hwnd = None;
        if let Some(container) = workspace.focused_container()
            && let Some(window) = container.focused_window()
        {
            focused_hwnd = Some(window.hwnd);
        }

        let initial_focused_container_index = workspace.focused_container_idx();
        let mut focused_container = workspace.focused_container().cloned();

        while let Some(focused) = &focused_container {
            if focused.windows().len() > 1 {
                workspace.new_container_for_focused_window()?;
                workspace.focus_container(initial_focused_container_index);
                focused_container = workspace.focused_container().cloned();
            } else {
                focused_container = None;
            }
        }

        if let Some(hwnd) = focused_hwnd {
            workspace.focus_container_by_window(hwnd)?;
        }

        if update_workspace {
            self.update_focused_workspace(self.mouse_follows_focus, true)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn add_window_to_container(&mut self, direction: OperationDirection) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("adding window to container");

        let workspace = self.focused_workspace_mut()?;
        let len = NonZeroUsize::new(workspace.containers_mut().len())
            .ok_or_eyre("there must be at least one container")?;
        let current_container_idx = workspace.focused_container_idx();

        let is_valid = direction
            .destination(
                workspace.layout.as_boxed_direction().as_ref(),
                workspace.layout_flip,
                workspace.focused_container_idx(),
                len,
                workspace.layout_options,
            )
            .is_some();

        if is_valid {
            let new_idx = workspace
                .new_idx_for_direction(direction)
                .ok_or_eyre("this is not a valid direction from the current position")?;

            let mut changed_focus = false;

            let adjusted_new_index = if new_idx > current_container_idx
                && !matches!(
                    workspace.layout,
                    Layout::Default(DefaultLayout::Grid)
                        | Layout::Default(DefaultLayout::UltrawideVerticalStack)
                ) {
                workspace.focus_container(new_idx);
                changed_focus = true;
                new_idx.saturating_sub(1)
            } else {
                new_idx
            };

            let mut target_container_is_stack = false;

            if let Some(container) = workspace.containers().get(adjusted_new_index)
                && container.windows().len() > 1
            {
                target_container_is_stack = true;
            }

            if let Some(current) = workspace.focused_container() {
                if current.windows().len() > 1 && !target_container_is_stack {
                    workspace.focus_container(adjusted_new_index);
                    changed_focus = true;
                    workspace.move_window_to_container(current_container_idx)?;
                } else {
                    workspace.move_window_to_container(adjusted_new_index)?;
                }
            }

            if changed_focus && let Some(container) = workspace.focused_container_mut() {
                container.load_focused_window();
                if let Some(window) = container.focused_window() {
                    window.focus(self.mouse_follows_focus)?;
                }
            }

            self.update_focused_workspace(self.mouse_follows_focus, false)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn promote_container_to_front(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace_mut()?;

        if matches!(workspace.layout, Layout::Default(DefaultLayout::Grid)) {
            tracing::debug!("ignoring promote command for grid layout");
            return Ok(());
        }

        tracing::info!("promoting container");

        workspace.promote_container()?;
        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn promote_container_swap(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace_mut()?;
        let focused_container_idx = workspace.focused_container_idx();

        let primary_idx = match &workspace.layout {
            Layout::Default(_) => 0,
            Layout::Custom(layout) => layout.first_container_idx(
                layout
                    .primary_idx()
                    .ok_or_eyre("this custom layout does not have a primary column")?,
            ),
        };

        if matches!(workspace.layout, Layout::Default(DefaultLayout::Grid)) {
            tracing::debug!("ignoring promote-swap command for grid layout");
            return Ok(());
        }

        let primary_tile_is_focused = focused_container_idx == primary_idx;

        if primary_tile_is_focused && let Some(swap_idx) = workspace.promotion_swap_container_idx {
            workspace.swap_containers(focused_container_idx, swap_idx);
        } else {
            workspace.promotion_swap_container_idx = Some(focused_container_idx);
            workspace.swap_containers(focused_container_idx, primary_idx);
        }

        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn promote_focus_to_front(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace_mut()?;

        if matches!(workspace.layout, Layout::Default(DefaultLayout::Grid)) {
            tracing::info!("ignoring promote focus command for grid layout");
            return Ok(());
        }

        tracing::info!("promoting focus");

        let target_idx = match &workspace.layout {
            Layout::Default(_) => 0,
            Layout::Custom(custom) => custom.first_container_idx(custom.primary_idx().unwrap_or(0)),
        };

        workspace.focus_container(target_idx);
        self.update_focused_workspace(self.mouse_follows_focus, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn remove_window_from_container(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        tracing::info!("removing window");

        if self.focused_container()?.windows().len() == 1 {
            bail!("a container must have at least one window");
        }

        let workspace = self.focused_workspace_mut()?;

        workspace.new_container_for_focused_window()?;
        self.update_focused_workspace(self.mouse_follows_focus, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn toggle_tiling(&mut self) -> eyre::Result<()> {
        let workspace = self.focused_workspace_mut()?;
        workspace.tile = !workspace.tile;
        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn toggle_float(&mut self, force_float: bool) -> eyre::Result<()> {
        let hwnd = WindowsApi::foreground_window()?;
        let workspace = self.focused_workspace_mut()?;
        if workspace.monocle_container.is_some() {
            tracing::warn!("ignoring toggle-float command while workspace has a monocle container");
            return Ok(());
        }

        let mut is_floating_window = false;

        for window in workspace.floating_windows() {
            if window.hwnd == hwnd {
                is_floating_window = true;
            }
        }

        if is_floating_window && !force_float {
            workspace.layer = WorkspaceLayer::Tiling;
            self.unfloat_window()?;
        } else {
            workspace.layer = WorkspaceLayer::Floating;
            self.float_window()?;
        }

        self.update_focused_workspace(is_floating_window, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn toggle_lock(&mut self) -> eyre::Result<()> {
        let workspace = self.focused_workspace_mut()?;
        if let Some(container) = workspace.focused_container_mut() {
            // Toggle the locked flag
            container.locked = !container.locked;
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn float_window(&mut self) -> eyre::Result<()> {
        tracing::info!("floating window");

        let work_area = self.focused_monitor_work_area()?;

        let toggle_float_placement = self.window_management_behaviour.toggle_float_placement;

        let workspace = self.focused_workspace_mut()?;
        workspace.new_floating_window()?;

        let window = workspace
            .floating_windows_mut()
            .back_mut()
            .ok_or_eyre("there is no floating window")?;

        if toggle_float_placement.should_center() {
            window.center(&work_area, toggle_float_placement.should_resize())?;
        }
        window.focus(self.mouse_follows_focus)?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn unfloat_window(&mut self) -> eyre::Result<()> {
        tracing::info!("unfloating window");

        let workspace = self.focused_workspace_mut()?;
        workspace.new_container_for_floating_window()
    }

    #[tracing::instrument(skip(self))]
    pub fn toggle_monocle(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace()?;
        match workspace.monocle_container {
            None => self.monocle_on()?,
            Some(_) => self.monocle_off()?,
        }

        self.update_focused_workspace(true, true)?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn monocle_on(&mut self) -> eyre::Result<()> {
        tracing::info!("enabling monocle");

        let workspace = self.focused_workspace_mut()?;
        workspace.new_monocle_container()?;

        for container in workspace.containers_mut() {
            container.hide(None);
        }

        for window in workspace.floating_windows_mut() {
            window.hide();
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn monocle_off(&mut self) -> eyre::Result<()> {
        tracing::info!("disabling monocle");

        let workspace = self.focused_workspace_mut()?;

        for container in workspace.containers_mut() {
            container.restore();
        }

        for window in workspace.floating_windows_mut() {
            window.restore();
        }

        workspace.reintegrate_monocle_container()
    }

    #[tracing::instrument(skip(self))]
    pub fn cycle_monocle(&mut self, direction: CycleDirection) -> eyre::Result<()> {
        tracing::info!("cycling monocle container");

        if self.focused_workspace()?.containers().is_empty() {
            return Ok(());
        }

        self.focused_workspace_mut()?
            .cycle_monocle_container(direction)?;

        for container in self.focused_workspace_mut()?.containers_mut() {
            container.hide(None);
        }

        // borders were getting funny during cycles, can't be bothered to root cause it
        border_manager::destroy_all_borders()?;

        self.update_focused_workspace(true, true)
    }

    #[tracing::instrument(skip(self))]
    pub fn toggle_maximize(&mut self) -> eyre::Result<()> {
        self.handle_unmanaged_window_behaviour()?;

        let workspace = self.focused_workspace_mut()?;

        match workspace.maximized_window {
            None => self.maximize_window()?,
            Some(_) => self.unmaximize_window()?,
        }

        self.update_focused_workspace(true, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn maximize_window(&mut self) -> eyre::Result<()> {
        tracing::info!("maximizing windowj");

        let workspace = self.focused_workspace_mut()?;
        workspace.new_maximized_window()
    }

    #[tracing::instrument(skip(self))]
    pub fn unmaximize_window(&mut self) -> eyre::Result<()> {
        tracing::info!("unmaximizing window");

        let workspace = self.focused_workspace_mut()?;
        workspace.reintegrate_maximized_window()
    }

    #[tracing::instrument(skip(self))]
    pub fn flip_layout(&mut self, layout_flip: Axis) -> eyre::Result<()> {
        let workspace = self.focused_workspace_mut()?;

        tracing::info!("flipping layout");

        #[allow(clippy::match_same_arms)]
        match workspace.layout_flip {
            None => {
                workspace.layout_flip = Option::from(layout_flip);
            }
            Some(current_layout_flip) => {
                match current_layout_flip {
                    Axis::Horizontal => match layout_flip {
                        Axis::Horizontal => workspace.layout_flip = None,
                        Axis::Vertical => {
                            workspace.layout_flip = Option::from(Axis::HorizontalAndVertical)
                        }
                        Axis::HorizontalAndVertical => {
                            workspace.layout_flip = Option::from(Axis::HorizontalAndVertical)
                        }
                    },
                    Axis::Vertical => match layout_flip {
                        Axis::Horizontal => {
                            workspace.layout_flip = Option::from(Axis::HorizontalAndVertical)
                        }
                        Axis::Vertical => workspace.layout_flip = None,
                        Axis::HorizontalAndVertical => {
                            workspace.layout_flip = Option::from(Axis::HorizontalAndVertical)
                        }
                    },
                    Axis::HorizontalAndVertical => match layout_flip {
                        Axis::Horizontal => workspace.layout_flip = Option::from(Axis::Vertical),
                        Axis::Vertical => workspace.layout_flip = Option::from(Axis::Horizontal),
                        Axis::HorizontalAndVertical => workspace.layout_flip = None,
                    },
                };
            }
        }

        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn change_workspace_layout_default(&mut self, layout: DefaultLayout) -> eyre::Result<()> {
        tracing::info!("changing layout");

        let monitor_count = self.monitors().len();
        let workspace = self.focused_workspace_mut()?;

        if monitor_count > 1 && matches!(layout, DefaultLayout::Scrolling) {
            tracing::warn!(
                "scrolling layout is only supported for a single monitor; not changing layout"
            );
            return Ok(());
        }

        match &workspace.layout {
            Layout::Default(_) => {}
            Layout::Custom(layout) => {
                let primary_idx = layout.first_container_idx(
                    layout
                        .primary_idx()
                        .ok_or_eyre("this custom layout does not have a primary column")?,
                );

                if !workspace.containers().is_empty() && primary_idx < workspace.containers().len()
                {
                    workspace.swap_containers(0, primary_idx);
                }
            }
        }

        workspace.layout = Layout::Default(layout);
        self.update_focused_workspace(self.mouse_follows_focus, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn cycle_layout(&mut self, direction: CycleDirection) -> eyre::Result<()> {
        tracing::info!("cycling layout");

        let workspace = self.focused_workspace_mut()?;
        let current_layout = &workspace.layout;

        match current_layout {
            Layout::Default(current) => {
                let new_layout = match direction {
                    CycleDirection::Previous => current.cycle_previous(),
                    CycleDirection::Next => current.cycle_next(),
                };

                tracing::info!("next layout: {new_layout}");
                workspace.layout = Layout::Default(new_layout);
            }
            Layout::Custom(_) => {}
        }

        self.update_focused_workspace(self.mouse_follows_focus, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn change_workspace_custom_layout<P>(&mut self, path: P) -> eyre::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        tracing::info!("changing layout");

        let layout = CustomLayout::from_path(path)?;
        let workspace = self.focused_workspace_mut()?;

        match workspace.layout {
            Layout::Default(_) => {
                let primary_idx = layout.first_container_idx(
                    layout
                        .primary_idx()
                        .ok_or_eyre("this custom layout does not have a primary column")?,
                );

                if !workspace.containers().is_empty() && primary_idx < workspace.containers().len()
                {
                    workspace.swap_containers(0, primary_idx);
                }
            }
            Layout::Custom(_) => {}
        }

        workspace.layout = Layout::Custom(layout);
        workspace.layout_flip = None;
        self.update_focused_workspace(self.mouse_follows_focus, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn adjust_workspace_padding(
        &mut self,
        sizing: Sizing,
        adjustment: i32,
    ) -> eyre::Result<()> {
        tracing::info!("adjusting workspace padding");

        let workspace = self.focused_workspace_mut()?;

        let padding = workspace
            .workspace_padding
            .ok_or_eyre("there is no workspace padding")?;

        workspace.workspace_padding = Option::from(sizing.adjust_by(padding, adjustment));

        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn adjust_container_padding(
        &mut self,
        sizing: Sizing,
        adjustment: i32,
    ) -> eyre::Result<()> {
        tracing::info!("adjusting container padding");

        let workspace = self.focused_workspace_mut()?;

        let padding = workspace
            .container_padding
            .ok_or_eyre("there is no container padding")?;

        workspace.container_padding = Option::from(sizing.adjust_by(padding, adjustment));

        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn set_workspace_tiling(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        tile: bool,
    ) -> eyre::Result<()> {
        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.tile = tile;

        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn add_workspace_layout_default_rule(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        at_container_count: usize,
        layout: DefaultLayout,
    ) -> eyre::Result<()> {
        tracing::info!("setting workspace layout");

        let focused_monitor_idx = self.focused_monitor_idx();

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let focused_workspace_idx = monitor.focused_workspace_idx();

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        let rules: &mut Vec<(usize, Layout)> = &mut workspace.layout_rules;
        rules.retain(|pair| pair.0 != at_container_count);
        rules.push((at_container_count, Layout::Default(layout)));
        rules.sort_by_key(|a| a.0);

        // If this is the focused workspace on a non-focused screen, let's update it
        if focused_monitor_idx != monitor_idx && focused_workspace_idx == workspace_idx {
            workspace.update()?;
            Ok(())
        } else {
            Ok(self.update_focused_workspace(false, false)?)
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn add_workspace_layout_custom_rule<P>(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        at_container_count: usize,
        path: P,
    ) -> eyre::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        tracing::info!("setting workspace layout");

        let focused_monitor_idx = self.focused_monitor_idx();

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let focused_workspace_idx = monitor.focused_workspace_idx();

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        let layout = CustomLayout::from_path(path)?;

        let rules: &mut Vec<(usize, Layout)> = &mut workspace.layout_rules;
        rules.retain(|pair| pair.0 != at_container_count);
        rules.push((at_container_count, Layout::Custom(layout)));
        rules.sort_by_key(|a| a.0);

        // If this is the focused workspace on a non-focused screen, let's update it
        if focused_monitor_idx != monitor_idx && focused_workspace_idx == workspace_idx {
            workspace.update()?;
            Ok(())
        } else {
            Ok(self.update_focused_workspace(false, false)?)
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn clear_workspace_layout_rules(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
    ) -> eyre::Result<()> {
        tracing::info!("setting workspace layout");

        let focused_monitor_idx = self.focused_monitor_idx();

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let focused_workspace_idx = monitor.focused_workspace_idx();

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        let rules: &mut Vec<(usize, Layout)> = &mut workspace.layout_rules;
        rules.clear();

        // If this is the focused workspace on a non-focused screen, let's update it
        if focused_monitor_idx != monitor_idx && focused_workspace_idx == workspace_idx {
            workspace.update()?;
            Ok(())
        } else {
            Ok(self.update_focused_workspace(false, false)?)
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn set_workspace_layout_default(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        layout: DefaultLayout,
    ) -> eyre::Result<()> {
        tracing::info!("setting workspace layout");

        let focused_monitor_idx = self.focused_monitor_idx();

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let focused_workspace_idx = monitor.focused_workspace_idx();

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.layout = Layout::Default(layout);

        // If this is the focused workspace on a non-focused screen, let's update it
        if focused_monitor_idx != monitor_idx && focused_workspace_idx == workspace_idx {
            workspace.update()?;
            Ok(())
        } else {
            Ok(self.update_focused_workspace(false, false)?)
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn set_workspace_layout_custom<P>(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        path: P,
    ) -> eyre::Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        tracing::info!("setting workspace layout");
        let layout = CustomLayout::from_path(path)?;
        let focused_monitor_idx = self.focused_monitor_idx();

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let focused_workspace_idx = monitor.focused_workspace_idx();

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.layout = Layout::Custom(layout);
        workspace.layout_flip = None;

        // If this is the focused workspace on a non-focused screen, let's update it
        if focused_monitor_idx != monitor_idx && focused_workspace_idx == workspace_idx {
            workspace.update()?;
            Ok(())
        } else {
            Ok(self.update_focused_workspace(false, false)?)
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn ensure_workspaces_for_monitor(
        &mut self,
        _monitor_idx: usize,
        _workspace_count: usize,
    ) -> eyre::Result<()> {
        tracing::warn!(
            "ensure_workspaces_for_monitor is a no-op: desktops are managed by Task View"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn ensure_named_workspaces_for_monitor(
        &mut self,
        _monitor_idx: usize,
        _names: &Vec<String>,
    ) -> eyre::Result<()> {
        tracing::warn!(
            "ensure_named_workspaces_for_monitor is a no-op: desktops are managed by Task View"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn set_workspace_padding(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        size: i32,
    ) -> eyre::Result<()> {
        tracing::info!("setting workspace padding");

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.workspace_padding = Option::from(size);

        self.update_focused_workspace(false, false)
    }

    #[tracing::instrument(skip(self))]
    pub fn set_workspace_name(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        name: String,
    ) -> eyre::Result<()> {
        let _ = monitor_idx; // Compatibility argument: the native name is global.
        self.sync_desktop_count()?;
        let id = *self
            .desktop_ids
            .get(workspace_idx)
            .ok_or_eyre("native desktop name index is out of range")?;
        virtual_desktop::backend().set_desktop_name_id(id, &name)?;
        self.sync_desktop_count()
    }

    #[tracing::instrument(skip(self))]
    pub fn set_container_padding(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        size: i32,
    ) -> eyre::Result<()> {
        tracing::info!("setting container padding");

        let monitor = self
            .monitors_mut()
            .get_mut(monitor_idx)
            .ok_or_eyre("there is no monitor")?;

        let workspace = monitor
            .workspaces_mut()
            .get_mut(workspace_idx)
            .ok_or_eyre("there is no monitor")?;

        workspace.container_padding = Option::from(size);

        self.update_focused_workspace(false, false)
    }

    pub fn focused_monitor_size(&self) -> eyre::Result<Rect> {
        Ok(self
            .focused_monitor()
            .ok_or_eyre("there is no monitor")?
            .size)
    }

    pub fn focused_monitor_work_area(&self) -> eyre::Result<Rect> {
        Ok(self
            .focused_monitor()
            .ok_or_eyre("there is no monitor")?
            .work_area_size)
    }

    #[tracing::instrument(skip(self))]
    pub fn focus_monitor(&mut self, idx: usize) -> eyre::Result<()> {
        tracing::info!("focusing monitor");

        if self.monitors().get(idx).is_some() {
            self.monitors.focus(idx);
        } else {
            bail!("this is not a valid monitor index");
        }

        Ok(())
    }

    pub fn monitor_idx_from_window(&mut self, window: Window) -> Option<usize> {
        let hmonitor = self.window_api.monitor_from_window(window.hwnd);

        for (i, monitor) in self.monitors().iter().enumerate() {
            if monitor.id == hmonitor {
                return Option::from(i);
            }
        }

        // our hmonitor might be stale, so if we didn't return above, try querying via the latest
        // info taken from win32_display_data and update our hmonitor while we're at it
        if let Ok(latest) = WindowsApi::monitor(hmonitor) {
            for (i, monitor) in self.monitors_mut().iter_mut().enumerate() {
                if monitor.device_id == latest.device_id {
                    monitor.id = latest.id;
                    return Option::from(i);
                }
            }
        }

        None
    }

    pub fn monitor_idx_from_current_pos(&mut self) -> Option<usize> {
        let hmonitor = WindowsApi::monitor_from_point(WindowsApi::cursor_pos().ok()?);

        for (i, monitor) in self.monitors().iter().enumerate() {
            if monitor.id == hmonitor {
                return Option::from(i);
            }
        }

        // our hmonitor might be stale, so if we didn't return above, try querying via the latest
        // info taken from win32_display_data and update our hmonitor while we're at it
        if let Ok(latest) = WindowsApi::monitor(hmonitor) {
            for (i, monitor) in self.monitors_mut().iter_mut().enumerate() {
                if monitor.device_id == latest.device_id {
                    monitor.id = latest.id;
                    return Option::from(i);
                }
            }
        }

        None
    }

    pub fn focused_workspace_idx(&self) -> eyre::Result<usize> {
        // Route through the global index; no per-monitor Ring is consulted.
        Ok(self.focused_desktop_idx)
    }

    pub fn focused_workspace(&self) -> eyre::Result<&Workspace> {
        let idx = self.focused_desktop_idx;
        self.focused_monitor()
            .ok_or_eyre("there is no monitor")?
            .workspaces()
            .get(idx)
            .ok_or_eyre("there is no workspace")
    }

    pub fn focused_workspace_mut(&mut self) -> eyre::Result<&mut Workspace> {
        let idx = self.focused_desktop_idx;
        self.focused_monitor_mut()
            .ok_or_eyre("there is no monitor")?
            .workspaces_mut()
            .get_mut(idx)
            .ok_or_eyre("there is no workspace")
    }

    pub fn focused_workspace_idx_for_monitor_idx(&self, idx: usize) -> eyre::Result<usize> {
        // All monitors share the same focused index (the global one).
        // Validate that the monitor exists but ignore its per-Ring index.
        self.monitors()
            .get(idx)
            .ok_or_eyre("there is no monitor at this index")?;
        Ok(self.focused_desktop_idx)
    }

    /// Returns `true` if the given window's cloak event was likely caused by a native
    /// desktop transition rather than a user action.
    ///
    /// When Windows switches virtual desktops, it automatically cloaks windows on the
    /// desktop being left and uncloaks windows on the desktop being entered. These
    /// cloak events should not trigger komorebi's reconciliation logic.
    pub fn is_desktop_transition_cloak(&self, window: Window) -> bool {
        // First check our tracked state, then fall back to backend
        let window_desktop = self.window_desktops.get(&window.hwnd).copied().or_else(|| {
            crate::virtual_desktop::backend()
                .desktop_idx_for_window(window.hwnd)
                .ok()
        });
        window_desktop != Some(self.focused_desktop_idx)
            || self.is_window_on_other_desktop(window.hwnd)
    }

    /// Returns `true` if the given window handle belongs to a different native desktop
    /// than the currently focused one. This is used to prevent reaping or reconciling
    /// windows that are on other desktops.
    pub fn is_window_on_other_desktop(&self, hwnd: isize) -> bool {
        let tracked = self.window_desktops.get(&hwnd).copied();
        if tracked.is_some_and(|idx| idx != self.focused_desktop_idx) {
            return true;
        }
        // The OS may cloak a window before its queued desktop event reaches the WM.
        // Preserve it if either the tracked state or current native identities say background.
        let backend = virtual_desktop::backend();
        if let Ok(current) = backend.current_desktop_id() {
            let native = backend
                .desktop_id_for_window(hwnd)
                .ok()
                .or_else(|| tracked.and_then(|idx| self.desktop_ids.get(idx).copied()));
            return native.is_some_and(|id| id != current);
        }
        false
    }

    pub fn focused_workspace_for_monitor_idx(&self, idx: usize) -> eyre::Result<&Workspace> {
        self.monitors()
            .get(idx)
            .ok_or_eyre("there is no monitor at this index")?
            .focused_workspace()
            .ok_or_eyre("there is no workspace")
    }

    pub fn focused_workspace_for_monitor_idx_mut(
        &mut self,
        idx: usize,
    ) -> eyre::Result<&mut Workspace> {
        self.monitors_mut()
            .get_mut(idx)
            .ok_or_eyre("there is no monitor at this index")?
            .focused_workspace_mut()
            .ok_or_eyre("there is no workspace")
    }

    /// Reconcile each monitor's workspace ring against an ordered native identity snapshot.
    /// Preflight orphan destinations before committing, preserving the last coherent
    /// association if a live window's native desktop cannot yet be queried.
    ///
    /// After this call the following invariant holds:
    ///   **Every monitor has `workspaces().len() == desktop_count`, and
    ///   every monitor's `focused_workspace_idx()` equals `focused_desktop_idx`.**
    pub fn sync_desktop_count(&mut self) -> eyre::Result<()> {
        let backend = virtual_desktop::backend();
        let snapshot = backend.snapshot()?;
        let ids = snapshot.desktops.iter().map(|d| d.id).collect::<Vec<_>>();
        let positions = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect::<HashMap<_, _>>();
        if ids.is_empty() || positions.len() != ids.len() {
            bail!("native desktop snapshot is empty or contains duplicate identities");
        }
        let focused = *positions
            .get(&snapshot.current)
            .ok_or_eyre("current desktop is missing from snapshot")?;
        let bootstrap = self.desktop_ids.is_empty();
        let old_ids = self.desktop_ids.clone();
        let previous_current = old_ids.get(self.focused_desktop_idx).copied();
        let mut destinations = HashMap::new();
        let mut closed = HashSet::new();
        // Resolve all orphan destinations before touching any association or count.
        for monitor in self.monitors() {
            for (i, workspace) in monitor.workspaces().iter().enumerate() {
                let id = workspace.native_desktop_id.or_else(|| {
                    if bootstrap {
                        ids.get(i).copied()
                    } else {
                        old_ids.get(i).copied()
                    }
                });
                let survives = id.is_some_and(|id| positions.contains_key(&id));
                if !survives {
                    for hwnd in Self::workspace_hwnds(workspace) {
                        let native_id = match backend.desktop_id_for_window(hwnd) {
                            Ok(id) => id,
                            Err(_) if !self.window_api.is_window(hwnd) => {
                                closed.insert(hwnd);
                                continue;
                            }
                            Err(error) => return Err(error),
                        };
                        let target = *positions
                            .get(&native_id)
                            .ok_or_eyre("orphan window reports a desktop outside the snapshot")?;
                        destinations.insert(hwnd, target);
                    }
                }
            }
        }
        if !destinations.is_empty() && backend.snapshot()? != snapshot {
            bail!("native desktops changed while resolving removed workspace windows");
        }
        // Work on a clone, preserving the last coherent state if preflight failed.
        let mut monitors = self.monitors.clone();
        let mut monitor_topology_changed = false;
        for monitor in monitors.elements_mut() {
            monitor_topology_changed |= monitor.workspaces().len() != ids.len()
                || monitor
                    .workspaces()
                    .iter()
                    .zip(&ids)
                    .any(|(ws, id)| ws.native_desktop_id != Some(*id));
            let last_id = monitor.last_focused_workspace.and_then(|i| {
                monitor
                    .workspaces()
                    .get(i)
                    .and_then(|ws| ws.native_desktop_id)
                    .or_else(|| old_ids.get(i).copied())
            });
            let old = std::mem::take(monitor.workspaces_mut());
            let mut surviving = HashMap::new();
            let mut removed = Vec::new();
            for (i, workspace) in old.into_iter().enumerate() {
                let id = workspace.native_desktop_id.or_else(|| {
                    if bootstrap {
                        ids.get(i).copied()
                    } else {
                        old_ids.get(i).copied()
                    }
                });
                if let Some(id) = id.filter(|id| positions.contains_key(id)) {
                    surviving.insert(id, workspace);
                } else {
                    removed.push(workspace);
                }
            }
            *monitor.workspaces_mut() = ids
                .iter()
                .map(|id| surviving.remove(id).unwrap_or_default())
                .collect();
            for mut workspace in removed {
                let mut containers = std::mem::take(workspace.containers_mut());
                if let Some(container) = workspace.monocle_container.take() {
                    containers.push_back(container);
                }
                for mut container in containers {
                    container
                        .windows_mut()
                        .retain(|w| !closed.contains(&w.hwnd));
                    if container.windows().is_empty() {
                        continue;
                    }
                    let focused_hwnd = container.focused_window().map(|w| w.hwnd);
                    let mut groups: HashMap<usize, Container> = HashMap::new();
                    let targets = container
                        .windows()
                        .iter()
                        .map(|w| destinations[&w.hwnd])
                        .collect::<HashSet<_>>();
                    if targets.len() == 1 {
                        let target = *targets.iter().next().ok_or_eyre("empty orphan container")?;
                        Self::adopt_desktop_container(
                            &mut monitor.workspaces_mut()[target],
                            container,
                        );
                    } else {
                        for &window in container.windows() {
                            let group = groups.entry(destinations[&window.hwnd]).or_default();
                            group.locked = container.locked;
                            group.add_window_without_visibility_changes(window);
                        }
                        for (target, mut group) in groups {
                            if let Some(idx) =
                                focused_hwnd.and_then(|hwnd| group.idx_for_window(hwnd))
                            {
                                group.focus_window(idx);
                            }
                            Self::adopt_desktop_container(
                                &mut monitor.workspaces_mut()[target],
                                group,
                            );
                        }
                    }
                }
                let mut floats = std::mem::take(workspace.floating_windows_mut());
                if let Some(window) = workspace.maximized_window.take() {
                    floats.push_back(window);
                }
                for window in floats {
                    if closed.contains(&window.hwnd) {
                        continue;
                    }
                    let target = &mut monitor.workspaces_mut()[destinations[&window.hwnd]];
                    if !target.contains_window(window.hwnd) {
                        target.floating_windows_mut().push_back(window);
                    }
                }
            }
            monitor.workspace_names.clear();
            for (i, desktop) in snapshot.desktops.iter().enumerate() {
                monitor.workspaces_mut()[i].native_desktop_id = Some(desktop.id);
                monitor.workspaces_mut()[i].name = desktop.name.clone();
                if let Some(name) = &desktop.name {
                    monitor.workspace_names.insert(i, name.clone());
                }
            }
            let last_id = if previous_current.is_some_and(|id| id != snapshot.current) {
                previous_current
            } else {
                last_id
            };
            monitor.last_focused_workspace = last_id.and_then(|id| positions.get(&id).copied());
            monitor.set_focused_workspace_idx(focused);
        }
        let topology_changed = old_ids != ids || monitor_topology_changed;
        let focus_changed = self.focused_desktop_idx != focused;
        self.monitors = monitors;
        self.desktop_ids = ids;
        self.desktop_count = snapshot.desktops.len();
        self.focused_desktop_idx = focused;
        self.workspace_names = snapshot
            .desktops
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.name.clone().map(|n| (i, n)))
            .collect();
        self.update_known_hwnds();
        if topology_changed || focus_changed {
            for m in 0..self.monitors().len() {
                // Retile all adopted workspaces without selecting or moving a native desktop.
                for d in 0..self.desktop_count {
                    self.update_desktop_workspace(m, d)?;
                }
            }
            border_manager::redraw_borders_for_focused_desktop(self)?;
        }
        Ok(())
    }

    pub(crate) fn workspace_hwnds(workspace: &Workspace) -> Vec<isize> {
        let mut hwnds = workspace
            .containers()
            .iter()
            .flat_map(|c| c.windows().iter().map(|w| w.hwnd))
            .collect::<Vec<_>>();
        hwnds.extend(workspace.floating_windows().iter().map(|w| w.hwnd));
        if let Some(c) = &workspace.monocle_container {
            hwnds.extend(c.windows().iter().map(|w| w.hwnd));
        }
        if let Some(w) = workspace.maximized_window {
            hwnds.push(w.hwnd);
        }
        hwnds
    }

    fn adopt_desktop_container(target: &mut Workspace, mut container: Container) {
        let selected_window = container.focused_window().map(|w| w.hwnd);
        container
            .windows_mut()
            .retain(|w| !target.contains_window(w.hwnd));
        if container.windows().is_empty() {
            return;
        }
        let selected_idx = selected_window
            .and_then(|hwnd| container.idx_for_window(hwnd))
            .unwrap_or(0);
        container.focus_window(selected_idx);
        let selected = target.focused_container_idx();
        let had_selection = target.focused_container().is_some();
        target.add_container_to_back(container);
        if had_selection {
            target.focus_container(selected);
        }
    }

    /// Switch to a native virtual desktop by index, affecting all monitors.
    ///
    /// This validates the index, calls the desktop backend to perform the OS-level
    /// switch, updates the global focused_desktop_idx, and synchronizes all monitors'
    /// Ring indices so that `monitor.focused_workspace_idx()` remains coherent.
    #[tracing::instrument(skip(self))]
    /// Match Windows navigation: reaching either end leaves the current desktop selected.
    pub fn focus_adjacent_desktop(&mut self, direction: CycleDirection) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        let target = match direction {
            CycleDirection::Previous => self.focused_desktop_idx.checked_sub(1),
            CycleDirection::Next => self
                .focused_desktop_idx
                .checked_add(1)
                .filter(|&idx| idx < self.desktop_count),
        };
        if let Some(idx) = target {
            self.focus_desktop(idx)?;
        }
        Ok(())
    }

    pub fn focus_desktop(&mut self, idx: usize) -> eyre::Result<()> {
        self.sync_desktop_count()?;
        let id = *self
            .desktop_ids
            .get(idx)
            .ok_or_eyre(format!("desktop index {idx} is out of range"))?;
        self.focus_desktop_by_id(id)
    }

    fn focus_desktop_by_id(&mut self, id: virtual_desktop::DesktopId) -> eyre::Result<()> {
        virtual_desktop::backend().switch_desktop_id(id)?;
        self.sync_desktop_count()
    }

    /// Alias for `focus_desktop` to maintain compatibility with existing code.
    ///
    /// In the native-desktop model, "workspace" and "desktop" are synonymous—
    /// all monitors share a single global desktop index.
    #[tracing::instrument(skip(self))]
    pub fn focus_workspace(&mut self, idx: usize) -> eyre::Result<()> {
        self.focus_desktop(idx)
    }

    /// Returns the native desktop index where a new window should be placed.
    ///
    /// Priority:
    /// 1. If we've seen this window before, use our tracked desktop
    /// 2. Otherwise, query the backend (works for fake backend in tests)
    /// 3. If the native query fails, use the focused desktop for an unknown window.
    /// A successful but stale query is not corrected by this fallback; native
    /// WindowDesktopChanged events explicitly reconcile existing associations.
    #[tracing::instrument(skip(self))]
    pub fn desktop_idx_for_new_window(&mut self, window: Window) -> eyre::Result<usize> {
        let hwnd = window.hwnd;

        // If we've seen this window before, use our tracked desktop
        if let Some(&tracked_desktop) = self.window_desktops.get(&hwnd) {
            tracing::debug!(
                "Using tracked desktop {} for window {}",
                tracked_desktop,
                hwnd
            );
            return Ok(tracked_desktop.min(self.desktop_count.saturating_sub(1)));
        }

        // Resolve the native identity in our snapshot; never clamp an unknown ID
        // to an unrelated surviving desktop during concurrent topology changes.
        if let Ok(id) = virtual_desktop::backend().desktop_id_for_window(hwnd) {
            let idx = self
                .desktop_ids
                .iter()
                .position(|known| *known == id)
                .ok_or_eyre("new window reports a desktop outside the current snapshot")?;
            self.window_desktops.insert(hwnd, idx);
            return Ok(idx);
        }

        // Fallback for a failed native query, not for successfully returned stale data.
        tracing::debug!(
            "New window {}, using focused desktop {}",
            hwnd,
            self.focused_desktop_idx
        );
        Ok(self.focused_desktop_idx)
    }

    #[tracing::instrument(skip(self))]
    pub fn monitor_workspace_index_by_name(&mut self, name: &str) -> Option<(usize, usize)> {
        (0..self.desktop_count)
            .find(|i| self.workspace_names.get(i).is_some_and(|n| n == name))
            .map(|d| (self.focused_monitor_idx(), d))
    }

    #[tracing::instrument(skip(self))]
    pub fn new_workspace(&mut self) -> eyre::Result<()> {
        tracing::warn!("new_workspace is a no-op: desktops are managed by Task View");
        Ok(())
    }

    pub fn focused_container(&self) -> eyre::Result<&Container> {
        self.focused_workspace()?
            .focused_container()
            .ok_or_eyre("there is no container")
    }

    pub fn focused_container_idx(&self) -> eyre::Result<usize> {
        Ok(self.focused_workspace()?.focused_container_idx())
    }

    pub fn focused_container_mut(&mut self) -> eyre::Result<&mut Container> {
        self.focused_workspace_mut()?
            .focused_container_mut()
            .ok_or_eyre("there is no container")
    }

    pub fn focused_window(&self) -> eyre::Result<&Window> {
        self.focused_container()?
            .focused_window()
            .ok_or_eyre("there is no window")
    }

    fn focused_window_mut(&mut self) -> eyre::Result<&mut Window> {
        self.focused_container_mut()?
            .focused_window_mut()
            .ok_or_eyre("there is no window")
    }

    /// Updates the list of `known_hwnds` and their monitor/workspace index pair
    ///
    /// [`known_hwnds`]: `Self.known_hwnds`
    pub fn update_known_hwnds(&mut self) {
        tracing::trace!("updating list of known hwnds");
        let mut known_hwnds = HashMap::new();
        let mut window_desktops = HashMap::new();
        for (m_idx, monitor) in self.monitors().iter().enumerate() {
            for (w_idx, workspace) in monitor.workspaces().iter().enumerate() {
                for container in workspace.containers() {
                    for window in container.windows() {
                        known_hwnds.insert(window.hwnd, (m_idx, w_idx));
                        window_desktops.insert(window.hwnd, w_idx);
                    }
                }

                for window in workspace.floating_windows() {
                    known_hwnds.insert(window.hwnd, (m_idx, w_idx));
                    window_desktops.insert(window.hwnd, w_idx);
                }

                if let Some(window) = workspace.maximized_window {
                    known_hwnds.insert(window.hwnd, (m_idx, w_idx));
                    window_desktops.insert(window.hwnd, w_idx);
                }

                if let Some(container) = &workspace.monocle_container {
                    for window in container.windows() {
                        known_hwnds.insert(window.hwnd, (m_idx, w_idx));
                        window_desktops.insert(window.hwnd, w_idx);
                    }
                }
            }
        }

        // Update our tracked window desktops
        self.window_desktops = window_desktops;

        // Refresh every derived cache, even if another consumer cleared its copy.
        *crate::reaper::HWNDS_CACHE.lock() = known_hwnds.clone();
        if self.known_hwnds != known_hwnds {
            // Save to file
            let hwnd_json = DATA_DIR.join("komorebi.hwnd.json");
            match OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(hwnd_json)
            {
                Ok(file) => {
                    if let Err(error) =
                        serde_json::to_writer_pretty(&file, &known_hwnds.keys().collect::<Vec<_>>())
                    {
                        tracing::error!("Failed to save list of known_hwnds on file: {}", error);
                    }
                }
                Err(error) => {
                    tracing::error!("Failed to save list of known_hwnds on file: {}", error);
                }
            }

            // Store new hwnds
            self.known_hwnds = known_hwnds;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor;
    use crate::virtual_desktop;
    use crate::virtual_desktop::testing::FakeDesktopBackend;
    use crossbeam_channel::Sender;
    use crossbeam_channel::bounded;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestContext {
        socket_path: Option<PathBuf>,
        _backend_guard: Option<virtual_desktop::testing::InstalledBackendGuard>,
        reaper_cache: HashMap<isize, (usize, usize)>,
        hidden_hwnds: Vec<isize>,
        border_redraws: usize,
        workspace_rules: Vec<crate::core::config_generation::WorkspaceMatchingRule>,
        floating_rules: Vec<crate::core::config_generation::MatchingRule>,
        display_preferences: HashMap<usize, String>,
        monitor_preferences: HashMap<usize, Rect>,
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            if let Some(socket_path) = &self.socket_path {
                // Clean up the socket file
                let _ = std::fs::remove_file(socket_path);
            }
            *crate::reaper::HWNDS_CACHE.lock() = self.reaper_cache.clone();
            *crate::HIDDEN_HWNDS.lock() = self.hidden_hwnds.clone();
            crate::border_manager::BORDER_REDRAWS.store(self.border_redraws, Ordering::SeqCst);
            *crate::WORKSPACE_MATCHING_RULES.lock() = self.workspace_rules.clone();
            *crate::FLOATING_APPLICATIONS.lock() = self.floating_rules.clone();
            *crate::DISPLAY_INDEX_PREFERENCES.write() = self.display_preferences.clone();
            *crate::MONITOR_INDEX_PREFERENCES.lock() = self.monitor_preferences.clone();
        }
    }

    fn setup_window_manager() -> (WindowManager, TestContext) {
        let backend_guard = virtual_desktop::testing::InstalledBackendGuard::new();
        virtual_desktop::set_backend(Some(Arc::new(FakeDesktopBackend::new(10))));
        let (_sender, receiver): (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) =
            bounded(1);

        // Temporary socket path for testing
        let socket_name = format!("komorebi-test-{}.sock", Uuid::new_v4());
        let socket_path = PathBuf::from(socket_name);

        // Create a new WindowManager instance
        let wm = WindowManager::new_with_window_api(
            receiver,
            Some(socket_path.clone()),
            Arc::new(crate::windows_api::FakeWindowApi::default()),
        );

        // Window Manager should be created successfully
        assert!(wm.is_ok());

        (
            wm.unwrap(),
            TestContext {
                socket_path: Some(socket_path),
                _backend_guard: Some(backend_guard),
                reaper_cache: crate::reaper::HWNDS_CACHE.lock().clone(),
                hidden_hwnds: crate::HIDDEN_HWNDS.lock().clone(),
                border_redraws: crate::border_manager::BORDER_REDRAWS.load(Ordering::SeqCst),
                workspace_rules: crate::WORKSPACE_MATCHING_RULES.lock().clone(),
                floating_rules: crate::FLOATING_APPLICATIONS.lock().clone(),
                display_preferences: crate::DISPLAY_INDEX_PREFERENCES.read().clone(),
                monitor_preferences: crate::MONITOR_INDEX_PREFERENCES.lock().clone(),
            },
        )
    }

    /// Create a `WindowManager` backed by a `FakeDesktopBackend` with `count`
    /// desktops, add one test monitor, and call `sync_desktop_count` so the
    /// workspace lists and global index are already in a consistent state.
    ///
    /// The returned context owns the socket and restores the previous backend.
    fn setup_window_manager_with_fake_desktops(
        count: usize,
    ) -> (WindowManager, Arc<FakeDesktopBackend>, TestContext) {
        let (mut wm, ctx) = setup_window_manager();
        let fake = Arc::new(FakeDesktopBackend::new(count));
        virtual_desktop::set_backend(Some(
            fake.clone() as Arc<dyn virtual_desktop::DesktopBackend>
        ));

        // Add one test monitor so sync_desktop_count has something to resize.
        let m = monitor::new(
            0,
            crate::core::Rect::default(),
            crate::core::Rect::default(),
            "FakeMonitor".to_string(),
            "FakeDevice".to_string(),
            "FakeDeviceID".to_string(),
            Some("FakeSN".to_string()),
        );
        wm.monitors_mut().push_back(m);

        wm.sync_desktop_count().unwrap();

        (wm, fake, ctx)
    }

    #[test]
    fn test_create_window_manager() {
        let (_wm, _test_context) = setup_window_manager();
    }

    #[test]
    fn test_focus_workspace() {
        let (mut wm, _test_context) = setup_window_manager();

        let m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // a new monitor should have a single workspace
        assert_eq!(m.workspaces().len(), 1);

        // the next index on the monitor should be the not-yet-created second workspace
        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        // add the monitor to the window manager
        wm.monitors_mut().push_back(m);

        {
            // focusing a workspace which doesn't yet exist should create it
            let monitor = wm.focused_monitor_mut().unwrap();
            monitor.sync_workspace_count(monitor.workspaces().len().max((new_workspace_index) + 1));
            monitor.focus_workspace(new_workspace_index).unwrap();
            assert_eq!(monitor.workspaces().len(), 2);
            // Check the per-monitor ring (focus_workspace was called directly on monitor,
            // not through wm, so the global wm.focused_desktop_idx is not updated here).
            assert_eq!(monitor.focused_workspace_idx(), 1);
        }

        {
            // focusing a workspace many indices ahead should create all workspaces
            // required along the way
            let monitor = wm.focused_monitor_mut().unwrap();
            monitor.sync_workspace_count(
                monitor
                    .workspaces()
                    .len()
                    .max((new_workspace_index + 2) + 1),
            );
            monitor.focus_workspace(new_workspace_index + 2).unwrap();
            assert_eq!(monitor.workspaces().len(), 4);
            assert_eq!(monitor.focused_workspace_idx(), 3);
        }

        // we should be able to successfully focus an existing workspace too
        wm.focus_workspace(0).unwrap();
        assert_eq!(wm.focused_workspace_idx().unwrap(), 0);
    }

    #[test]
    fn test_remove_focused_workspace() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let before = wm.monitors.clone();
        assert!(wm.remove_focused_workspace().is_none());
        assert_eq!(wm.monitors, before);
    }

    #[test]
    fn test_set_workspace_name() {
        let (mut wm, _test_context) = setup_window_manager();

        let m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // a new monitor should have a single workspace
        assert_eq!(m.workspaces().len(), 1);

        // create a new workspace
        let new_workspace_index = m.new_workspace_idx();
        assert_eq!(new_workspace_index, 1);

        // add the monitor to the window manager
        wm.monitors_mut().push_back(m);

        {
            // focusing a workspace which doesn't yet exist should create it
            let monitor = wm.focused_monitor_mut().unwrap();
            monitor.sync_workspace_count(monitor.workspaces().len().max((new_workspace_index) + 1));
            monitor.focus_workspace(new_workspace_index).unwrap();
            assert_eq!(monitor.workspaces().len(), 2);
            // Verify the per-monitor ring index (called directly on monitor).
            assert_eq!(monitor.focused_workspace_idx(), 1);
        }

        // set the name of the first workspace
        wm.set_workspace_name(0, 0, "workspace1".to_string())
            .unwrap();

        // monitor_workspace_index_by_name should return the index of the workspace with the name "workspace1"
        let workspace_index = wm.monitor_workspace_index_by_name("workspace1").unwrap();

        // workspace index 0 should now have the name "workspace1"
        assert_eq!(workspace_index.1, 0);
    }

    #[test]
    fn test_switch_focus_monitors() {
        let (mut wm, _test_context) = setup_window_manager();

        {
            // Create a first monitor
            let m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // monitor should have a single workspace
            assert_eq!(m.workspaces().len(), 1);

            // add the monitor to the window manager
            wm.monitors_mut().push_back(m);
        }
        assert_eq!(wm.monitors().len(), 1);

        {
            // Create a second monitor
            let m = monitor::new(
                1,
                Rect::default(),
                Rect::default(),
                "TestMonitor2".to_string(),
                "TestDevice2".to_string(),
                "TestDeviceID2".to_string(),
                Some("TestMonitorID2".to_string()),
            );

            // monitor should have a single workspace
            assert_eq!(m.workspaces().len(), 1);

            // add the monitor to the window manager
            wm.monitors_mut().push_back(m);
        }
        assert_eq!(wm.monitors().len(), 2);

        {
            // Create a third monitor
            let m = monitor::new(
                2,
                Rect::default(),
                Rect::default(),
                "TestMonitor3".to_string(),
                "TestDevice3".to_string(),
                "TestDeviceID3".to_string(),
                Some("TestMonitorID3".to_string()),
            );

            // monitor should have a single workspace
            assert_eq!(m.workspaces().len(), 1);

            // add the monitor to the window manager
            wm.monitors_mut().push_back(m);
        }
        assert_eq!(wm.monitors().len(), 3);

        {
            // Set the first monitor as focused and check if it is focused
            wm.focus_monitor(0).unwrap();
            let current_monitor_idx = wm.monitors.focused_idx();
            assert_eq!(current_monitor_idx, 0);
        }

        {
            // Set the second monitor as focused and check if it is focused
            wm.focus_monitor(1).unwrap();
            let current_monitor_idx = wm.monitors.focused_idx();
            assert_eq!(current_monitor_idx, 1);
        }

        {
            // Set the third monitor as focused and check if it is focused
            wm.focus_monitor(2).unwrap();
            let current_monitor_idx = wm.monitors.focused_idx();
            assert_eq!(current_monitor_idx, 2);
        }

        // Switch back to the first monitor
        wm.focus_monitor(0).unwrap();
        let current_monitor_idx = wm.monitors.focused_idx();
        assert_eq!(current_monitor_idx, 0);
    }

    #[test]
    fn test_switch_focus_to_nonexistent_monitor() {
        let (mut wm, _test_context) = setup_window_manager();

        {
            // Create a first monitor
            let m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // monitor should have a single workspace
            assert_eq!(m.workspaces().len(), 1);

            // add the monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should have 1 monitor and the monitor index should be 0
        assert_eq!(wm.monitors().len(), 1);
        assert_eq!(wm.focused_monitor_idx(), 0);

        // Should receive an error when trying to focus a non-existent monitor
        let result = wm.focus_monitor(1);
        assert!(
            result.is_err(),
            "Expected an error when focusing a non-existent monitor"
        );

        // Should still be focused on the first monitor
        assert_eq!(wm.focused_monitor_idx(), 0);
    }

    #[test]
    fn test_focused_monitor_size() {
        let (mut wm, _test_context) = setup_window_manager();

        {
            // Create a first monitor
            let m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // monitor should have a single workspace
            assert_eq!(m.workspaces().len(), 1);

            // add the monitor to the window manager
            wm.monitors_mut().push_back(m);
        }
        assert_eq!(wm.monitors().len(), 1);

        {
            // Set the first monitor as focused and check if it is focused
            wm.focus_monitor(0).unwrap();
            let current_monitor_size = wm.focused_monitor_size().unwrap();
            assert_eq!(current_monitor_size, Rect::default());
        }
    }

    #[test]
    fn test_focus_container_in_cycle_direction() {
        let (mut wm, _test_context) = setup_window_manager();

        // Create a monitor
        let mut m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "TestMonitor".to_string(),
            "TestDevice".to_string(),
            "TestDeviceID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let workspace = m.focused_workspace_mut().unwrap();
        workspace.layer = WorkspaceLayer::Tiling;

        for i in 0..4 {
            let mut container = Container::default();
            container.windows_mut().push_back(Window::from(i));
            workspace.add_container_to_back(container);
        }
        assert_eq!(workspace.containers().len(), 4);

        workspace.focus_container(0);

        // add the monitor to the window manager
        wm.monitors_mut().push_back(m);

        // container focus should be on the second container
        wm.focus_container_in_cycle_direction(CycleDirection::Next)
            .ok();
        assert_eq!(wm.focused_container_idx().unwrap(), 1);

        // container focus should be on the third container
        wm.focus_container_in_cycle_direction(CycleDirection::Next)
            .ok();
        assert_eq!(wm.focused_container_idx().unwrap(), 2);

        // container focus should be on the second container
        wm.focus_container_in_cycle_direction(CycleDirection::Previous)
            .ok();
        assert_eq!(wm.focused_container_idx().unwrap(), 1);

        // container focus should be on the first container
        wm.focus_container_in_cycle_direction(CycleDirection::Previous)
            .ok();
        assert_eq!(wm.focused_container_idx().unwrap(), 0);
    }

    #[test]
    fn test_transfer_window() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(0));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        {
            // Create a second monitor
            let mut m = monitor::new(
                1,
                Rect::default(),
                Rect::default(),
                "TestMonitor2".to_string(),
                "TestDevice2".to_string(),
                "TestDeviceID2".to_string(),
                Some("TestMonitorID2".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(1));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        // Should contain 2 monitors
        assert_eq!(wm.monitors().len(), 2);

        {
            // Monitor 0, Workspace 0, Window 0
            let origin = (0, 0, 0);

            // Monitor 1, Workspace 0, Window 0
            let target = (1, 0, 0);

            // Transfer the window from monitor 0 to monitor 1
            wm.transfer_window(origin, target).unwrap();

            // Monitor 1 should contain 0 containers
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 0);

            // Monitor 2 should contain 2 containers
            wm.focus_monitor(1).unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 2);
        }

        {
            // Monitor 1, Workspace 0, Window 0
            let origin = (1, 0, 0);

            // Monitor 0, Workspace 0, Window 0
            let target = (0, 0, 0);

            // Transfer the window from monitor 1 back to monitor 0
            wm.transfer_window(origin, target).unwrap();

            // Monitor 2 should contain 1 containers
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 1);

            // Monitor 1 should contain 1 containers
            wm.focus_monitor(0).unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 1);
        }
    }

    #[test]
    fn test_transfer_window_to_nonexistent_monitor() {
        // NOTE: transfer_window is primarily used when a window is being dragged by a mouse. The
        // transfer_window function does return an error when the target monitor doesn't exist but
        // there is a bug where the window isn't in the container after the window fails to
        // transfer. The test will test for the result of the transfer_window function but not if
        // the window is in the container after the transfer fails.

        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(0));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        {
            // Monitor 0, Workspace 0, Window 0
            let origin = (0, 0, 0);

            // Monitor 1, Workspace 0, Window 0
            //
            let target = (1, 0, 0);

            // Attempt to transfer the window from monitor 0 to a non-existent monitor
            let result = wm.transfer_window(origin, target);

            // Result should be an error since the monitor doesn't exist
            assert!(
                result.is_err(),
                "Expected an error when transferring to a non-existent monitor"
            );

            assert_eq!(wm.focused_container_idx().unwrap(), 0);
            assert_eq!(wm.focused_workspace_idx().unwrap(), 0);
        }
    }

    #[test]
    fn test_transfer_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(0));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        {
            // Create a second monitor
            let mut m = monitor::new(
                1,
                Rect::default(),
                Rect::default(),
                "TestMonitor2".to_string(),
                "TestDevice2".to_string(),
                "TestDeviceID2".to_string(),
                Some("TestMonitorID2".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(1));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        // Should contain 2 monitors
        assert_eq!(wm.monitors().len(), 2);

        {
            // Monitor 0, Workspace 0, Window 0
            let origin = (0, 0, 0);

            // Monitor 1, Workspace 0, Window 0
            let target = (1, 0, 0);

            // Transfer the window from monitor 0 to monitor 1
            wm.transfer_container(origin, target).unwrap();

            // Monitor 1 should contain 0 containers
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 0);

            // Monitor 2 should contain 2 containers
            wm.focus_monitor(1).unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 2);
        }

        {
            // Monitor 1, Workspace 0, Window 0
            let origin = (1, 0, 0);

            // Monitor 0, Workspace 0, Window 0
            let target = (0, 0, 0);

            // Transfer the window from monitor 1 back to monitor 0
            wm.transfer_container(origin, target).unwrap();

            // Monitor 2 should contain 1 containers
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 1);

            // Monitor 1 should contain 1 containers
            wm.focus_monitor(0).unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 1);
        }
    }

    #[test]
    fn test_remove_window_from_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor1".to_string(),
                "TestDevice1".to_string(),
                "TestDeviceID1".to_string(),
                Some("TestMonitorID1".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }
            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Focus last window
            container.focus_window(2);

            // Should be focused on the 2nd window
            assert_eq!(container.focused_window_idx(), 2);

            // Add the container to a workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Remove the focused window from the container
        wm.remove_window_from_container().ok();

        {
            // Should have 2 containers in the workspace
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 2);

            // Should contain 1 window in the new container
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.windows().len(), 1);
        }

        {
            // Switch to the old container
            let workspace = wm.focused_workspace_mut().unwrap();
            workspace.focus_container(0);

            // Should contain 2 windows in the old container
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.windows().len(), 2);
        }
    }

    #[test]
    fn test_remove_nonexistent_window_from_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor1".to_string(),
                "TestDevice1".to_string(),
                "TestDeviceID1".to_string(),
                Some("TestMonitorID1".to_string()),
            );

            // Create a container
            let container = Container::default();

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 0);

            // Add the container to a workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should receive an error when trying to remove a window from an empty container
        let result = wm.remove_window_from_container();
        assert!(
            result.is_err(),
            "Expected an error when trying to remove a window from an empty container"
        );
    }

    #[test]
    fn cycle_container_window_in_direction() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            let workspace = m.focused_workspace_mut().unwrap();

            {
                let mut container = Container::default();

                for i in 0..3 {
                    container.windows_mut().push_back(Window::from(i));
                }

                // Should have 3 windows in the container
                assert_eq!(container.windows().len(), 3);

                // Add container to workspace
                workspace.add_container_to_back(container);
            }

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Cycle to the next window
        wm.cycle_container_window_in_direction(CycleDirection::Next)
            .ok();

        {
            // Should be on Window 1
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 1);
        }

        // Cycle to the next window
        wm.cycle_container_window_in_direction(CycleDirection::Next)
            .ok();

        {
            // Should be on Window 2
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 2);
        }

        // Cycle to the previous window
        wm.cycle_container_window_in_direction(CycleDirection::Previous)
            .ok();

        {
            // Should be on Window 1
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 1);
        }
    }

    #[test]
    fn test_cycle_nonexistent_windows() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor1".to_string(),
                "TestDevice1".to_string(),
                "TestDeviceID1".to_string(),
                Some("TestMonitorID1".to_string()),
            );

            // Create a container
            let container = Container::default();

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 0);

            // Add the container to a workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should return an error when trying to cycle through windows in an empty container
        let result = wm.cycle_container_window_in_direction(CycleDirection::Next);
        assert!(
            result.is_err(),
            "Expected an error when cycling through windows in an empty container"
        );
    }

    #[test]
    fn test_cycle_container_window_index_in_direction() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            let workspace = m.focused_workspace_mut().unwrap();

            {
                let mut container = Container::default();

                for i in 0..3 {
                    container.windows_mut().push_back(Window::from(i));
                }

                // Should have 3 windows in the container
                assert_eq!(container.windows().len(), 3);

                // Add container to workspace
                workspace.add_container_to_back(container);
            }

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Cycle to the next window
        wm.cycle_container_window_index_in_direction(CycleDirection::Next)
            .ok();

        {
            // Should be on Window 1
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 1);
        }

        // Cycle to the next window
        wm.cycle_container_window_index_in_direction(CycleDirection::Next)
            .ok();

        {
            // Should be on Window 2
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 2);
        }

        // Cycle to the Previous window
        wm.cycle_container_window_index_in_direction(CycleDirection::Previous)
            .ok();

        {
            // Should be on Window 1
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.focused_window_idx(), 1);
        }
    }

    #[test]
    fn test_swap_containers() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        {
            // Create a second monitor
            let mut m = monitor::new(
                1,
                Rect::default(),
                Rect::default(),
                "TestMonitor2".to_string(),
                "TestDevice2".to_string(),
                "TestDeviceID2".to_string(),
                Some("TestMonitorID2".to_string()),
            );

            // Create a container
            let workspace = m.focused_workspace_mut().unwrap();
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(1));
            workspace.add_container_to_back(container);

            // Should contain 1 container
            assert_eq!(workspace.containers().len(), 1);

            wm.monitors_mut().push_back(m);
        }

        // Should contain 2 monitors
        assert_eq!(wm.monitors().len(), 2);

        // Monitor 0, Workspace 0, Window 0
        let origin = (0, 0, 0);

        // Monitor 1, Workspace 0, Window 0
        let target = (1, 0, 0);

        wm.swap_containers(origin, target).unwrap();

        {
            // Monitor 0 Workspace 0 container 0 should contain 1 container
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.windows().len(), 1);
        }

        wm.focus_monitor(1).unwrap();

        {
            // Monitor 1 Workspace 0 container 0 should contain 3 containers
            let workspace = wm.focused_workspace_mut().unwrap();
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.windows().len(), 3);
        }
    }

    #[test]
    fn test_swap_container_with_nonexistent_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a first monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Monitor 0, Workspace 0, Window 0
        let origin = (0, 0, 0);

        // Monitor 1, Workspace 0, Window 0
        let target = (0, 3, 0);

        // Should be focused on the first container
        assert_eq!(wm.focused_container_idx().unwrap(), 0);

        // Should return an error since there is only one container in the workspace
        let result = wm.swap_containers(origin, target);
        assert!(
            result.is_err(),
            "Expected an error when swapping with a non-existent container"
        );

        // Should still be focused on the first container
        assert_eq!(wm.focused_container_idx().unwrap(), 0);

        {
            // Should still have 1 container in the workspace
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.containers().len(), 1);

            // Container should still have 3 windows
            let container = workspace.focused_container_mut().unwrap();
            assert_eq!(container.windows().len(), 3);
        }
    }

    #[test]
    fn test_swap_monitor_workspaces() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let mut second = wm.monitors()[0].clone();
        second.id = 1;
        wm.monitors_mut().push_back(second);
        let before = wm.monitors.clone();
        assert!(wm.swap_monitor_workspaces(0, 1).is_err());
        assert_eq!(wm.monitors, before);
    }

    #[test]
    fn test_swap_workspace_with_nonexistent_monitor() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add another workspace
            let new_workspace_index = m.new_workspace_idx();
            m.sync_workspace_count(m.workspaces().len().max((new_workspace_index) + 1));
            m.focus_workspace(new_workspace_index).unwrap();

            // Should have 2 workspaces
            assert_eq!(m.workspaces().len(), 2);

            // Add monitor to window manager
            wm.monitors_mut().push_back(m);
        }

        // Should be an error since Monitor 1 does not exist
        let result = wm.swap_monitor_workspaces(1, 0);
        assert!(
            result.is_err(),
            "Expected an error when swapping with a non-existent monitor"
        );

        {
            // Should still have 2 workspaces in Monitor 0
            let monitor = wm.monitors().front().unwrap();
            let workspaces = monitor.workspaces();
            assert_eq!(
                workspaces.len(),
                2,
                "Expected 2 workspaces after swap attempt"
            );
            assert_eq!(wm.focused_monitor_idx(), 0);
        }
    }

    #[test]
    fn test_move_workspace_to_monitor() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add another workspace
            let new_workspace_index = m.new_workspace_idx();
            m.sync_workspace_count(m.workspaces().len().max((new_workspace_index) + 1));
            m.focus_workspace(new_workspace_index).unwrap();

            // Should have 2 workspaces
            assert_eq!(m.workspaces().len(), 2);

            // Add monitor to window manager
            wm.monitors_mut().push_back(m);
        }

        {
            let m = monitor::new(
                1,
                Rect::default(),
                Rect::default(),
                "TestMonitor2".to_string(),
                "TestDevice2".to_string(),
                "TestDeviceID2".to_string(),
                Some("TestMonitorID2".to_string()),
            );

            // Should contain 1 workspace
            assert_eq!(m.workspaces().len(), 1);

            // Add monitor to workspace
            wm.monitors_mut().push_back(m);
        }

        // Should contain 2 monitors
        assert_eq!(wm.monitors().len(), 2);

        // Moving a workspace to another monitor is not supported with native desktops
        assert!(wm.move_workspace_to_monitor(1).is_err());
    }

    #[test]
    fn test_move_workspace_to_nonexistent_monitor() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add another workspace
            let new_workspace_index = m.new_workspace_idx();
            m.sync_workspace_count(m.workspaces().len().max((new_workspace_index) + 1));
            m.focus_workspace(new_workspace_index).unwrap();

            // Should have 2 workspaces
            assert_eq!(m.workspaces().len(), 2);

            // Add monitor to window manager
            wm.monitors_mut().push_back(m);
        }

        // Attempt to move a workspace to a non-existent monitor
        let result = wm.move_workspace_to_monitor(1);

        // Should be an error since Monitor 1 does not exist
        assert!(
            result.is_err(),
            "Expected an error when moving to a non-existent monitor"
        );
    }

    #[test]
    fn test_toggle_tiling() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Set Workspace Layer to Tiling
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.layer = WorkspaceLayer::Tiling;

            // Tiling state should be true
            assert!(workspace.tile);

            // Add monitor to workspace
            wm.monitors_mut().push_back(m);
        }

        {
            // Tiling state should be false
            wm.toggle_tiling().unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert!(!workspace.tile);
        }

        {
            // Tiling state should be true
            wm.toggle_tiling().unwrap();
            let workspace = wm.focused_workspace_mut().unwrap();
            assert!(workspace.tile);
        }
    }

    #[test]
    fn test_toggle_lock() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Add monitor with default workspace to
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            let workspace = m.focused_workspace_mut().unwrap();

            // Create containers to add to the workspace
            for _ in 0..3 {
                let container = Container::default();
                workspace.add_container_to_back(container);
            }

            wm.monitors_mut().push_back(m);
        }

        {
            // Ensure container 2 is not locked
            let workspace = wm.focused_workspace_mut().unwrap();
            assert_eq!(workspace.focused_container_idx(), 2);
            assert!(!workspace.focused_container().unwrap().locked);
        }

        // Toggle lock on focused container
        wm.toggle_lock().unwrap();

        {
            // Ensure container 2 is locked
            let workspace = wm.focused_workspace_mut().unwrap();
            assert!(workspace.focused_container().unwrap().locked);
        }

        // Toggle lock on focused container
        wm.toggle_lock().unwrap();

        {
            // Ensure container 2 is not locked
            let workspace = wm.focused_workspace_mut().unwrap();
            assert!(!workspace.focused_container().unwrap().locked);
        }
    }

    #[test]
    fn test_float_window() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Add focused window to floating window list
        wm.float_window().ok();

        {
            let workspace = wm.focused_workspace().unwrap();
            let floating_windows = workspace.floating_windows();
            let container = workspace.focused_container().unwrap();

            // Hwnd 0 should be added to floating_windows
            assert_eq!(floating_windows[0].hwnd, 0);

            // Should have a length of 1
            assert_eq!(floating_windows.len(), 1);

            // Should have 2 windows in the container
            assert_eq!(container.windows().len(), 2);

            // Should be focused on window 1
            assert_eq!(container.focused_window(), Some(&Window { hwnd: 1 }));
        }

        // Add focused window to floating window list
        wm.float_window().ok();

        {
            let workspace = wm.focused_workspace().unwrap();
            let floating_windows = workspace.floating_windows();
            let container = workspace.focused_container().unwrap();

            // Hwnd 1 should be added to floating_windows
            assert_eq!(floating_windows[1].hwnd, 1);

            // Should have a length of 2
            assert_eq!(floating_windows.len(), 2);

            // Should have 1 window in the container
            assert_eq!(container.windows().len(), 1);

            // Should be focused on window 2
            assert_eq!(container.focused_window(), Some(&Window { hwnd: 2 }));
        }
    }

    #[test]
    fn test_float_nonexistent_window() {
        let (mut wm, _context) = setup_window_manager();

        {
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add another workspace
            let new_workspace_index = m.new_workspace_idx();
            m.sync_workspace_count(m.workspaces().len().max((new_workspace_index) + 1));
            m.focus_workspace(new_workspace_index).unwrap();

            // Should have 2 workspaces
            assert_eq!(m.workspaces().len(), 2);

            // Add monitor to window manager
            wm.monitors_mut().push_back(m);
        }

        // Should return an error when trying to float a non-existent window
        let result = wm.float_window();
        assert!(
            result.is_err(),
            "Expected an error when trying to float a non-existent window"
        );
    }

    #[test]
    fn test_maximize_and_unmaximize_window() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        {
            // No windows should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, None);
        }

        // Maximize the focused window
        wm.maximize_window().ok();

        {
            // Window 0 should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, Some(Window::from(0)));
        }

        wm.unmaximize_window().ok();

        {
            // No windows should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, None);
        }

        // Focus container at index 1
        wm.focused_workspace_mut().unwrap().focus_container(1);

        {
            // Focus the window at index 1
            let container = wm.focused_container_mut().unwrap();
            container.focus_window(1);
        }

        // Maximize the focused window
        wm.maximize_window().ok();

        {
            // Window 2 should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, Some(Window::from(2)));
        }

        wm.unmaximize_window().ok();

        {
            // No windows should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, None);
        }
    }

    #[test]
    fn test_toggle_maximize() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add three windows to the container
            for i in 0..3 {
                container.windows_mut().push_back(Window::from(i));
            }

            // Should have 3 windows in the container
            assert_eq!(container.windows().len(), 3);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Toggle maximize on
        wm.toggle_maximize().ok();

        {
            // Window 0 should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, Some(Window::from(0)));
        }

        // Toggle maximize off
        wm.toggle_maximize().ok();

        {
            // No windows should be maximized
            let workspace = wm.focused_workspace().unwrap();
            let maximized_window = workspace.maximized_window;
            assert_eq!(maximized_window, None);
        }
    }

    #[test]
    fn test_toggle_maximize_nonexistent_window() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let container = Container::default();

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should return an error when trying to toggle maximize on a non-existent window
        let result = wm.toggle_maximize();
        assert!(
            result.is_err(),
            "Expected an error when trying to toggle maximize on a non-existent window"
        );
    }

    #[test]
    fn test_monocle_on_and_monocle_off() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(1));

            // Should have 1 window in the container
            assert_eq!(container.windows().len(), 1);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Move container to monocle container
        wm.monocle_on().ok();

        {
            // Container should be a monocle container
            let monocle_container = wm
                .focused_workspace()
                .unwrap()
                .monocle_container
                .as_ref()
                .unwrap();
            assert_eq!(monocle_container.windows().len(), 1);
            assert_eq!(monocle_container.windows()[0].hwnd, 1);
        }

        {
            // Should not have any containers
            let container = wm.focused_workspace().unwrap();
            assert_eq!(container.containers().len(), 0);
        }

        // Move monocle container to regular container
        wm.monocle_off().ok();

        {
            // Should have 1 container in the workspace
            let container = wm.focused_workspace().unwrap();
            assert_eq!(container.containers().len(), 1);
            assert_eq!(container.containers()[0].windows()[0].hwnd, 1);
        }

        {
            // No windows should be in the monocle container
            let monocle_container = &wm.focused_workspace().unwrap().monocle_container;
            assert_eq!(*monocle_container, None);
        }
    }

    #[test]
    fn test_monocle_on_and_off_nonexistent_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should return an error when trying to move a non-existent container to monocle
        let result = wm.monocle_on();
        assert!(
            result.is_err(),
            "Expected an error when trying to move a non-existent container to monocle"
        );

        // Should return an error when trying to restore a non-existent container from monocle
        let result = wm.monocle_off();
        assert!(
            result.is_err(),
            "Expected an error when trying to restore a non-existent container from monocle"
        );
    }

    #[test]
    fn test_toggle_monocle() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let mut m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Create a container
            let mut container = Container::default();

            // Add a window to the container
            container.windows_mut().push_back(Window::from(1));

            // Should have 1 window in the container
            assert_eq!(container.windows().len(), 1);

            // Add the container to the workspace
            let workspace = m.focused_workspace_mut().unwrap();
            workspace.add_container_to_back(container);

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Toggle monocle on
        wm.toggle_monocle().ok();

        {
            // Container should be a monocle container
            let monocle_container = wm
                .focused_workspace()
                .unwrap()
                .monocle_container
                .as_ref()
                .unwrap();
            assert_eq!(monocle_container.windows().len(), 1);
            assert_eq!(monocle_container.windows()[0].hwnd, 1);
        }

        {
            // Should not have any containers
            let container = wm.focused_workspace().unwrap();
            assert_eq!(container.containers().len(), 0);
        }

        // Toggle monocle off
        wm.toggle_monocle().ok();

        {
            // Should have 1 container in the workspace
            let container = wm.focused_workspace().unwrap();
            assert_eq!(container.containers().len(), 1);
            assert_eq!(container.containers()[0].windows()[0].hwnd, 1);
        }

        {
            // No windows should be in the monocle container
            let monocle_container = &wm.focused_workspace().unwrap().monocle_container;
            assert_eq!(*monocle_container, None);
        }
    }

    #[test]
    fn test_toggle_monocle_nonexistent_container() {
        let (mut wm, _context) = setup_window_manager();

        {
            // Create a monitor
            let m = monitor::new(
                0,
                Rect::default(),
                Rect::default(),
                "TestMonitor".to_string(),
                "TestDevice".to_string(),
                "TestDeviceID".to_string(),
                Some("TestMonitorID".to_string()),
            );

            // Add monitor to the window manager
            wm.monitors_mut().push_back(m);
        }

        // Should return an error when trying to toggle monocle on a non-existent container
        let result = wm.toggle_monocle();
        assert!(
            result.is_err(),
            "Expected an error when trying to toggle monocle on a non-existent container"
        );
    }

    #[test]
    fn test_workspace_names_synced_from_backend() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);

        // Set names on the fake backend
        crate::virtual_desktop::DesktopBackend::set_desktop_name(&*fake, 0, "Workspace").unwrap();
        crate::virtual_desktop::DesktopBackend::set_desktop_name(&*fake, 1, "Workspace1").unwrap();

        // Sync desktop count should load names from backend
        wm.sync_desktop_count().unwrap();

        // Both monitors should have the workspace names from the backend
        for monitor in wm.monitors() {
            let workspaces = monitor.workspaces();
            assert_eq!(workspaces.len(), 2);
            assert_eq!(workspaces[0].name.as_deref(), Some("Workspace"));
            assert_eq!(workspaces[1].name.as_deref(), Some("Workspace1"));
        }

        // Global workspace_names should also be populated
        assert_eq!(wm.workspace_names.get(&0), Some(&"Workspace".to_string()));
        assert_eq!(wm.workspace_names.get(&1), Some(&"Workspace1".to_string()));
    }

    #[test]
    fn sync_desktop_count_keeps_monitors_uniform() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        wm.sync_desktop_count().unwrap();
        assert_eq!(wm.desktop_count, 3);
        for m in wm.monitors() {
            assert_eq!(m.workspaces().len(), 3);
        }
    }

    #[test]
    fn focus_desktop_moves_every_monitor_to_the_same_index() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        wm.focus_desktop(2).unwrap();
        assert_eq!(wm.focused_desktop_idx, 2);
        assert_eq!(fake.switch_calls(), 1);
        for m in wm.monitors() {
            assert_eq!(m.focused_workspace_idx(), 2);
            assert_eq!(m.workspaces().len(), 3);
        }
    }

    #[test]
    fn focus_desktop_rejects_out_of_range_index() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        assert!(wm.focus_desktop(3).is_err());
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn desktop_changed_updates_index_without_switching_back() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.set_current_desktop(1);
        wm.process_event(WindowManagerEvent::DesktopChanged { new: 1, old: 0 })
            .unwrap();
        assert_eq!(wm.focused_desktop_idx, 1);
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn desktop_moved_permutes_workspaces_on_every_monitor() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.move_desktop(0, 2);
        wm.process_event(WindowManagerEvent::DesktopMoved {
            desktop: 0,
            old: 0,
            new: 2,
        })
        .unwrap();
        for m in wm.monitors() {
            assert_eq!(m.workspaces().len(), 3);
        }
        assert_eq!(wm.focused_desktop_idx, 2);
    }

    #[test]
    fn window_born_on_other_desktop_is_tiled_without_stealing_focus() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.set_window_desktop(42, 2);
        wm.focus_desktop(0).unwrap();
        let idx = wm.desktop_idx_for_new_window(Window::from(42)).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(wm.focused_desktop_idx, 0);
    }

    #[test]
    fn move_container_moves_every_window_to_target_desktop() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        // Create a container with hwnds 10 and 11 on desktop 0
        let focused_workspace = wm.focused_workspace_mut().unwrap();
        focused_workspace.new_container_for_window(Window::from(10));
        focused_workspace
            .focused_container_mut()
            .unwrap()
            .add_window_without_visibility_changes(Window::from(11));
        wm.move_container_to_workspace(2, false, None)
            .unwrap_or_else(|e| panic!("move_container_to_workspace failed: {:?}", e));
        assert_eq!(fake.moved(), vec![(10, 2), (11, 2)]);
        assert_eq!(wm.focused_desktop_idx, 0);
        assert!(!wm.monitors()[0].workspaces()[0].contains_window(10));
        assert!(wm.monitors()[0].workspaces()[2].contains_window(10));
        assert_eq!(wm.known_hwnds.get(&11), Some(&(0, 2)));
    }

    #[test]
    fn desktop_move_follow_transfers_stack_before_switching() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        wm.focused_workspace_mut()
            .unwrap()
            .focused_container_mut()
            .unwrap()
            .add_window_without_visibility_changes(Window::from(11));
        wm.move_container_to_workspace(2, true, None).unwrap();
        assert_eq!(fake.moved(), vec![(10, 2), (11, 2)]);
        assert!(!wm.monitors()[0].workspaces()[0].contains_window(10));
        assert_eq!(
            wm.monitors()[0].workspaces()[2].containers()[0]
                .windows()
                .len(),
            2
        );
        assert_eq!(wm.known_hwnds.get(&11), Some(&(0, 2)));
        assert_eq!(wm.focused_desktop_idx, 2);
        assert_eq!(fake.switch_calls(), 1);
    }

    #[test]
    fn desktop_move_of_monocle_restores_source_stack_selection_and_floats() {
        for follow in [false, true] {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let api = Arc::new(crate::windows_api::FakeWindowApi {
                foreground_hwnd: 10,
                ..Default::default()
            });
            wm.window_api = api.clone();
            let source = wm.focused_workspace_mut().unwrap();
            source.new_container_for_window(Window::from(20));
            source
                .focused_container_mut()
                .unwrap()
                .add_window_without_visibility_changes(Window::from(21));
            source.new_container_for_window(Window::from(30));
            source.floating_windows_mut().push_back(Window::from(40));
            let mut monocle = crate::container::Container::default();
            monocle.add_window_without_visibility_changes(Window::from(10));
            source.monocle_container = Some(monocle);
            source.monocle_container_restore_idx = Some(0);

            wm.move_container_to_workspace(1, follow, None).unwrap();

            assert_eq!(fake.moved(), vec![(10, 1)]);
            assert_eq!(
                *api.effects.lock(),
                vec![("restore", 21), ("restore", 30), ("restore", 40)]
            );
            let source = &wm.monitors()[0].workspaces()[0];
            assert!(source.monocle_container.is_none());
            assert_eq!(source.monocle_container_restore_idx, None);
            assert!(source.contains_window(20));
            assert!(source.contains_window(21));
            assert!(!source.contains_window(10));
            assert_eq!(wm.known_hwnds.get(&10), Some(&(0, 1)));
            assert_eq!(wm.focused_desktop_idx, usize::from(follow));
            assert_eq!(fake.switch_calls(), usize::from(follow));
        }
    }

    #[test]
    fn desktop_move_selects_active_float_instead_of_stack() {
        for follow in [false, true] {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            wm.window_api = Arc::new(crate::windows_api::FakeWindowApi {
                foreground_hwnd: 20,
                ..Default::default()
            });
            wm.focused_workspace_mut()
                .unwrap()
                .new_container_for_window(Window::from(10));
            wm.focused_workspace_mut()
                .unwrap()
                .floating_windows_mut()
                .push_back(Window::from(20));
            wm.move_container_to_workspace(2, follow, None).unwrap();
            assert_eq!(fake.moved(), vec![(20, 2)]);
            assert!(wm.monitors()[0].workspaces()[0].contains_window(10));
            assert!(
                wm.monitors()[0].workspaces()[0]
                    .floating_windows()
                    .is_empty()
            );
            assert_eq!(
                wm.monitors()[0].workspaces()[2].floating_windows()[0].hwnd,
                20
            );
            assert_eq!(wm.known_hwnds.get(&20), Some(&(0, 2)));
            assert_eq!(wm.focused_desktop_idx, if follow { 2 } else { 0 });
            assert_eq!(fake.switch_calls(), usize::from(follow));
        }
    }

    #[test]
    fn desktop_move_invalid_or_same_destination_has_no_native_effects() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        assert!(wm.move_container_to_workspace(2, true, None).is_err());
        wm.move_container_to_workspace(0, true, None).unwrap();
        assert!(fake.moved().is_empty());
        assert_eq!(fake.switch_calls(), 0);
        assert!(wm.monitors()[0].workspaces()[0].contains_window(10));
    }

    #[test]
    fn desktop_move_partial_failure_reconciles_all_members_without_following() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        for hwnd in [11, 12] {
            wm.focused_workspace_mut()
                .unwrap()
                .focused_container_mut()
                .unwrap()
                .add_window_without_visibility_changes(Window::from(hwnd));
        }
        for hwnd in [10, 11, 12] {
            fake.set_window_desktop(hwnd, 0);
        }
        fake.fail_move_for(11);
        let error = wm.move_container_to_workspace(2, true, None).unwrap_err();
        assert!(error.to_string().contains("injected move failure"));
        assert_eq!(fake.moved(), vec![(10, 2)]);
        assert_eq!(wm.known_hwnds.get(&10), Some(&(0, 2)));
        assert_eq!(wm.known_hwnds.get(&11), Some(&(0, 0)));
        assert_eq!(wm.known_hwnds.get(&12), Some(&(0, 0)));
        assert_eq!(
            wm.monitors()[0].workspaces()[0].containers()[0]
                .windows()
                .len(),
            2
        );
        assert_eq!(wm.focused_desktop_idx, 0);
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn external_desktop_move_preserves_surviving_stack_selection() {
        for source_idx in [0, 1] {
            for monocle in [false, true] {
                for moved_hwnd in [10, 11] {
                    let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
                    let api = Arc::new(crate::windows_api::FakeWindowApi::default());
                    wm.window_api = api.clone();
                    let source = &mut wm.monitors_mut()[0].workspaces_mut()[source_idx];
                    source.new_container_for_window(Window::from(10));
                    for hwnd in [11, 12] {
                        source
                            .focused_container_mut()
                            .unwrap()
                            .add_window_without_visibility_changes(Window::from(hwnd));
                    }
                    if monocle {
                        source.monocle_container = source.remove_container_by_idx(0);
                        source.monocle_container_restore_idx = Some(0);
                    }
                    fake.set_window_desktop(moved_hwnd, 2);
                    wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: moved_hwnd })
                        .unwrap();
                    let source = &wm.monitors()[0].workspaces()[source_idx];
                    let stack = if monocle {
                        source.monocle_container.as_ref().unwrap()
                    } else {
                        &source.containers()[0]
                    };
                    assert_eq!(stack.focused_window().unwrap().hwnd, 12);
                    assert_eq!(stack.windows().len(), 2);
                    assert!(api.effects.lock().is_empty());
                    assert_eq!(wm.focused_desktop_idx, 0);
                    assert_eq!(fake.switch_calls(), 0);
                    assert!(fake.moved().is_empty());
                }
            }
        }
    }

    #[test]
    fn external_desktop_move_restores_only_newly_selected_stack_member() {
        for source_idx in [0, 1] {
            for monocle in [false, true] {
                for moved_hwnd in [10, 11] {
                    let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
                    let api = Arc::new(crate::windows_api::FakeWindowApi::default());
                    wm.window_api = api.clone();
                    let source = &mut wm.monitors_mut()[0].workspaces_mut()[source_idx];
                    source.new_container_for_window(Window::from(10));
                    source
                        .focused_container_mut()
                        .unwrap()
                        .add_window_without_visibility_changes(Window::from(11));
                    if monocle {
                        source.monocle_container = source.remove_container_by_idx(0);
                        source.monocle_container_restore_idx = Some(0);
                        source.new_container_for_window(Window::from(30));
                        source.floating_windows_mut().push_back(Window::from(40));
                    }
                    fake.set_window_desktop(moved_hwnd, 2);
                    for _ in 0..2 {
                        wm.process_event(WindowManagerEvent::WindowDesktopChanged {
                            hwnd: moved_hwnd,
                        })
                        .unwrap();
                    }
                    let expected = if moved_hwnd == 11 {
                        vec![("restore", 10)]
                    } else {
                        vec![]
                    };
                    assert_eq!(*api.effects.lock(), expected);
                    assert_eq!(wm.focused_desktop_idx, 0);
                    assert_eq!(fake.switch_calls(), 0);
                    assert!(fake.moved().is_empty());
                }
            }
        }
    }

    #[test]
    fn external_desktop_move_of_last_monocle_restores_stack_selection_and_floats() {
        for source_idx in [0, 1] {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            let api = Arc::new(crate::windows_api::FakeWindowApi::default());
            wm.window_api = api.clone();
            let source = &mut wm.monitors_mut()[0].workspaces_mut()[source_idx];
            source.new_container_for_window(Window::from(20));
            source
                .focused_container_mut()
                .unwrap()
                .add_window_without_visibility_changes(Window::from(21));
            source.new_container_for_window(Window::from(30));
            source.floating_windows_mut().push_back(Window::from(40));
            let mut monocle = crate::container::Container::default();
            monocle.add_window_without_visibility_changes(Window::from(10));
            source.monocle_container = Some(monocle);
            source.monocle_container_restore_idx = Some(0);
            fake.set_window_desktop(10, 2);
            for _ in 0..2 {
                wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 10 })
                    .unwrap();
            }
            assert_eq!(
                *api.effects.lock(),
                vec![("restore", 21), ("restore", 30), ("restore", 40)]
            );
            assert!(
                wm.monitors()[0].workspaces()[source_idx]
                    .monocle_container
                    .is_none()
            );
            assert_eq!(wm.known_hwnds.get(&10), Some(&(0, 2)));
            assert_eq!(wm.focused_desktop_idx, 0);
            assert_eq!(fake.switch_calls(), 0);
            assert!(fake.moved().is_empty());
        }
    }

    #[test]
    fn external_desktop_move_is_idempotent_and_preserves_monitor_and_focus() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let mut second = wm.monitors()[0].clone();
        second.id = 1;
        second.workspaces_mut()[0].new_container_for_window(Window::from(10));
        second.workspaces_mut()[0]
            .focused_container_mut()
            .unwrap()
            .add_window_without_visibility_changes(Window::from(11));
        wm.monitors_mut().push_back(second);
        wm.update_known_hwnds();
        fake.set_window_desktop(10, 2);
        for _ in 0..2 {
            wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 10 })
                .unwrap();
        }
        assert_eq!(wm.known_hwnds.get(&10), Some(&(1, 2)));
        assert_eq!(wm.window_desktops.get(&10), Some(&2));
        assert_eq!(wm.known_hwnds.get(&11), Some(&(1, 0)));
        assert_eq!(wm.monitors()[1].workspaces()[2].containers().len(), 1);
        assert_eq!(wm.focused_monitor_idx(), 0);
        assert_eq!(wm.focused_desktop_idx, 0);
        assert_eq!(fake.switch_calls(), 0);
        assert!(fake.moved().is_empty());
    }

    #[test]
    fn external_desktop_move_query_failure_preserves_management() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        wm.update_known_hwnds();
        assert!(
            wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 10 })
                .is_err()
        );
        assert_eq!(wm.known_hwnds.get(&10), Some(&(0, 0)));
        assert!(wm.monitors()[0].workspaces()[0].contains_window(10));
    }

    #[test]
    fn external_desktop_move_keeps_floating_window_floating() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .floating_windows_mut()
            .push_back(Window::from(20));
        wm.update_known_hwnds();
        fake.set_window_desktop(20, 1);
        wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 20 })
            .unwrap();
        assert!(
            wm.monitors()[0].workspaces()[0]
                .floating_windows()
                .is_empty()
        );
        assert_eq!(
            wm.monitors()[0].workspaces()[1].floating_windows()[0].hwnd,
            20
        );
        assert_eq!(wm.known_hwnds.get(&20), Some(&(0, 1)));
    }

    #[test]
    fn background_desktop_show_manage_uncloak_do_not_duplicate_or_steal_monitor_focus() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let mut second = wm.monitors()[0].clone();
        second.id = 7;
        wm.monitors_mut().push_back(second);
        wm.window_api = Arc::new(crate::windows_api::FakeWindowApi {
            monitor_handle: 7,
            manage_windows: true,
            ..Default::default()
        });
        fake.set_window_desktop(42, 2);
        for event in [
            WindowManagerEvent::Show(crate::winevent::WinEvent::ObjectShow, Window::from(42)),
            WindowManagerEvent::Manage(Window::from(42)),
            WindowManagerEvent::Uncloak(
                crate::winevent::WinEvent::ObjectUncloaked,
                Window::from(42),
            ),
        ] {
            wm.process_event(event).unwrap();
        }
        assert_eq!(wm.monitors()[1].workspaces()[2].containers().len(), 1);
        assert!(wm.monitors()[0].workspaces()[2].is_empty());
        assert_eq!(wm.known_hwnds.get(&42), Some(&(1, 2)));
        assert_eq!(crate::reaper::HWNDS_CACHE.lock().get(&42), Some(&(1, 2)));
        assert_eq!(wm.focused_monitor_idx(), 0);
        assert_eq!(wm.focused_desktop_idx, 0);
        assert_eq!(fake.switch_calls(), 0);
        assert!(fake.moved().is_empty());
    }

    #[test]
    fn background_desktop_respects_destination_float_override() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.monitors_mut()[0].workspaces_mut()[1].float_override = Some(true);
        wm.window_api = Arc::new(crate::windows_api::FakeWindowApi {
            manage_windows: true,
            ..Default::default()
        });
        fake.set_window_desktop(42, 1);
        wm.process_event(WindowManagerEvent::Manage(Window::from(42)))
            .unwrap();
        assert_eq!(
            wm.monitors()[0].workspaces()[1].floating_windows()[0].hwnd,
            42
        );
        assert!(wm.monitors()[0].workspaces()[1].containers().is_empty());
        assert_eq!(wm.focused_desktop_idx, 0);
    }

    #[test]
    fn desktop_move_rejects_maximized_before_native_side_effects() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut().unwrap().maximized_window = Some(Window::from(10));
        assert!(wm.move_container_to_workspace(1, true, None).is_err());
        assert!(fake.moved().is_empty());
        assert_eq!(fake.switch_calls(), 0);
        assert_eq!(
            wm.focused_workspace()
                .unwrap()
                .maximized_window
                .unwrap()
                .hwnd,
            10
        );
    }

    #[test]
    fn desktop_move_without_follow_keeps_a_valid_source_container_selection() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(11));
        wm.move_container_to_workspace(1, false, None).unwrap();
        assert_eq!(
            wm.focused_workspace()
                .unwrap()
                .focused_container()
                .unwrap()
                .focused_window()
                .unwrap()
                .hwnd,
            10
        );
        assert_eq!(wm.known_hwnds.get(&11), Some(&(0, 1)));
    }

    #[test]
    fn external_desktop_move_ignores_unmanaged_windows() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        // No native mapping: an unmanaged notification should require no query.
        wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 99 })
            .unwrap();
        assert!(wm.known_hwnds.is_empty());
        assert!(fake.moved().is_empty());
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn desktop_move_error_after_native_move_reconciles_the_failed_member() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        wm.focused_workspace_mut()
            .unwrap()
            .focused_container_mut()
            .unwrap()
            .add_window_without_visibility_changes(Window::from(11));
        fake.set_window_desktop(11, 0);
        fake.fail_after_move_for(10);
        assert!(wm.move_container_to_workspace(1, true, None).is_err());
        assert_eq!(wm.known_hwnds.get(&10), Some(&(0, 1)));
        assert_eq!(wm.known_hwnds.get(&11), Some(&(0, 0)));
        assert_eq!(fake.moved(), vec![(10, 1)]);
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn desktop_move_native_notifications_do_not_split_the_transferred_stack() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.focused_workspace_mut()
            .unwrap()
            .new_container_for_window(Window::from(10));
        wm.focused_workspace_mut()
            .unwrap()
            .focused_container_mut()
            .unwrap()
            .add_window_without_visibility_changes(Window::from(11));
        wm.move_container_to_workspace(1, false, None).unwrap();
        for hwnd in [10, 11, 10] {
            wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd })
                .unwrap();
        }
        assert_eq!(wm.monitors()[0].workspaces()[1].containers().len(), 1);
        assert_eq!(
            wm.monitors()[0].workspaces()[1].containers()[0]
                .windows()
                .len(),
            2
        );
        assert_eq!(fake.moved(), vec![(10, 1), (11, 1)]);
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn moving_a_workspace_to_another_monitor_is_rejected() {
        let _guard = crate::virtual_desktop::testing::InstalledBackendGuard::new();
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        assert!(wm.move_workspace_to_monitor(0).is_err());
    }

    #[test]
    fn workspace_name_is_written_to_the_native_desktop() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.set_workspace_name(0, 1, "web".to_string()).unwrap();
        assert_eq!(fake.names().get(&1).map(String::as_str), Some("web"));
        for m in wm.monitors() {
            assert_eq!(m.workspaces()[1].name.as_deref(), Some("web"));
        }
    }

    #[test]
    fn new_workspace_is_a_noop() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        wm.new_workspace().unwrap();
        assert_eq!(wm.desktop_count, 2);
        assert_eq!(
            crate::virtual_desktop::DesktopBackend::desktop_count(&*fake).unwrap(),
            2
        );
        assert_eq!(wm.monitors()[0].workspaces().len(), 2);
    }

    #[test]
    fn cloak_from_desktop_transition_does_not_trigger_reconciliation() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.set_window_desktop(42, 0);
        // window 42 lives on desktop 0; focus goes to desktop 1
        wm.focus_desktop(1).unwrap();
        assert!(wm.is_desktop_transition_cloak(Window::from(42)));
        crate::HIDDEN_HWNDS.lock().clear();
        wm.process_event(WindowManagerEvent::Cloak(
            crate::winevent::WinEvent::ObjectCloaked,
            Window::from(42),
        ))
        .unwrap();
        assert_eq!(wm.focused_desktop_idx, 1);
        assert!(!crate::HIDDEN_HWNDS.lock().contains(&42));
    }

    #[test]
    fn cloak_on_the_focused_desktop_is_still_processed() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.set_window_desktop(43, 1);
        wm.focus_desktop(1).unwrap();
        assert!(!wm.is_desktop_transition_cloak(Window::from(43)));
    }

    #[test]
    fn switching_desktop_redraws_borders_once() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let before =
            crate::border_manager::BORDER_REDRAWS.load(std::sync::atomic::Ordering::SeqCst);
        wm.focus_desktop(1).unwrap();
        assert_eq!(
            crate::border_manager::BORDER_REDRAWS.load(std::sync::atomic::Ordering::SeqCst),
            before + 1
        );
    }

    #[test]
    fn window_on_other_desktop_is_not_reaped() {
        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        fake.set_window_desktop(99, 2);
        wm.focus_desktop(0).unwrap();
        assert!(wm.is_window_on_other_desktop(99));
    }

    #[test]
    fn load_focused_workspace_does_not_hide_other_workspaces() {
        let mut m = crate::monitor::Monitor::new(
            0,
            crate::core::Rect::default(),
            crate::core::Rect::default(),
            "M".into(),
            "D".into(),
            "DID".into(),
            Some("SN".into()),
        );
        m.sync_workspace_count(2);
        crate::HIDDEN_HWNDS.lock().clear();
        m.load_focused_workspace(false).unwrap();
        assert!(crate::HIDDEN_HWNDS.lock().is_empty());
    }

    #[test]
    fn applying_state_with_fewer_workspaces_than_desktops_keeps_all_desktops() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(3);
        let mut state = crate::State::from(&wm);
        state.monitors.elements_mut()[0]
            .workspaces
            .elements_mut()
            .truncate(2);
        wm.apply_state(state).unwrap();
        assert_eq!(wm.monitors()[0].workspaces().len(), 3);
    }

    #[test]
    fn applying_state_with_more_workspaces_than_desktops_does_not_lose_windows() {
        let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
        let mut state = crate::State::from(&wm);
        while state.monitors.elements()[0].workspaces().len() < 3 {
            state.monitors.elements_mut()[0]
                .workspaces
                .elements_mut()
                .push_back(crate::workspace::Workspace::default());
        }
        wm.apply_state(state).unwrap();
        assert_eq!(wm.monitors()[0].workspaces().len(), 2);
    }

    mod native_desktop_regressions {
        use super::*;
        use crate::core::config_generation::{
            IdWithIdentifier, MatchingRule, MatchingStrategy, WorkspaceMatchingRule,
        };
        use crate::windows_api::{FakeWindowApi, WindowIdentifiers};
        use crate::winevent::WinEvent;

        fn events(hwnd: isize) -> [WindowManagerEvent; 3] {
            [
                WindowManagerEvent::Show(WinEvent::ObjectShow, Window::from(hwnd)),
                WindowManagerEvent::Manage(Window::from(hwnd)),
                WindowManagerEvent::Uncloak(WinEvent::ObjectUncloaked, Window::from(hwnd)),
            ]
        }

        fn api(foreground: isize, monitor: isize) -> FakeWindowApi {
            FakeWindowApi {
                foreground_hwnd: foreground,
                monitor_handle: monitor,
                manage_windows: true,
                identifiers: Some(WindowIdentifiers {
                    exe: "native-test.exe".into(),
                    title: "Native test".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        fn rule(m: usize, d: usize, initial_only: bool) -> WorkspaceMatchingRule {
            WorkspaceMatchingRule {
                monitor_index: m,
                workspace_index: d,
                initial_only,
                matching_rule: MatchingRule::Simple(IdWithIdentifier {
                    kind: crate::core::ApplicationIdentifier::Exe,
                    id: "native-test.exe".into(),
                    matching_strategy: Some(MatchingStrategy::Equals),
                }),
            }
        }

        fn second_monitor(wm: &mut WindowManager) {
            let mut monitor = wm.monitors()[0].clone();
            monitor.id = 7;
            wm.monitors_mut().push_back(monitor);
        }

        fn assert_caches(wm: &WindowManager, hwnd: isize, pair: (usize, usize)) {
            assert_eq!(wm.known_hwnds.get(&hwnd), Some(&pair));
            assert_eq!(wm.window_desktops.get(&hwnd), Some(&pair.1));
            assert_eq!(crate::reaper::HWNDS_CACHE.lock().get(&hwnd), Some(&pair));
            assert_eq!(
                wm.monitors()
                    .iter()
                    .flat_map(|m| m.workspaces())
                    .map(|ws| {
                        ws.containers()
                            .iter()
                            .map(|c| c.windows().iter().filter(|w| w.hwnd == hwnd).count())
                            .sum::<usize>()
                            + ws.floating_windows()
                                .iter()
                                .filter(|w| w.hwnd == hwnd)
                                .count()
                            + usize::from(ws.maximized_window.is_some_and(|w| w.hwnd == hwnd))
                            + ws.monocle_container.as_ref().map_or(0, |c| {
                                c.windows().iter().filter(|w| w.hwnd == hwnd).count()
                            })
                    })
                    .sum::<usize>(),
                1
            );
        }

        #[test]
        fn desktop_identity_reorders_all_positions_and_focus_without_reapplying_delayed_events() {
            use crate::virtual_desktop::DesktopBackend;
            for old in 0..4 {
                for new in 0..4 {
                    if old == new {
                        continue;
                    }
                    for focused in 0..4 {
                        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(4);
                        second_monitor(&mut wm);
                        fake.set_current_desktop(focused);
                        wm.sync_desktop_count().unwrap();
                        let ids = wm.desktop_ids.clone();
                        for m in 0..2 {
                            for d in 0..4 {
                                let hwnd = 1000 + (m * 100 + d) as isize;
                                wm.monitors_mut()[m].workspaces_mut()[d].workspace_padding =
                                    Some((10 * m + d) as i32);
                                wm.monitors_mut()[m].workspaces_mut()[d]
                                    .new_container_for_window(Window::from(hwnd));
                                fake.set_window_desktop(hwnd, d);
                            }
                        }
                        wm.update_known_hwnds();
                        fake.move_desktop(old, new);
                        for _ in 0..2 {
                            wm.process_event(WindowManagerEvent::DesktopMoved {
                                desktop: new,
                                old,
                                new,
                            })
                            .unwrap();
                        }
                        // A stale focus notification cannot override the OS snapshot.
                        wm.process_event(WindowManagerEvent::DesktopChanged {
                            new: usize::MAX,
                            old: new,
                        })
                        .unwrap();
                        let snapshot = fake.snapshot().unwrap();
                        let native_focus = snapshot
                            .desktops
                            .iter()
                            .position(|d| d.id == snapshot.current)
                            .unwrap();
                        assert_eq!(wm.focused_desktop_idx, native_focus);
                        for (d, desktop) in snapshot.desktops.iter().enumerate() {
                            let original = ids.iter().position(|id| *id == desktop.id).unwrap();
                            for m in 0..2 {
                                assert_eq!(
                                    wm.monitors()[m].workspaces()[d].workspace_padding,
                                    Some((10 * m + original) as i32)
                                );
                                assert_caches(&wm, 1000 + (m * 100 + original) as isize, (m, d));
                                assert_eq!(wm.monitors()[m].focused_workspace_idx(), native_focus);
                            }
                        }
                        assert!(fake.moved().is_empty());
                        assert_eq!(fake.switch_calls(), 0);
                    }
                }
            }
        }

        #[test]
        fn desktop_identity_creation_at_each_position_preserves_survivors() {
            for inserted in 0..=3 {
                let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
                second_monitor(&mut wm);
                fake.set_current_desktop(1);
                wm.sync_desktop_count().unwrap();
                for m in 0..2 {
                    for d in 0..3 {
                        wm.monitors_mut()[m].workspaces_mut()[d].container_padding =
                            Some((10 * m + d) as i32);
                    }
                }
                let ids = wm.desktop_ids.clone();
                let created = fake.create_desktop(inserted);
                wm.process_event(WindowManagerEvent::DesktopCountChanged)
                    .unwrap();
                assert_eq!(wm.desktop_count, 4);
                assert_eq!(wm.desktop_ids[inserted], created);
                assert_eq!(wm.focused_desktop_idx, if inserted <= 1 { 2 } else { 1 });
                for (d, id) in wm.desktop_ids.iter().enumerate() {
                    if let Some(original) = ids.iter().position(|old| old == id) {
                        for m in 0..2 {
                            assert_eq!(
                                wm.monitors()[m].workspaces()[d].container_padding,
                                Some((10 * m + original) as i32)
                            );
                        }
                    }
                }
                assert!(fake.moved().is_empty());
                assert_eq!(fake.switch_calls(), 0);
            }
        }

        #[test]
        fn desktop_identity_removal_at_each_position_and_current_preserves_windows_and_layouts() {
            for removed in 0..4 {
                for current in [removed, (removed + 1) % 4] {
                    let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(4);
                    second_monitor(&mut wm);
                    fake.set_current_desktop(current);
                    wm.sync_desktop_count().unwrap();
                    let ids = wm.desktop_ids.clone();
                    for m in 0..2 {
                        for d in 0..4 {
                            wm.monitors_mut()[m].workspaces_mut()[d].workspace_padding =
                                Some((10 * m + d) as i32);
                            let hwnd = 1000 + (m * 100 + d) as isize;
                            wm.monitors_mut()[m].workspaces_mut()[d]
                                .new_container_for_window(Window::from(hwnd));
                            fake.set_window_desktop(hwnd, d);
                        }
                        // A floating member and a second stack member on the removed desktop.
                        let ws = &mut wm.monitors_mut()[m].workspaces_mut()[removed];
                        ws.focused_container_mut()
                            .unwrap()
                            .add_window_without_visibility_changes(Window::from(2000 + m as isize));
                        ws.floating_windows_mut()
                            .push_back(Window::from(3000 + m as isize));
                        fake.set_window_desktop(2000 + m as isize, removed);
                        fake.set_window_desktop(3000 + m as isize, removed);
                    }
                    let fallback = (removed + 1) % 4;
                    fake.destroy_desktop(removed, fallback);
                    wm.process_event(WindowManagerEvent::DesktopCountChanged)
                        .unwrap();
                    let target = wm
                        .desktop_ids
                        .iter()
                        .position(|id| *id == ids[fallback])
                        .unwrap();
                    assert_eq!(wm.desktop_count, 3);
                    assert!(!wm.desktop_ids.contains(&ids[removed]));
                    for m in 0..2 {
                        assert_eq!(wm.monitors()[m].workspaces().len(), 3);
                        assert_eq!(
                            wm.monitors()[m].focused_workspace_idx(),
                            wm.focused_desktop_idx
                        );
                        assert_caches(&wm, 1000 + (m * 100 + removed) as isize, (m, target));
                        assert_caches(&wm, 2000 + m as isize, (m, target));
                        assert_caches(&wm, 3000 + m as isize, (m, target));
                        let ws = &wm.monitors()[m].workspaces()[target];
                        assert_eq!(
                            ws.containers()
                                .iter()
                                .find(|c| c.contains_window(2000 + m as isize))
                                .unwrap()
                                .windows()
                                .len(),
                            2
                        );
                        assert!(
                            ws.floating_windows()
                                .iter()
                                .any(|w| w.hwnd == 3000 + m as isize)
                        );
                        for (d, id) in wm.desktop_ids.iter().enumerate() {
                            let original = ids.iter().position(|old| old == id).unwrap();
                            assert_eq!(
                                wm.monitors()[m].workspaces()[d].workspace_padding,
                                Some((10 * m + original) as i32)
                            );
                        }
                    }
                    assert!(fake.moved().is_empty());
                    assert_eq!(fake.switch_calls(), 0);
                }
            }
        }

        #[test]
        fn desktop_identity_failed_window_query_keeps_old_state_until_retry() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            let ids = wm.desktop_ids.clone();
            wm.monitors_mut()[0].workspaces_mut()[1].new_container_for_window(Window::from(42));
            wm.update_known_hwnds();
            fake.destroy_desktop(1, 2);
            assert!(
                wm.process_event(WindowManagerEvent::DesktopCountChanged)
                    .is_err()
            );
            assert_eq!(wm.desktop_ids, ids);
            assert_eq!(wm.desktop_count, 3);
            assert_caches(&wm, 42, (0, 1));
            fake.set_window_desktop(42, 1);
            wm.process_event(WindowManagerEvent::DesktopCountChanged)
                .unwrap();
            assert_eq!(wm.desktop_count, 2);
            assert_eq!(wm.desktop_ids[1], ids[2]);
            assert_caches(&wm, 42, (0, 1));
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn desktop_identity_removal_uses_each_actual_window_destination_including_special_modes() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            let ws = &mut wm.monitors_mut()[0].workspaces_mut()[1];
            let mut monocle = Container::default();
            monocle.add_window_without_visibility_changes(Window::from(10));
            monocle.add_window_without_visibility_changes(Window::from(11));
            ws.monocle_container = Some(monocle);
            ws.maximized_window = Some(Window::from(12));
            fake.destroy_desktop(1, 0);
            fake.set_window_desktop(10, 0);
            fake.set_window_desktop(11, 1);
            fake.set_window_desktop(12, 1);
            wm.process_event(WindowManagerEvent::DesktopCountChanged)
                .unwrap();
            assert_caches(&wm, 10, (0, 0));
            assert_caches(&wm, 11, (0, 1));
            assert_caches(&wm, 12, (0, 1));
            let a = &wm.monitors()[0].workspaces()[0].containers()[0];
            let b = &wm.monitors()[0].workspaces()[1].containers()[0];
            assert_ne!(a.id, b.id);
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn desktop_identity_names_and_empty_names_update_all_caches_and_subscribers() {
            use crate::virtual_desktop::DesktopBackend;
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            fake.set_desktop_name(1, "Work").unwrap();
            let subscriber = SubscriberProbe::new();
            wm.process_event(WindowManagerEvent::DesktopRenamed { desktop: 1 })
                .unwrap();
            let named = subscriber.next();
            assert_eq!(
                named.state.monitors.elements()[1].workspaces()[1]
                    .name
                    .as_deref(),
                Some("Work")
            );
            for monitor in wm.monitors() {
                assert_eq!(
                    monitor.workspace_names.get(&1).map(String::as_str),
                    Some("Work")
                );
            }
            fake.set_desktop_name(1, "").unwrap();
            wm.process_event(WindowManagerEvent::DesktopRenamed { desktop: 99 })
                .unwrap();
            let cleared = subscriber.next();
            for monitor in cleared.state.monitors.elements() {
                assert!(monitor.workspaces()[1].name.is_none());
                assert!(!monitor.workspace_names.contains_key(&1));
            }
            assert!(!wm.workspace_names.contains_key(&1));
        }

        #[test]
        fn desktop_identity_reaper_rejects_stale_or_background_associations() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            wm.monitors_mut()[0].workspaces_mut()[0].new_container_for_window(Window::from(42));
            fake.set_window_desktop(42, 0);
            wm.update_known_hwnds();
            assert!(crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                true
            ));
            fake.move_desktop(0, 2);
            wm.process_event(WindowManagerEvent::DesktopMoved {
                desktop: 2,
                old: 0,
                new: 2,
            })
            .unwrap();
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                true
            ));
            fake.set_current_desktop(0);
            wm.process_event(WindowManagerEvent::DesktopChanged { new: 0, old: 2 })
                .unwrap();
            assert!(wm.is_window_on_other_desktop(42));
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 2),
                true
            ));
        }

        #[test]
        fn desktop_identity_invalid_snapshots_do_not_mutate_the_last_coherent_state() {
            use crate::virtual_desktop::{DesktopId, DesktopInfo, DesktopSnapshot};
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let ids = wm.desktop_ids.clone();
            let monitors = wm.monitors.clone();
            for snapshot in [
                DesktopSnapshot {
                    desktops: vec![],
                    current: ids[0],
                },
                DesktopSnapshot {
                    desktops: vec![
                        DesktopInfo {
                            id: ids[0],
                            name: None
                        };
                        2
                    ],
                    current: ids[0],
                },
                DesktopSnapshot {
                    desktops: vec![DesktopInfo {
                        id: ids[0],
                        name: None,
                    }],
                    current: DesktopId(999),
                },
            ] {
                fake.set_snapshot_override(Some(snapshot));
                assert!(
                    wm.process_event(WindowManagerEvent::DesktopCountChanged)
                        .is_err()
                );
                assert_eq!(wm.desktop_ids, ids);
                assert_eq!(wm.monitors, monitors);
                assert_eq!(wm.desktop_count, 2);
            }
            fake.set_snapshot_override(None);
        }

        #[test]
        fn desktop_identity_closed_window_does_not_block_removal() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            wm.monitors_mut()[0].workspaces_mut()[1].new_container_for_window(Window::from(42));
            wm.update_known_hwnds();
            wm.window_api = Arc::new(FakeWindowApi {
                invalid_hwnds: HashSet::from([42]),
                ..api(0, 0)
            });
            fake.destroy_desktop(1, 0);
            wm.process_event(WindowManagerEvent::DesktopCountChanged)
                .unwrap();
            assert_eq!(wm.desktop_count, 2);
            assert!(!wm.known_hwnds.contains_key(&42));
            assert!(!crate::reaper::HWNDS_CACHE.lock().contains_key(&42));
        }

        #[test]
        fn desktop_identity_focus_event_redraws_once_and_publishes_native_focus() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            let before = crate::border_manager::BORDER_REDRAWS.load(Ordering::SeqCst);
            let subscriber = SubscriberProbe::new();
            fake.set_current_desktop(1);
            // Payload is deliberately stale. The authoritative current ID is desktop 1.
            wm.process_event(WindowManagerEvent::DesktopChanged { new: 0, old: 1 })
                .unwrap();
            let notification = subscriber.next();
            assert_eq!(notification.state.focused_desktop_idx, 1);
            for monitor in notification.state.monitors.elements() {
                assert_eq!(monitor.focused_workspace_idx(), 1);
            }
            wm.process_event(WindowManagerEvent::DesktopChanged { new: 0, old: 1 })
                .unwrap();
            assert_eq!(
                crate::border_manager::BORDER_REDRAWS.load(Ordering::SeqCst),
                before + 1
            );
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn desktop_identity_global_changes_are_detected_even_without_monitors() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.monitors.elements_mut().clear();
            let before = crate::State::from(&wm);
            fake.create_desktop(2);
            wm.sync_desktop_count().unwrap();
            assert!(before.has_been_modified(&wm));
            let before = crate::State::from(&wm);
            fake.set_current_desktop(1);
            wm.sync_desktop_count().unwrap();
            assert!(before.has_been_modified(&wm));
        }

        #[test]
        fn desktop_identity_reaper_cleans_dead_background_windows() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.monitors_mut()[0].workspaces_mut()[0].new_container_for_window(Window::from(42));
            fake.set_window_desktop(42, 0);
            wm.update_known_hwnds();
            fake.set_current_desktop(1);
            assert!(wm.is_window_on_other_desktop(42));

            // Inject HWND validity: synthetic handles must never query live windows.
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                true
            ));
            assert!(crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                false
            ));
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 1),
                false
            ));
        }

        #[test]
        fn desktop_identity_reaper_preserves_windows_before_the_queued_native_event() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.monitors_mut()[0].workspaces_mut()[0].new_container_for_window(Window::from(42));
            fake.set_window_desktop(42, 0);
            wm.update_known_hwnds();
            fake.set_current_desktop(1);
            assert!(wm.is_window_on_other_desktop(42));
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                true
            ));
            fake.set_current_desktop(0);
            fake.set_window_desktop(42, 1);
            assert!(wm.is_window_on_other_desktop(42));
            assert!(!crate::reaper::should_reap_orphan_with_window_validity(
                &wm,
                42,
                (0, 0),
                true
            ));
        }

        #[test]
        fn desktop_identity_cached_monitor_keeps_its_own_workspace_associations() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            second_monitor(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[0].workspace_padding = Some(123);
            let mut cached = wm.monitors_mut().pop_back().unwrap();
            cached.last_focused_workspace = Some(0);
            let cached_id = cached.workspaces()[0].native_desktop_id;
            fake.move_desktop(0, 2);
            wm.process_event(WindowManagerEvent::DesktopMoved {
                desktop: 2,
                old: 0,
                new: 2,
            })
            .unwrap();
            wm.monitors_mut().push_back(cached);
            wm.sync_desktop_count().unwrap();
            assert_eq!(
                wm.monitors()[1].workspaces()[2].native_desktop_id,
                cached_id
            );
            assert_eq!(
                wm.monitors()[1].workspaces()[2].workspace_padding,
                Some(123)
            );
            assert_eq!(wm.monitors()[1].last_focused_workspace, Some(2));
            assert_eq!(
                wm.monitors()[1].focused_workspace_idx(),
                wm.focused_desktop_idx
            );
            // Runtime IDs do not change the public index-based serialization contract.
            let state = serde_json::to_value(crate::State::from(&wm)).unwrap();
            assert!(
                state["monitors"]["elements"][1]["workspaces"]["elements"][2]
                    .get("native_desktop_id")
                    .is_none()
            );
        }

        #[test]
        fn desktop_identity_stack_move_keeps_target_when_windows_reorders_desktops_mid_command() {
            use crate::virtual_desktop::DesktopBackend;
            for follow in [false, true] {
                let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
                let target = wm.desktop_ids[2];
                let ws = wm.focused_workspace_mut().unwrap();
                ws.new_container_for_window(Window::from(10));
                ws.focused_container_mut()
                    .unwrap()
                    .add_window_without_visibility_changes(Window::from(11));
                fake.set_window_desktop(10, 0);
                fake.set_window_desktop(11, 0);
                fake.reorder_after_next_move(0, 2);
                wm.move_container_to_workspace(2, follow, None).unwrap();
                assert_eq!(fake.desktop_id_for_window(10).unwrap(), target);
                assert_eq!(fake.desktop_id_for_window(11).unwrap(), target);
                assert_eq!(fake.moved(), vec![(10, 2), (11, 1)]);
                assert_caches(&wm, 10, (0, 1));
                assert_caches(&wm, 11, (0, 1));
                assert_eq!(wm.monitors()[0].workspaces()[1].containers().len(), 1);
                assert_eq!(
                    wm.monitors()[0].workspaces()[1].containers()[0]
                        .windows()
                        .len(),
                    2
                );
                assert_eq!(wm.focused_desktop_idx, if follow { 1 } else { 2 });
                assert_eq!(fake.switch_calls(), usize::from(follow));
            }
        }

        #[test]
        fn desktop_identity_index_commands_refresh_native_topology_before_resolving_target() {
            use crate::virtual_desktop::DesktopBackend;
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let created = fake.create_desktop(0);
            wm.focus_desktop(0).unwrap();
            assert_eq!(wm.desktop_count, 3);
            assert_eq!(wm.desktop_ids[0], created);
            assert_eq!(fake.current_desktop_id().unwrap(), created);
            assert_eq!(wm.focused_desktop_idx, 0);
        }

        #[test]
        fn native_compat_obsolete_commands_cannot_create_remove_or_swap_desktops() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            let before = wm.monitors.clone();
            for command in [
                crate::SocketMessage::NewWorkspace,
                crate::SocketMessage::CloseWorkspace,
                crate::SocketMessage::EnsureWorkspaces(0, 99),
                crate::SocketMessage::EnsureNamedWorkspaces(
                    1,
                    vec!["A".into(), "B".into(), "C".into()],
                ),
            ] {
                wm.process_command(command, Vec::new()).unwrap();
                assert_eq!(wm.monitors, before);
            }
            assert!(
                wm.process_command(
                    crate::SocketMessage::SwapWorkspacesToMonitorNumber(1),
                    Vec::new()
                )
                .is_err()
            );
            assert!(
                wm.process_command(
                    crate::SocketMessage::MoveWorkspaceToMonitorNumber(1),
                    Vec::new()
                )
                .is_err()
            );
            assert_eq!(wm.monitors, before);
            assert_eq!(wm.desktop_count, 2);
            assert!(fake.moved().is_empty());
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn native_navigation_stops_at_both_ends_and_refreshes_native_focus() {
            for count in [1, 3] {
                let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(count);
                wm.focus_adjacent_desktop(CycleDirection::Previous).unwrap();
                assert_eq!(wm.focused_desktop_idx, 0);
                assert_eq!(fake.switch_calls(), 0);
                for idx in 1..count {
                    wm.focus_adjacent_desktop(CycleDirection::Next).unwrap();
                    assert_eq!(wm.focused_desktop_idx, idx);
                }
                let calls = fake.switch_calls();
                wm.focus_adjacent_desktop(CycleDirection::Next).unwrap();
                assert_eq!(wm.focused_desktop_idx, count - 1);
                assert_eq!(fake.switch_calls(), calls);
                for idx in (0..count.saturating_sub(1)).rev() {
                    wm.focus_adjacent_desktop(CycleDirection::Previous).unwrap();
                    assert_eq!(wm.focused_desktop_idx, idx);
                }
            }
        }

        #[test]
        fn native_compat_focus_last_and_aliases_use_one_global_desktop() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.process_command(crate::SocketMessage::FocusWorkspaceNumbers(1), Vec::new())
                .unwrap();
            wm.process_command(crate::SocketMessage::FocusLastWorkspace, Vec::new())
                .unwrap();
            assert_eq!(wm.focused_desktop_idx, 0);
            for monitor in wm.monitors() {
                assert_eq!(monitor.focused_workspace_idx(), 0);
                assert_eq!(monitor.last_focused_workspace, Some(1));
            }
            wm.process_command(crate::SocketMessage::FocusLastWorkspace, Vec::new())
                .unwrap();
            assert_eq!(wm.focused_desktop_idx, 1);
            assert_eq!(fake.switch_calls(), 3);
            let before = wm.monitors.clone();
            assert!(
                wm.process_command(
                    crate::SocketMessage::FocusMonitorWorkspaceNumber(1, 99),
                    Vec::new()
                )
                .is_err()
            );
            assert_eq!(wm.monitors, before);
        }

        #[test]
        fn native_compat_eager_focus_switches_native_desktop_before_selecting_window() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[1].new_container_for_window(Window::from(42));
            fake.set_window_desktop(42, 1);
            let window_api = Arc::new(FakeWindowApi {
                window_monitors: HashMap::from([(42, 7)]),
                ..api(0, 0)
            });
            wm.window_api = window_api.clone();
            wm.process_command(
                crate::SocketMessage::EagerFocus("native-test.exe".into()),
                Vec::new(),
            )
            .unwrap();
            assert_eq!(wm.focused_desktop_idx, 1);
            assert_eq!(wm.focused_monitor_idx(), 1);
            assert_eq!(fake.switch_calls(), 1);
            assert!(fake.moved().is_empty());
            assert!(window_api.effects.lock().contains(&("focus", 42)));
            for monitor in wm.monitors() {
                assert_eq!(monitor.focused_workspace_idx(), 1);
            }
        }

        #[test]
        fn native_compat_monitor_move_translates_untiled_stack_and_preserves_selection() {
            for destination in [0, 1] {
                for follow in [false, true] {
                    let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
                    second_monitor(&mut wm);
                    let api = Arc::new(api(42, 0));
                    wm.window_api = api.clone();
                    wm.monitors_mut()[1].workspaces_mut()[destination].tile = false;
                    let source = wm.focused_workspace_mut().unwrap();
                    source.new_container_for_window(Window::from(42));
                    let stack = source.focused_container_mut().unwrap();
                    stack.add_window_without_visibility_changes(Window::from(43));
                    stack.focus_window(0);
                    for hwnd in [42, 43] {
                        fake.set_window_desktop(hwnd, 0);
                    }

                    wm.move_container_to_monitor(1, Some(destination), follow, None)
                        .unwrap();

                    let target = &wm.monitors()[1].workspaces()[destination];
                    assert_eq!(
                        target
                            .focused_container()
                            .unwrap()
                            .focused_window()
                            .unwrap()
                            .hwnd,
                        42
                    );
                    for hwnd in [42, 43] {
                        assert_caches(&wm, hwnd, (1, destination));
                    }
                    let effects = api.effects.lock();
                    let translated = effects
                        .iter()
                        .filter(|(effect, _)| *effect == "move_area")
                        .copied()
                        .collect::<Vec<_>>();
                    assert_eq!(translated, vec![("move_area", 42), ("move_area", 43)]);
                    assert!(!effects.contains(&("focus", 43)));
                    if follow {
                        assert_eq!(
                            effects.iter().rev().find(|(effect, _)| *effect == "focus"),
                            Some(&("focus", 42))
                        );
                    } else {
                        assert!(!effects.iter().any(|(effect, _)| *effect == "focus"));
                    }
                }
            }
        }

        #[test]
        fn native_compat_monitor_desktop_movement_keeps_all_rings_global() {
            for follow in [false, true] {
                let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
                second_monitor(&mut wm);
                wm.focused_workspace_mut()
                    .unwrap()
                    .new_container_for_window(Window::from(42));
                wm.focused_workspace_mut()
                    .unwrap()
                    .focused_container_mut()
                    .unwrap()
                    .add_window_without_visibility_changes(Window::from(43));
                fake.set_window_desktop(42, 0);
                fake.set_window_desktop(43, 0);
                wm.move_container_to_monitor(1, Some(1), follow, None)
                    .unwrap();
                assert_caches(&wm, 42, (1, 1));
                assert_caches(&wm, 43, (1, 1));
                assert_eq!(fake.moved(), vec![(42, 1), (43, 1)]);
                assert_eq!(fake.switch_calls(), usize::from(follow));
                for monitor in wm.monitors() {
                    assert_eq!(monitor.focused_workspace_idx(), usize::from(follow));
                }
            }
        }

        #[test]
        fn native_compat_restore_queries_destinations_and_preserves_new_windows_and_modes() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let mut state = crate::State::from(&wm);
            state.monitors.elements_mut()[0]
                .workspaces_mut()
                .resize(3, Workspace::default());
            let excess = &mut state.monitors.elements_mut()[0].workspaces_mut()[2];
            excess.new_container_for_window(Window::from(10));
            excess
                .focused_container_mut()
                .unwrap()
                .add_window_without_visibility_changes(Window::from(14));
            excess.floating_windows_mut().push_back(Window::from(11));
            excess.maximized_window = Some(Window::from(12));
            let mut monocle = Container::default();
            monocle.add_window_without_visibility_changes(Window::from(13));
            excess.monocle_container = Some(monocle);
            wm.monitors_mut()[0].workspaces_mut()[1].new_container_for_window(Window::from(20));
            for (hwnd, d) in [(10, 0), (14, 0), (11, 1), (12, 1), (13, 0), (20, 1)] {
                fake.set_window_desktop(hwnd, d);
            }
            fake.set_current_desktop(1);
            wm.apply_state(state).unwrap();
            for (hwnd, d) in [(10, 0), (14, 0), (11, 1), (12, 1), (13, 0), (20, 1)] {
                assert_caches(&wm, hwnd, (0, d));
            }
            assert_eq!(wm.monitors()[0].workspaces().len(), 2);
            assert_eq!(wm.focused_desktop_idx, 1);
            assert_eq!(
                wm.monitors()[0].workspaces()[0]
                    .containers()
                    .iter()
                    .find(|c| c.contains_window(10))
                    .unwrap()
                    .windows()
                    .len(),
                2
            );
            assert_eq!(
                wm.monitors()[0].workspaces()[0]
                    .monocle_container
                    .as_ref()
                    .unwrap()
                    .focused_window()
                    .unwrap()
                    .hwnd,
                13
            );
            assert_eq!(
                wm.monitors()[0].workspaces()[1]
                    .maximized_window
                    .unwrap()
                    .hwnd,
                12
            );
            assert!(fake.moved().is_empty());
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn native_compat_restore_unknown_live_window_fails_without_overwriting_state() {
            let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let before = wm.monitors.clone();
            let mut state = crate::State::from(&wm);
            state.monitors.elements_mut()[0].workspaces_mut()[0]
                .new_container_for_window(Window::from(42));
            assert!(wm.apply_state(state).is_err());
            assert_eq!(wm.monitors, before);
        }

        #[test]
        fn native_compat_legacy_serialized_state_uses_current_native_focus_and_names() {
            use crate::virtual_desktop::DesktopBackend;
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let mut serialized = serde_json::to_value(crate::State::from(&wm)).unwrap();
            serialized
                .as_object_mut()
                .unwrap()
                .remove("focused_desktop_idx");
            serialized.as_object_mut().unwrap().remove("desktop_count");
            let state = serde_json::from_value(serialized).unwrap();
            fake.set_desktop_name(1, "From Task View").unwrap();
            fake.set_current_desktop(1);
            wm.apply_state(state).unwrap();
            assert_eq!(wm.focused_desktop_idx, 1);
            assert_eq!(wm.desktop_count, 2);
            assert_eq!(
                wm.workspace_names.get(&1).map(String::as_str),
                Some("From Task View")
            );
            assert_eq!(fake.switch_calls(), 0);
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn native_compat_discovery_places_windows_on_their_native_desktops_without_following() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.window_api = Arc::new(FakeWindowApi {
                enumerated_windows: vec![10, 20, 10],
                window_monitors: HashMap::from([(20, 7)]),
                ..api(0, 0)
            });
            fake.set_window_desktop(10, 1);
            fake.set_window_desktop(20, 0);
            wm.discover_windows().unwrap();
            assert_caches(&wm, 10, (0, 1));
            assert_caches(&wm, 20, (1, 0));
            assert_eq!(wm.focused_desktop_idx, 0);
            assert_eq!(wm.focused_monitor_idx(), 0);
            assert!(fake.moved().is_empty());
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn native_compat_config_load_and_reload_preserve_windows_names() {
            use crate::virtual_desktop::DesktopBackend;
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            // Exercise the configuration stages shared by postload and reload,
            // without invoking reload's real Windows focus-setting APIs.
            for native_name in ["From Task View", "Renamed in Task View"] {
                fake.set_desktop_name(0, native_name).unwrap();
                let snapshot = fake.snapshot().unwrap();
                wm.sync_desktop_count().unwrap();
                let ids = wm.desktop_ids.clone();
                for (i, monitor) in wm.monitors_mut().iter_mut().enumerate() {
                    crate::StaticConfig::align_configured_monitor(monitor, &ids, 20).unwrap();
                    let config = serde_json::from_value(serde_json::json!({
                        "name": format!("Legacy name {i}"),
                        "workspace_padding": 17
                    }))
                    .unwrap();
                    monitor.workspaces_mut()[0]
                        .load_static_config(&config, None)
                        .unwrap();
                    // Loading settings itself must not temporarily expose a stale name.
                    assert_eq!(monitor.workspaces()[0].name.as_deref(), Some(native_name));
                    assert_eq!(monitor.workspaces()[0].workspace_padding, Some(17));
                }
                wm.sync_desktop_count().unwrap();
                assert_eq!(fake.snapshot().unwrap(), snapshot);
                assert_eq!(wm.desktop_count, 2);
                assert_eq!(
                    wm.workspace_names.get(&0).map(String::as_str),
                    Some(native_name)
                );
                let state = crate::State::from(&wm);
                for monitor in state.monitors.elements() {
                    assert_eq!(monitor.workspaces().len(), 2);
                    assert_eq!(monitor.workspaces()[0].name.as_deref(), Some(native_name));
                }
            }
            assert_eq!(fake.switch_calls(), 0);
            assert!(fake.moved().is_empty());
        }

        fn identify_second(wm: &mut WindowManager) {
            second_monitor(wm);
            wm.monitors_mut()[1].device_id = "SecondDevice".into();
            wm.monitors_mut()[1].serial_number_id = Some("SecondSerial".into());
        }

        #[test]
        fn native_compat_hotplug_preserves_background_windows_and_current_desktop_without_minimizing()
         {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            identify_second(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[0].workspace_padding = Some(123);
            wm.monitors_mut()[1].workspaces_mut()[0].new_container_for_window(Window::from(20));
            wm.monitors_mut()[1].workspaces_mut()[1]
                .floating_windows_mut()
                .push_back(Window::from(21));
            fake.set_window_desktop(20, 0);
            fake.set_window_desktop(21, 1);
            wm.window_api = Arc::new(api(0, 0));
            wm.update_known_hwnds();
            let primary = wm.monitors()[0].clone();
            let second = wm.monitors()[1].clone();
            let mut cache = HashMap::new();
            crate::monitor_reconciliator::reconcile_attached_monitors(
                &mut wm,
                vec![primary.clone()],
                &mut cache,
            )
            .unwrap();
            assert_caches(&wm, 20, (0, 0));
            assert_caches(&wm, 21, (0, 1));
            fake.destroy_desktop(1, 0);
            wm.process_event(WindowManagerEvent::DesktopCountChanged)
                .unwrap();
            crate::monitor_reconciliator::reconcile_attached_monitors(
                &mut wm,
                vec![primary, second],
                &mut cache,
            )
            .unwrap();
            assert_caches(&wm, 20, (0, 0));
            assert_caches(&wm, 21, (0, 0));
            assert_eq!(
                wm.monitors()[1].workspaces()[0].workspace_padding,
                Some(123)
            );
            for monitor in wm.monitors() {
                assert_eq!(monitor.workspaces().len(), 1);
                assert_eq!(monitor.focused_workspace_idx(), 0);
            }
            assert_eq!(fake.switch_calls(), 0);
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn native_hotplug_adopts_removed_monitor_windows_despite_stale_hwnd_cache() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            identify_second(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[0].new_container_for_window(Window::from(81));
            wm.monitors_mut()[1].workspaces_mut()[1]
                .floating_windows_mut()
                .push_back(Window::from(82));
            fake.set_window_desktop(81, 0);
            fake.set_window_desktop(82, 1);
            wm.window_api = Arc::new(api(0, 0));
            wm.update_known_hwnds();
            let previous = crate::state::State::from(wm.as_ref());
            wm.monitors_mut().pop_back();
            // The HWND cache still refers to the old topology until reconciliation finishes.
            assert_eq!(wm.known_hwnds.get(&81), Some(&(1, 0)));
            wm.preserve_windows_after_monitor_change(&previous).unwrap();
            assert_caches(&wm, 81, (0, 0));
            assert_caches(&wm, 82, (0, 1));
            assert_eq!(fake.switch_calls(), 0);
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn native_compat_hotplug_query_failure_rolls_back_membership_and_cache() {
            let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            identify_second(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[1].new_container_for_window(Window::from(42));
            wm.update_known_hwnds();
            let before = wm.monitors.clone();
            let primary = wm.monitors()[0].clone();
            let mut cache = HashMap::new();
            assert!(
                crate::monitor_reconciliator::reconcile_attached_monitors(
                    &mut wm,
                    vec![primary],
                    &mut cache
                )
                .is_err()
            );
            assert_eq!(wm.monitors, before);
            assert!(cache.is_empty());
            assert_caches(&wm, 42, (1, 1));
        }

        #[test]
        fn native_compat_hotplug_rebuilds_preferences_and_distinguishes_monitors_without_serials() {
            let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            identify_second(&mut wm);
            let first = wm.monitors()[0].clone();
            let second = wm.monitors()[1].clone();
            crate::DISPLAY_INDEX_PREFERENCES
                .write()
                .insert(5, "SecondSerial".into());
            let mut cache = HashMap::new();
            crate::monitor_reconciliator::reconcile_attached_monitors(
                &mut wm,
                vec![second.clone(), first.clone()],
                &mut cache,
            )
            .unwrap();
            assert_eq!(wm.monitor_usr_idx_map.get(&5), Some(&1));
            let mut a = first;
            let mut b = second;
            a.serial_number_id = None;
            b.serial_number_id = None;
            assert!(!crate::monitor_reconciliator::same_device(&a, &b));
        }

        #[test]
        fn native_compat_failed_monitor_move_publishes_reconciled_native_membership() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            identify_second(&mut wm);
            wm.window_api = Arc::new(FakeWindowApi {
                fail_area_move: true,
                ..api(20, 0)
            });
            wm.monitors_mut()[0].workspaces_mut()[0]
                .floating_windows_mut()
                .push_back(Window::from(20));
            fake.set_window_desktop(20, 0);
            let subscriber = SubscriberProbe::new();
            assert!(
                wm.process_command(
                    crate::SocketMessage::MoveContainerToMonitorWorkspaceNumber(1, 1),
                    Vec::new()
                )
                .is_err()
            );
            assert_caches(&wm, 20, (0, 1));
            let notification = subscriber.next();
            assert_eq!(
                notification.state.monitors.elements()[0].workspaces()[1].floating_windows()[0]
                    .hwnd,
                20
            );
            assert_eq!(fake.moved(), vec![(20, 1)]);
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn native_compat_restore_keeps_resize_and_current_config_without_restoring_preselection() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let mut saved = crate::State::from(&wm);
            let workspace = &mut saved.monitors.elements_mut()[0].workspaces_mut()[0];
            workspace.new_container_for_window(Window::from(42));
            workspace.resize_dimensions = vec![Some(Rect {
                left: 1,
                top: 2,
                right: 3,
                bottom: 4,
            })];
            workspace.preselected_container_idx = Some(0);
            workspace.workspace_config =
                Some(serde_json::from_value(serde_json::json!({"name":"Old"})).unwrap());
            wm.monitors_mut()[0].workspaces_mut()[0].workspace_config =
                Some(serde_json::from_value(serde_json::json!({"name":"Current"})).unwrap());
            fake.set_window_desktop(42, 0);
            wm.apply_state(saved).unwrap();
            let workspace = &wm.monitors()[0].workspaces()[0];
            assert_eq!(
                workspace.resize_dimensions[0],
                Some(Rect {
                    left: 1,
                    top: 2,
                    right: 3,
                    bottom: 4
                })
            );
            assert!(workspace.preselected_container_idx.is_none());
            assert_eq!(workspace.workspace_config.as_ref().unwrap().name, "Current");
            assert_caches(&wm, 42, (0, 0));
        }

        struct SubscriberProbe {
            name: String,
            path: PathBuf,
            stop: Arc<std::sync::atomic::AtomicBool>,
            worker: Option<std::thread::JoinHandle<()>>,
            received: std::sync::mpsc::Receiver<crate::Notification>,
        }

        impl SubscriberProbe {
            fn new() -> Self {
                use std::io::Read;
                let name = format!("native-test-{}", Uuid::new_v4());
                let path = std::env::temp_dir().join(format!("{name}.sock"));
                let listener = uds_windows::UnixListener::bind(&path).unwrap();
                listener.set_nonblocking(true).unwrap();
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done = stop.clone();
                let (sender, received) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    while !done.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                stream
                                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                                    .unwrap();
                                let mut bytes = Vec::new();
                                stream.read_to_end(&mut bytes).unwrap();
                                if sender
                                    .send(serde_json::from_slice(&bytes).unwrap())
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(error) => panic!("subscriber accept failed: {error}"),
                        }
                    }
                });
                crate::SUBSCRIPTION_SOCKETS
                    .lock()
                    .insert(name.clone(), path.clone());
                Self {
                    name,
                    path,
                    stop,
                    worker: Some(worker),
                    received,
                }
            }

            fn next(&self) -> crate::Notification {
                self.received
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap()
            }
        }

        impl Drop for SubscriberProbe {
            fn drop(&mut self) {
                crate::SUBSCRIPTION_SOCKETS.lock().remove(&self.name);
                self.stop.store(true, Ordering::SeqCst);
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
                let _ = std::fs::remove_file(&self.path);
            }
        }

        #[test]
        fn subscribers_receive_partial_state_when_centering_fails() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.window_api = Arc::new(FakeWindowApi {
                fail_center: true,
                ..api(0, 0)
            });
            wm.monitors_mut()[0].workspaces_mut()[1].float_override = Some(true);
            wm.window_management_behaviour.float_override_placement =
                crate::core::Placement::Center;
            fake.set_window_desktop(42, 1);
            let subscriber = SubscriberProbe::new();
            assert!(
                wm.process_event(events(42)[0])
                    .unwrap_err()
                    .to_string()
                    .contains("centering failure")
            );
            assert_caches(&wm, 42, (0, 1));
            let notification = subscriber.next();
            assert!(matches!(
                notification.event,
                crate::NotificationEvent::WindowManager(WindowManagerEvent::Show(..))
            ));
            assert_eq!(
                notification.state.monitors.elements()[0].workspaces()[1].floating_windows()[0]
                    .hwnd,
                42
            );
        }

        #[test]
        fn subscribers_receive_rule_changes_before_mouse_capture_returns() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.window_api = Arc::new(api(0, 0));
            wm.focused_workspace_mut()
                .unwrap()
                .new_container_for_window(Window::from(42));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, false)];
            fake.set_window_desktop(42, 0);
            let subscriber = SubscriberProbe::new();
            wm.process_event(WindowManagerEvent::MouseCapture(
                WinEvent::SystemCaptureEnd,
                Window::from(42),
            ))
            .unwrap();
            let notification = subscriber.next();
            assert!(notification.state.monitors.elements()[0].workspaces()[0].is_empty());
            assert!(notification.state.monitors.elements()[0].workspaces()[1].contains_window(42));
            assert_caches(&wm, 42, (0, 1));
        }

        #[test]
        fn fixture_restores_backend_and_global_caches_after_panic() {
            let original_backend = virtual_desktop::backend();
            let original_reaper = crate::reaper::HWNDS_CACHE.lock().clone();
            let original_hidden = crate::HIDDEN_HWNDS.lock().clone();
            let original_rules = crate::WORKSPACE_MATCHING_RULES.lock().clone();
            assert!(
                std::panic::catch_unwind(|| {
                    let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
                    wm.focused_workspace_mut()
                        .unwrap()
                        .new_container_for_window(Window::from(42));
                    wm.update_known_hwnds();
                    crate::HIDDEN_HWNDS.lock().push(42);
                    *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, true)];
                    panic!("exercise fixture unwind");
                })
                .is_err()
            );
            assert!(Arc::ptr_eq(&original_backend, &virtual_desktop::backend()));
            assert_eq!(*crate::reaper::HWNDS_CACHE.lock(), original_reaper);
            assert_eq!(*crate::HIDDEN_HWNDS.lock(), original_hidden);
            assert_eq!(*crate::WORKSPACE_MATCHING_RULES.lock(), original_rules);
        }

        #[test]
        fn repeated_events_are_idempotent_for_focused_and_background_desktops() {
            for desktop in [0, 1] {
                for order in [
                    [0, 1, 2],
                    [0, 2, 1],
                    [1, 0, 2],
                    [1, 2, 0],
                    [2, 0, 1],
                    [2, 1, 0],
                ] {
                    for floating in [false, true] {
                        let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
                        wm.window_api = Arc::new(api(0, 0));
                        wm.monitors_mut()[0].workspaces_mut()[desktop].float_override =
                            Some(floating);
                        fake.set_window_desktop(42, desktop);
                        for idx in order.into_iter().cycle().take(9) {
                            wm.process_event(events(42)[idx]).unwrap();
                        }
                        assert_caches(&wm, 42, (0, desktop));
                        assert_eq!(
                            wm.monitors()[0].workspaces()[desktop]
                                .floating_windows()
                                .len(),
                            usize::from(floating)
                        );
                        assert_eq!(wm.focused_desktop_idx, 0);
                        assert!(fake.moved().is_empty());
                        assert_eq!(fake.switch_calls(), 0);
                    }
                }
            }
        }

        #[test]
        fn repeated_events_preserve_existing_stacks_and_foreground_selection() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            let window_api = Arc::new(api(10, 0));
            wm.window_api = window_api.clone();
            let ws = wm.focused_workspace_mut().unwrap();
            ws.new_container_for_window(Window::from(10));
            ws.focused_container_mut()
                .unwrap()
                .add_window_without_visibility_changes(Window::from(11));
            for event in events(10).into_iter().chain(events(11)) {
                wm.process_event(event).unwrap();
            }
            assert_caches(&wm, 10, (0, 0));
            assert_caches(&wm, 11, (0, 0));
            let container = wm.focused_workspace().unwrap().focused_container().unwrap();
            assert_eq!(container.windows().len(), 2);
            assert_eq!(container.focused_window().unwrap().hwnd, 10);
            assert!(
                window_api
                    .effects
                    .lock()
                    .iter()
                    .all(|(kind, hwnd)| *kind == "stack" && *hwnd == 10)
            );
            assert!(fake.moved().is_empty());
        }

        #[test]
        fn explicit_rule_moves_native_desktop_and_monitor_without_following() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            second_monitor(&mut wm);
            wm.window_api = Arc::new(api(42, 0));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(1, 2, true)];
            fake.set_window_desktop(42, 0);
            for event in events(42) {
                wm.process_event(event).unwrap();
            }
            assert_caches(&wm, 42, (1, 2));
            assert_eq!(fake.moved(), vec![(42, 2)]);
            assert_eq!(
                virtual_desktop::backend()
                    .desktop_idx_for_window(42)
                    .unwrap(),
                2
            );
            assert_eq!(wm.focused_desktop_idx, 0);
            assert_eq!(wm.focused_monitor_idx(), 0);
            assert_eq!(fake.switch_calls(), 0);
            assert!(wm.already_moved_window_handles.lock().contains(&42));
        }

        #[test]
        fn rule_to_current_desktop_is_enforced_and_keeps_target_append_behaviour() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[0].new_container_for_window(Window::from(99));
            wm.monitors_mut()[1].workspaces_mut()[0].window_container_behaviour =
                Some(WindowContainerBehaviour::Append);
            wm.window_api = Arc::new(api(0, 0));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(1, 0, false)];
            fake.set_window_desktop(42, 1);
            wm.process_event(events(42)[0]).unwrap();
            assert_caches(&wm, 42, (1, 0));
            assert_eq!(wm.monitors()[1].workspaces()[0].containers().len(), 1);
            assert_eq!(
                wm.monitors()[1].workspaces()[0].containers()[0]
                    .windows()
                    .len(),
                2
            );
            assert_eq!(fake.moved(), vec![(42, 0)]);
            assert_eq!(wm.focused_monitor_idx(), 0);
            assert_eq!(fake.switch_calls(), 0);
        }

        #[test]
        fn invalid_rule_destinations_do_not_move_or_create_workspaces() {
            for (m, d) in [(99, 0), (0, 2)] {
                let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
                let window_api = Arc::new(api(42, 0));
                wm.window_api = window_api.clone();
                *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(m, d, true)];
                fake.set_window_desktop(42, 0);
                assert!(wm.process_event(events(42)[0]).is_err());
                assert!(wm.known_hwnds.is_empty());
                assert!(wm.already_moved_window_handles.lock().is_empty());
                assert!(fake.moved().is_empty());
                assert!(window_api.effects.lock().is_empty());
                assert_eq!(wm.monitors()[0].workspaces().len(), 2);
            }
        }

        #[test]
        fn failed_initial_rule_is_retryable_and_does_not_lose_the_window() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.window_api = Arc::new(api(0, 0));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, true)];
            fake.set_window_desktop(42, 0);
            fake.fail_after_move_for(42);
            assert!(wm.process_event(events(42)[0]).is_err());
            assert_caches(&wm, 42, (0, 1));
            assert!(!wm.already_moved_window_handles.lock().contains(&42));
            wm.process_event(events(42)[1]).unwrap();
            assert_caches(&wm, 42, (0, 1));
            assert!(wm.already_moved_window_handles.lock().contains(&42));
            assert_eq!(fake.moved(), vec![(42, 1)]);
        }

        #[test]
        fn initial_rule_does_not_undo_a_later_task_view_move() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(3);
            wm.window_api = Arc::new(api(0, 0));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, true)];
            fake.set_window_desktop(42, 0);
            wm.process_event(events(42)[0]).unwrap();
            fake.set_window_desktop(42, 2);
            wm.process_event(WindowManagerEvent::WindowDesktopChanged { hwnd: 42 })
                .unwrap();
            for event in events(42) {
                wm.process_event(event).unwrap();
            }
            assert_caches(&wm, 42, (0, 2));
            assert_eq!(fake.moved(), vec![(42, 1)]);
        }

        #[test]
        fn floating_rule_updates_monitor_geometry_and_native_desktop() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[1].float_override = Some(true);
            let window_api = Arc::new(api(0, 0));
            wm.window_api = window_api.clone();
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(1, 1, false)];
            fake.set_window_desktop(42, 0);
            for event in events(42) {
                wm.process_event(event).unwrap();
            }
            assert_caches(&wm, 42, (1, 1));
            assert_eq!(
                wm.monitors()[1].workspaces()[1].floating_windows()[0].hwnd,
                42
            );
            assert_eq!(
                window_api
                    .effects
                    .lock()
                    .iter()
                    .filter(|(kind, _)| *kind == "move_area")
                    .count(),
                1
            );
            assert_eq!(fake.moved(), vec![(42, 1)]);
        }

        #[test]
        fn floating_monitor_failure_reconciles_desktop_and_preserves_original_monitor() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            second_monitor(&mut wm);
            wm.monitors_mut()[1].workspaces_mut()[1].float_override = Some(true);
            wm.window_api = Arc::new(FakeWindowApi {
                fail_area_move: true,
                ..api(0, 0)
            });
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(1, 1, true)];
            fake.set_window_desktop(42, 0);
            assert!(wm.process_event(events(42)[0]).is_err());
            assert_caches(&wm, 42, (0, 1));
            assert!(wm.monitors()[1].workspaces()[1].is_empty());
            assert!(!wm.already_moved_window_handles.lock().contains(&42));
            assert_eq!(fake.moved(), vec![(42, 1)]);
        }

        #[test]
        fn mouse_capture_early_return_keeps_rule_movement_caches_consistent() {
            let (mut wm, fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.window_api = Arc::new(api(0, 0));
            wm.focused_workspace_mut()
                .unwrap()
                .new_container_for_window(Window::from(42));
            *crate::WORKSPACE_MATCHING_RULES.lock() = vec![rule(0, 1, false)];
            fake.set_window_desktop(42, 0);
            wm.process_event(WindowManagerEvent::MouseCapture(
                WinEvent::SystemCaptureEnd,
                Window::from(42),
            ))
            .unwrap();
            assert_caches(&wm, 42, (0, 1));
            assert_eq!(fake.moved(), vec![(42, 1)]);
        }

        #[test]
        fn ignored_event_also_repairs_derived_caches() {
            let (mut wm, _fake, _ctx) = setup_window_manager_with_fake_desktops(2);
            wm.focused_workspace_mut()
                .unwrap()
                .new_container_for_window(Window::from(42));
            wm.update_known_hwnds();
            crate::reaper::HWNDS_CACHE.lock().clear();
            wm.window_desktops.clear();
            // Default fake rejects management, exercising the earliest eligibility return.
            wm.process_event(events(42)[0]).unwrap();
            assert_caches(&wm, 42, (0, 0));
        }
    }
}
