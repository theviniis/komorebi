//! Historical per-monitor hotplug behavior for traditional workspaces.
use super::*;

pub(super) fn handle_traditional_notifications<F, I>(
    wm: Arc<Mutex<WindowManager>>,
    display_provider: F,
    receiver: Receiver<MonitorNotification>,
) -> color_eyre::Result<()>
where
    F: Fn() -> I + Copy,
    I: Iterator<Item = Result<win32_display_data::Device, win32_display_data::Error>>,
{
    tracing::info!("listening");

    'receiver: for notification in receiver {
        if !ACTIVE.load_consume()
            && matches!(
                notification,
                MonitorNotification::ResumingFromSuspendedState
                    | MonitorNotification::SessionUnlocked
            )
        {
            tracing::debug!(
                "reactivating reconciliator - system has resumed from suspended state or session has been unlocked"
            );

            ACTIVE.store(true, Ordering::SeqCst);
            border_manager::send_notification(None);
        }

        // Keep reference to Arc for potential re-locking
        let wm_arc = Arc::clone(&wm);
        let mut wm = wm.lock();

        let initial_state = State::from(wm.as_ref());

        match notification {
            MonitorNotification::EnteringSuspendedState | MonitorNotification::SessionLocked => {
                tracing::debug!(
                    "deactivating reconciliator until system resumes from suspended state or session is unlocked"
                );
                ACTIVE.store(false, Ordering::SeqCst);
            }
            MonitorNotification::WorkAreaChanged => {
                tracing::debug!("handling work area changed notification");
                let offset = wm.work_area_offset;
                for monitor in wm.monitors_mut() {
                    let mut should_update = false;

                    // Update work areas as necessary
                    if let Ok(reference) = WindowsApi::monitor(monitor.id)
                        && reference.work_area_size != monitor.work_area_size
                    {
                        monitor.work_area_size = Rect {
                            left: reference.work_area_size.left,
                            top: reference.work_area_size.top,
                            right: reference.work_area_size.right,
                            bottom: reference.work_area_size.bottom,
                        };

                        should_update = true;
                    }

                    if should_update {
                        tracing::info!("updated work area for {}", monitor.device_id);
                        monitor.update_focused_workspace(offset)?;
                        border_manager::send_notification(None);
                    } else {
                        tracing::debug!(
                            "work areas match, reconciliation not required for {}",
                            monitor.device_id
                        );
                    }
                }
            }
            MonitorNotification::ResolutionScalingChanged => {
                tracing::debug!("handling resolution/scaling changed notification");
                let offset = wm.work_area_offset;
                for monitor in wm.monitors_mut() {
                    let mut should_update = false;

                    // Update sizes and work areas as necessary
                    if let Ok(reference) = WindowsApi::monitor(monitor.id) {
                        if reference.work_area_size != monitor.work_area_size {
                            monitor.work_area_size = Rect {
                                left: reference.work_area_size.left,
                                top: reference.work_area_size.top,
                                right: reference.work_area_size.right,
                                bottom: reference.work_area_size.bottom,
                            };

                            should_update = true;
                        }

                        if reference.size != monitor.size {
                            monitor.size = Rect {
                                left: reference.size.left,
                                top: reference.size.top,
                                right: reference.size.right,
                                bottom: reference.size.bottom,
                            };

                            should_update = true;
                        }
                    }

                    if should_update {
                        tracing::info!(
                            "updated monitor resolution/scaling for {}",
                            monitor.device_id
                        );

                        monitor.update_focused_workspace(offset)?;
                        border_manager::send_notification(None);
                    } else {
                        tracing::debug!(
                            "resolutions match, reconciliation not required for {}",
                            monitor.device_id
                        );
                    }
                }
            }
            // this is handled above if the reconciliator is paused but we should still check if
            // there were any changes to the connected monitors while the system was
            // suspended/locked.
            MonitorNotification::ResumingFromSuspendedState
            | MonitorNotification::SessionUnlocked
            | MonitorNotification::DisplayConnectionChange => {
                tracing::debug!("handling display connection change notification");
                let mut monitor_cache = MONITOR_CACHE
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock();

                let initial_monitor_count = wm.monitors().len();

                // Get the currently attached display devices
                let attached_devices = attached_display_devices(display_provider)?;

                let geometry_changed = wm.monitors().iter().any(|monitor| {
                    attached_devices.iter().any(|attached| {
                        same_device(monitor, attached)
                            && (monitor.size != attached.size
                                || monitor.work_area_size != attached.work_area_size
                                || monitor.id != attached.id)
                    })
                });

                // Make sure that in our state any attached displays have the latest Win32 data
                for monitor in wm.monitors_mut() {
                    for attached in &attached_devices {
                        let serial_number_ids_match = if let (Some(attached_snid), Some(m_snid)) =
                            (&attached.serial_number_id, &monitor.serial_number_id)
                        {
                            attached_snid.eq(m_snid)
                        } else {
                            false
                        };

                        if serial_number_ids_match || attached.device_id.eq(&monitor.device_id) {
                            monitor.id = attached.id;
                            monitor.device = attached.device.clone();
                            monitor.device_id = attached.device_id.clone();
                            monitor.serial_number_id = attached.serial_number_id.clone();
                            monitor.name = attached.name.clone();
                            monitor.size = attached.size;
                            monitor.work_area_size = attached.work_area_size;
                        }
                    }
                }

                if attached_devices.is_empty() {
                    tracing::debug!(
                        "no devices found, skipping reconciliation to avoid breaking state"
                    );
                    drop(wm);
                    continue 'receiver;
                }

                // Handle potential monitor removal with verification
                let attached_devices = if has_detached_monitors(wm.monitors(), &attached_devices) {
                    tracing::info!(
                        "potential monitor removal detected ({initial_monitor_count} vs {}), verifying in 3s",
                        attached_devices.len()
                    );

                    // Release locks before waiting
                    drop(wm);
                    drop(monitor_cache);

                    // Wait 3 seconds for display state to stabilize
                    std::thread::sleep(std::time::Duration::from_secs(3));

                    // Re-query the Win32 display APIs
                    let re_queried_devices = match attached_display_devices(display_provider) {
                        Ok(devices) => devices,
                        Err(e) => {
                            tracing::error!("failed to re-query display devices: {}", e);
                            continue 'receiver;
                        }
                    };

                    tracing::debug!(
                        "after verification: wm had {} monitors, initial query found {}, re-query found {}",
                        initial_monitor_count,
                        attached_devices.len(),
                        re_queried_devices.len()
                    );

                    if re_queried_devices.is_empty() {
                        tracing::debug!("no devices after verification, preserving monitor state");
                        continue 'receiver;
                    }

                    // If monitors are back, the removal was transient (spurious event)
                    // Still try to restore state since windows might have been minimized
                    if {
                        let current = wm_arc.lock();
                        !has_detached_monitors(current.monitors(), &re_queried_devices)
                    } {
                        tracing::info!(
                            "monitor removal was transient (spurious event), attempting state restoration. Initial: {}, Re-queried: {}",
                            initial_monitor_count,
                            re_queried_devices.len()
                        );

                        // Re-acquire locks for state restoration
                        wm = wm_arc.lock();

                        // Update Win32 data for all monitors
                        for monitor in wm.monitors_mut() {
                            for attached in &re_queried_devices {
                                let serial_number_ids_match =
                                    if let (Some(attached_snid), Some(m_snid)) =
                                        (&attached.serial_number_id, &monitor.serial_number_id)
                                    {
                                        attached_snid.eq(m_snid)
                                    } else {
                                        false
                                    };

                                if serial_number_ids_match
                                    || attached.device_id.eq(&monitor.device_id)
                                {
                                    monitor.id = attached.id;
                                    monitor.device = attached.device.clone();
                                    monitor.device_id = attached.device_id.clone();
                                    monitor.serial_number_id = attached.serial_number_id.clone();
                                    monitor.name = attached.name.clone();
                                    monitor.size = attached.size;
                                    monitor.work_area_size = attached.work_area_size;
                                }
                            }
                        }

                        // Try to restore windows that might have been minimized
                        let offset = wm.work_area_offset;
                        for monitor in wm.monitors_mut() {
                            let focused_workspace_idx = monitor.focused_workspace_idx();

                            for (idx, workspace) in monitor.workspaces_mut().iter_mut().enumerate()
                            {
                                let is_focused_workspace = idx == focused_workspace_idx;

                                if is_focused_workspace {
                                    // Restore containers
                                    for container in workspace.containers_mut() {
                                        if let Some(window) = container.focused_window()
                                            && WindowsApi::is_window(window.hwnd)
                                        {
                                            tracing::debug!(
                                                "restoring window after transient removal: {}",
                                                window.hwnd
                                            );
                                            WindowsApi::restore_window(window.hwnd);
                                        } else if let Some(window) = container.focused_window() {
                                            tracing::debug!(
                                                "skipping restore of invalid window: {}",
                                                window.hwnd
                                            );
                                        }
                                    }

                                    // Restore maximized window
                                    if let Some(window) = &workspace.maximized_window
                                        && WindowsApi::is_window(window.hwnd)
                                    {
                                        WindowsApi::restore_window(window.hwnd);
                                    }

                                    // Restore monocle container
                                    if let Some(container) = &workspace.monocle_container
                                        && let Some(window) = container.focused_window()
                                        && WindowsApi::is_window(window.hwnd)
                                    {
                                        WindowsApi::restore_window(window.hwnd);
                                    }

                                    // Restore floating windows
                                    for window in workspace.floating_windows() {
                                        if WindowsApi::is_window(window.hwnd) {
                                            WindowsApi::restore_window(window.hwnd);
                                        }
                                    }
                                }
                            }

                            monitor.update_focused_workspace(offset)?;
                        }

                        border_manager::send_notification(None);
                        continue 'receiver;
                    }

                    // If monitors are still missing, proceed with actual removal logic
                    tracing::info!(
                        "verified monitor removal ({initial_monitor_count} vs {}), removing disconnected monitors",
                        re_queried_devices.len()
                    );

                    // Re-acquire locks for removal processing
                    wm = wm_arc.lock();
                    monitor_cache = MONITOR_CACHE
                        .get_or_init(|| Mutex::new(HashMap::new()))
                        .lock();

                    // Make sure that in our state any attached displays have the latest Win32 data
                    // We must do this again because we dropped the lock and are working with new data
                    for monitor in wm.monitors_mut() {
                        for attached in &re_queried_devices {
                            let serial_number_ids_match =
                                if let (Some(attached_snid), Some(m_snid)) =
                                    (&attached.serial_number_id, &monitor.serial_number_id)
                                {
                                    attached_snid.eq(m_snid)
                                } else {
                                    false
                                };

                            if serial_number_ids_match || attached.device_id.eq(&monitor.device_id)
                            {
                                monitor.id = attached.id;
                                monitor.device = attached.device.clone();
                                monitor.device_id = attached.device_id.clone();
                                monitor.serial_number_id = attached.serial_number_id.clone();
                                monitor.name = attached.name.clone();
                                monitor.size = attached.size;
                                monitor.work_area_size = attached.work_area_size;
                            }
                        }
                    }

                    // Use re-queried devices for remaining logic
                    re_queried_devices
                } else {
                    attached_devices
                };

                if has_detached_monitors(wm.monitors(), &attached_devices) {
                    tracing::info!("removing disconnected monitors");

                    // Windows to remove from `known_hwnds`
                    let mut windows_to_remove = Vec::new();

                    // Collect the ids in our state which aren't in the current attached display ids
                    // These are monitors that have been removed
                    let mut newly_removed_displays = vec![];

                    for (m_idx, m) in wm.monitors().iter().enumerate() {
                        if !attached_devices
                            .iter()
                            .any(|attached| same_device(m, attached))
                        {
                            let id = m
                                .serial_number_id
                                .as_ref()
                                .map_or(m.device_id.clone(), |sn| sn.clone());

                            newly_removed_displays.push(id.clone());

                            let focused_workspace_idx = m.focused_workspace_idx();

                            for (idx, workspace) in m.workspaces().iter().enumerate() {
                                let is_focused_workspace = idx == focused_workspace_idx;
                                let focused_container_idx = workspace.focused_container_idx();
                                for (c_idx, container) in workspace.containers().iter().enumerate()
                                {
                                    let focused_window_idx = container.focused_window_idx();
                                    for (w_idx, window) in container.windows().iter().enumerate() {
                                        windows_to_remove.push(window.hwnd);
                                        if is_focused_workspace
                                            && c_idx == focused_container_idx
                                            && w_idx == focused_window_idx
                                        {
                                            // Minimize the focused window since Windows might try
                                            // to move it to another monitor if it was focused.
                                            if window.is_focused() {
                                                window.minimize();
                                            }
                                        }
                                    }
                                }

                                if let Some(maximized) = &workspace.maximized_window {
                                    windows_to_remove.push(maximized.hwnd);
                                    // Minimize the focused window since Windows might try
                                    // to move it to another monitor if it was focused.
                                    if maximized.is_focused() {
                                        maximized.minimize();
                                    }
                                }

                                if let Some(container) = &workspace.monocle_container {
                                    for window in container.windows() {
                                        windows_to_remove.push(window.hwnd);
                                    }
                                    if let Some(window) = container.focused_window() {
                                        // Minimize the focused window since Windows might try
                                        // to move it to another monitor if it was focused.
                                        if window.is_focused() {
                                            window.minimize();
                                        }
                                    }
                                }

                                for window in workspace.floating_windows() {
                                    windows_to_remove.push(window.hwnd);
                                    // Minimize the focused window since Windows might try
                                    // to move it to another monitor if it was focused.
                                    if window.is_focused() {
                                        window.minimize();
                                    }
                                }
                            }

                            // Remove any workspace_rules for this specific monitor
                            let mut workspace_rules = WORKSPACE_MATCHING_RULES.lock();
                            let mut rules_to_remove = Vec::new();
                            for (i, rule) in workspace_rules.iter().enumerate().rev() {
                                if rule.monitor_index == m_idx {
                                    rules_to_remove.push(i);
                                }
                            }
                            for i in rules_to_remove {
                                workspace_rules.remove(i);
                            }

                            // Let's add their state to the cache for later, make sure to use what
                            // the user set as preference as the id.
                            let dip = DISPLAY_INDEX_PREFERENCES.read();
                            let mut dip_ids = dip.values();
                            let preferred_id = if dip_ids.any(|id| id.eq(&m.device_id)) {
                                m.device_id.clone()
                            } else if dip_ids.any(|id| Some(id) == m.serial_number_id.as_ref()) {
                                m.serial_number_id.clone().unwrap_or_default()
                            } else {
                                id
                            };
                            monitor_cache.insert(preferred_id, m.clone());
                        }
                    }

                    // Update known_hwnds
                    wm.known_hwnds.retain(|i, _| !windows_to_remove.contains(i));

                    if !newly_removed_displays.is_empty() {
                        // After we have cached them, remove them from our state
                        wm.monitors_mut().retain(|m| {
                            !newly_removed_displays.iter().any(|id| {
                                m.serial_number_id.as_ref().is_some_and(|sn| sn == id)
                                    || m.device_id.eq(id)
                            })
                        });
                    }

                    let post_removal_monitor_count = wm.monitors().len();
                    let focused_monitor_idx = wm.focused_monitor_idx();
                    if focused_monitor_idx >= post_removal_monitor_count {
                        // A replacement can temporarily leave no monitors until discovery below.
                        wm.monitors.focus(0);
                    }

                    let offset = wm.work_area_offset;

                    for monitor in wm.monitors_mut() {
                        // If we have lost a monitor, update everything to filter out any jank
                        if initial_monitor_count != post_removal_monitor_count {
                            monitor.update_focused_workspace(offset)?;
                        }
                    }
                }

                let post_removal_monitor_count = wm.monitors().len();

                // This is the list of device ids after we have removed detached displays. We can
                // keep this with just the device_ids without the serial numbers since this is used
                // only to check which one is the newly added monitor below if there is a new
                // monitor. Everything done after with said new monitor will again consider both
                // serial number and device ids.
                let post_removal_device_ids = wm
                    .monitors()
                    .iter()
                    .map(|m| &m.device_id)
                    .cloned()
                    .collect::<Vec<_>>();

                // Check for and add any new monitors that may have been plugged in
                // Monitor and display index preferences get applied in this function
                WindowsApi::load_monitor_information(&mut wm)?;

                let post_addition_monitor_count = wm.monitors().len();

                if post_addition_monitor_count > post_removal_monitor_count {
                    tracing::info!(
                        "monitor count mismatch ({post_removal_monitor_count} vs {post_addition_monitor_count}), adding connected monitors",
                    );

                    let known_hwnds = wm.known_hwnds.clone();
                    let offset = wm.work_area_offset;
                    let mouse_follows_focus = wm.mouse_follows_focus;
                    let focused_monitor_idx = wm.focused_monitor_idx();
                    let focused_workspace_idx = wm.focused_workspace_idx()?;

                    // Look in the updated state for new monitors
                    for (i, m) in wm.monitors_mut().iter_mut().enumerate() {
                        let device_id = &m.device_id;
                        // We identify a new monitor when we encounter a new device id
                        if !post_removal_device_ids.contains(device_id) {
                            let mut cache_hit = false;
                            let mut cached_id = String::new();
                            // Check if that device id exists in the cache for this session
                            if let Some((id, cached)) = monitor_cache.get_key_value(device_id).or(m
                                .serial_number_id
                                .as_ref()
                                .and_then(|sn| monitor_cache.get_key_value(sn)))
                            {
                                cache_hit = true;
                                cached_id = id.clone();

                                tracing::info!(
                                    "found monitor and workspace configuration for {id} in the monitor cache, applying"
                                );

                                // If it does, update the cached monitor info with the new one and
                                // load the cached monitor removing any window that has since been
                                // closed or moved to another workspace
                                *m = Monitor {
                                    native_desktop_workspaces: false,
                                    // Data that should be the one just read from `win32-display-data`
                                    id: m.id,
                                    name: m.name.clone(),
                                    device: m.device.clone(),
                                    device_id: m.device_id.clone(),
                                    serial_number_id: m.serial_number_id.clone(),
                                    size: m.size,
                                    work_area_size: m.work_area_size,

                                    // The rest should come from the cached monitor
                                    work_area_offset: cached.work_area_offset,
                                    window_based_work_area_offset: cached
                                        .window_based_work_area_offset,
                                    window_based_work_area_offset_limit: cached
                                        .window_based_work_area_offset_limit,
                                    workspaces: cached.workspaces.clone(),
                                    last_focused_workspace: cached.last_focused_workspace,
                                    workspace_names: cached.workspace_names.clone(),
                                    container_padding: cached.container_padding,
                                    workspace_padding: cached.workspace_padding,
                                    wallpaper: cached.wallpaper.clone(),
                                    floating_layer_behaviour: cached.floating_layer_behaviour,
                                };

                                let focused_workspace_idx = m.focused_workspace_idx();

                                for (j, workspace) in m.workspaces_mut().iter_mut().enumerate() {
                                    // If this is the focused workspace we need to show (restore) all
                                    // windows that were visible since they were probably minimized by
                                    // Windows.
                                    let is_focused_workspace = j == focused_workspace_idx;
                                    let mut empty_containers = Vec::new();
                                    for (idx, container) in
                                        workspace.containers_mut().iter_mut().enumerate()
                                    {
                                        container.windows_mut().retain(|window| {
                                            window.exe().is_ok()
                                                && !known_hwnds.contains_key(&window.hwnd)
                                        });

                                        if container.windows().is_empty() {
                                            empty_containers.push(idx);
                                        }

                                        if is_focused_workspace {
                                            if let Some(window) = container.focused_window()
                                                && WindowsApi::is_window(window.hwnd)
                                            {
                                                tracing::debug!(
                                                    "restoring window: {}",
                                                    window.hwnd
                                                );
                                                WindowsApi::restore_window(window.hwnd);
                                            } else {
                                                // If the focused window was moved or removed by
                                                // the user after the disconnect then focus the
                                                // first window and show that one
                                                container.focus_window(0);

                                                if let Some(window) = container.focused_window()
                                                    && WindowsApi::is_window(window.hwnd)
                                                {
                                                    WindowsApi::restore_window(window.hwnd);
                                                }
                                            }
                                        }
                                    }

                                    // Remove empty containers
                                    remove_empty_containers(workspace, empty_containers);

                                    if let Some(window) = &workspace.maximized_window {
                                        if window.exe().is_err()
                                            || known_hwnds.contains_key(&window.hwnd)
                                        {
                                            workspace.maximized_window = None;
                                        } else if is_focused_workspace
                                            && WindowsApi::is_window(window.hwnd)
                                        {
                                            WindowsApi::restore_window(window.hwnd);
                                        }
                                    }

                                    if let Some(container) = &mut workspace.monocle_container {
                                        container.windows_mut().retain(|window| {
                                            window.exe().is_ok()
                                                && !known_hwnds.contains_key(&window.hwnd)
                                        });

                                        if container.windows().is_empty() {
                                            workspace.monocle_container = None;
                                        } else if is_focused_workspace {
                                            if let Some(window) = container.focused_window()
                                                && WindowsApi::is_window(window.hwnd)
                                            {
                                                WindowsApi::restore_window(window.hwnd);
                                            } else {
                                                // If the focused window was moved or removed by
                                                // the user after the disconnect then focus the
                                                // first window and show that one
                                                container.focus_window(0);

                                                if let Some(window) = container.focused_window()
                                                    && WindowsApi::is_window(window.hwnd)
                                                {
                                                    WindowsApi::restore_window(window.hwnd);
                                                }
                                            }
                                        }
                                    }

                                    workspace.floating_windows_mut().retain(|window| {
                                        window.exe().is_ok()
                                            && !known_hwnds.contains_key(&window.hwnd)
                                    });

                                    if is_focused_workspace {
                                        for window in workspace.floating_windows() {
                                            if WindowsApi::is_window(window.hwnd) {
                                                WindowsApi::restore_window(window.hwnd);
                                            }
                                        }
                                    }

                                    // Apply workspace rules
                                    let mut workspace_matching_rules =
                                        WORKSPACE_MATCHING_RULES.lock();
                                    if let Some(rules) = workspace
                                        .workspace_config
                                        .as_ref()
                                        .and_then(|c| c.workspace_rules.as_ref())
                                    {
                                        for r in rules {
                                            workspace_matching_rules.push(WorkspaceMatchingRule {
                                                monitor_index: i,
                                                workspace_index: j,
                                                matching_rule: r.clone(),
                                                initial_only: false,
                                            });
                                        }
                                    }

                                    if let Some(rules) = workspace
                                        .workspace_config
                                        .as_ref()
                                        .and_then(|c| c.initial_workspace_rules.as_ref())
                                    {
                                        for r in rules {
                                            workspace_matching_rules.push(WorkspaceMatchingRule {
                                                monitor_index: i,
                                                workspace_index: j,
                                                matching_rule: r.clone(),
                                                initial_only: true,
                                            });
                                        }
                                    }
                                }

                                // Restore windows from new monitor and update the focused
                                // workspace
                                m.load_focused_workspace(mouse_follows_focus)?;
                                m.update_focused_workspace(offset)?;
                            }

                            // Entries in the cache should only be used once; remove the entry there was a cache hit
                            if cache_hit && !cached_id.is_empty() {
                                monitor_cache.remove(&cached_id);
                            }
                        }
                    }

                    // Refocus the previously focused monitor since the code above might
                    // steal the focus away.
                    wm.focus_monitor(focused_monitor_idx)?;
                    wm.focus_workspace(focused_workspace_idx)?;
                }

                let final_count = wm.monitors().len();

                if geometry_changed
                    || initial_monitor_count != post_removal_monitor_count
                    || post_removal_monitor_count != final_count
                {
                    wm.retile_all(true)?;
                    // Second retile to fix DPI/resolution related jank
                    wm.retile_all(true)?;
                    // Border updates to fix DPI/resolution related jank
                    border_manager::send_notification(None);
                }
            }
        }

        wm.update_known_hwnds();
        notify_subscribers(
            Notification {
                event: NotificationEvent::Monitor(notification),
                state: wm.as_ref().into(),
            },
            initial_state.has_been_modified(&wm),
        )?;
    }

    Ok(())
}

// Compare identity as well as count: a dock can replace a display in one snapshot.
fn has_detached_monitors(
    current: &std::collections::VecDeque<Monitor>,
    attached: &[Monitor],
) -> bool {
    current
        .iter()
        .any(|monitor| !attached.iter().any(|other| same_device(monitor, other)))
}

fn remove_empty_containers(workspace: &mut crate::workspace::Workspace, empty: Vec<usize>) {
    let selected = workspace
        .focused_container()
        .map(|container| container.id.clone());
    for idx in empty.into_iter().rev() {
        workspace.remove_container_by_idx(idx);
    }
    let focus = selected
        .and_then(|id| {
            workspace
                .containers()
                .iter()
                .position(|container| container.id == id)
        })
        .unwrap_or_else(|| {
            workspace
                .focused_container_idx()
                .min(workspace.containers().len().saturating_sub(1))
        });
    workspace.focus_container(focus);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{container::Container, window::Window, workspace::Workspace};

    fn display(id: &str) -> Monitor {
        let mut monitor = Monitor::placeholder();
        monitor.device_id = id.into();
        monitor.serial_number_id = None;
        monitor
    }

    #[test]
    fn replacement_with_same_count_and_no_serial_is_a_detachment() {
        let current = std::collections::VecDeque::from([display("a"), display("b")]);
        assert!(has_detached_monitors(
            &current,
            &[display("a"), display("c")]
        ));
        assert!(!has_detached_monitors(
            &current,
            &[display("b"), display("a")]
        ));
    }

    #[test]
    fn cached_empty_containers_do_not_remove_surviving_windows() {
        let mut workspace = Workspace::default();
        workspace.add_container_to_back(Container::default());
        workspace.add_container_to_back(Container::default());
        let mut survivor = Container::default();
        survivor.windows_mut().push_back(Window::from(42));
        let selected = survivor.id.clone();
        workspace.add_container_to_back(survivor);
        workspace.resize_dimensions = vec![None, None, None];
        remove_empty_containers(&mut workspace, vec![0, 1]);
        assert_eq!(workspace.containers().len(), 1);
        assert_eq!(workspace.focused_container().unwrap().id, selected);
        assert_eq!(
            workspace
                .focused_container()
                .unwrap()
                .focused_window()
                .unwrap()
                .hwnd,
            42
        );
        assert_eq!(workspace.resize_dimensions.len(), 1);
    }
}
