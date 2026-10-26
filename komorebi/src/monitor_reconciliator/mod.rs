#![deny(clippy::unwrap_used, clippy::expect_used)]

use crate::DISPLAY_INDEX_PREFERENCES;
use crate::DUPLICATE_MONITOR_SERIAL_IDS;
use crate::Notification;
use crate::NotificationEvent;
use crate::WORKSPACE_MATCHING_RULES;
use crate::WindowManager;
use crate::WindowsApi;
use crate::border_manager;
use crate::config_generation::WorkspaceMatchingRule;
use crate::core::Rect;
use crate::monitor;
use crate::monitor::Monitor;
use crate::monitor_reconciliator::hidden::Hidden;
use crate::notify_subscribers;
use crate::state::State;
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_utils::atomic::AtomicConsume;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

pub mod hidden;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", content = "content")]
pub enum MonitorNotification {
    ResolutionScalingChanged,
    WorkAreaChanged,
    DisplayConnectionChange,
    EnteringSuspendedState,
    ResumingFromSuspendedState,
    SessionLocked,
    SessionUnlocked,
}

static ACTIVE: AtomicBool = AtomicBool::new(true);

/// Timestamp (epoch millis) of the last DisplayConnectionChange notification.
/// Used to suppress OS-initiated window minimizes during transient display events.
static LAST_DISPLAY_CHANGE_TIMESTAMP: AtomicI64 = AtomicI64::new(0);

static CHANNEL: OnceLock<(Sender<MonitorNotification>, Receiver<MonitorNotification>)> =
    OnceLock::new();

static MONITOR_CACHE: OnceLock<Mutex<HashMap<String, Monitor>>> = OnceLock::new();

pub fn channel() -> &'static (Sender<MonitorNotification>, Receiver<MonitorNotification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(20))
}

fn event_tx() -> Sender<MonitorNotification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<MonitorNotification> {
    channel().1.clone()
}

pub fn send_notification(notification: MonitorNotification) {
    if matches!(
        notification,
        MonitorNotification::DisplayConnectionChange
            | MonitorNotification::ResumingFromSuspendedState
            | MonitorNotification::SessionUnlocked
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        LAST_DISPLAY_CHANGE_TIMESTAMP.store(now, Ordering::SeqCst);
    }

    if event_tx().try_send(notification).is_err() {
        tracing::warn!("channel is full; dropping notification")
    }
}

/// Returns true if a display connection change event was received within the
/// last `grace_period` duration. This is used by the event processor to avoid
/// treating OS-initiated minimizes (caused by transient monitor disconnects)
/// as user-initiated minimizes.
pub fn display_change_in_progress(grace_period: std::time::Duration) -> bool {
    let last = LAST_DISPLAY_CHANGE_TIMESTAMP.load(Ordering::SeqCst);
    if last == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    (now - last) < grace_period.as_millis() as i64
}

pub fn insert_in_monitor_cache(serial_or_device_id: &str, monitor: Monitor) {
    let dip = DISPLAY_INDEX_PREFERENCES.read();
    let mut dip_ids = dip.values();
    let preferred_id = if dip_ids.any(|id| id.eq(&monitor.device_id)) {
        monitor.device_id.clone()
    } else if dip_ids.any(|id| Some(id) == monitor.serial_number_id.as_ref()) {
        monitor.serial_number_id.clone().unwrap_or_default()
    } else {
        serial_or_device_id.to_string()
    };
    let mut monitor_cache = MONITOR_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock();

    monitor_cache.insert(preferred_id, monitor);
}

pub fn attached_display_devices<F, I>(display_provider: F) -> color_eyre::Result<Vec<Monitor>>
where
    F: Fn() -> I + Copy,
    I: Iterator<Item = Result<win32_display_data::Device, win32_display_data::Error>>,
{
    let mut attempts = 0;

    let (displays, errors) = loop {
        let (displays, errors): (Vec<_>, Vec<_>) = display_provider().partition(Result::is_ok);

        if errors.is_empty() {
            break (displays, errors);
        }

        for err in &errors {
            if let Err(e) = err {
                tracing::warn!(
                    "enumerating display in reconciliator (attempt {}): {:?}",
                    attempts + 1,
                    e
                );
            }
        }

        if attempts < 5 {
            attempts += 1;
            std::thread::sleep(std::time::Duration::from_millis(150));
            continue;
        }

        break (displays, errors);
    };

    if !errors.is_empty() {
        return Err(color_eyre::eyre::eyre!(
            "could not successfully enumerate all displays"
        ));
    }

    let all_displays = displays.into_iter().map(Result::unwrap).collect::<Vec<_>>();

    let mut serial_id_map = HashMap::new();

    for d in &all_displays {
        if let Some(id) = &d.serial_number_id {
            *serial_id_map.entry(id.clone()).or_insert(0) += 1;
        }
    }

    for d in &all_displays {
        if let Some(id) = &d.serial_number_id
            && serial_id_map.get(id).copied().unwrap_or_default() > 1
        {
            let mut dupes = DUPLICATE_MONITOR_SERIAL_IDS.write();
            if !dupes.contains(id) {
                (*dupes).push(id.clone());
            }
        }
    }

    Ok(all_displays
        .into_iter()
        .map(|mut display| {
            let path = display.device_path;

            let (device, device_id) = if path.is_empty() {
                (String::from("UNKNOWN"), String::from("UNKNOWN"))
            } else {
                let mut split: Vec<_> = path.split('#').collect();
                split.remove(0);
                split.remove(split.len() - 1);
                let device = split[0].to_string();
                let device_id = split.join("-");
                (device, device_id)
            };

            let name = display.device_name.trim_start_matches(r"\\.\").to_string();
            let name = name.split('\\').collect::<Vec<_>>()[0].to_string();

            if let Some(id) = &display.serial_number_id {
                let dupes = DUPLICATE_MONITOR_SERIAL_IDS.read();
                if dupes.contains(id) {
                    display.serial_number_id = None;
                }
            }

            monitor::new(
                display.hmonitor,
                display.size.into(),
                display.work_area_size.into(),
                name,
                device,
                device_id,
                display.serial_number_id,
            )
        })
        .collect::<Vec<_>>())
}

pub fn listen_for_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    #[allow(clippy::expect_used)]
    Hidden::create("komorebi-hidden")?;

    tracing::info!("created hidden window to listen for monitor-related events");

    std::thread::spawn(move || {
        loop {
            match handle_notifications(wm.clone(), win32_display_data::connected_displays_all) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    if cfg!(debug_assertions) {
                        tracing::error!("restarting failed thread: {:?}", error)
                    } else {
                        tracing::error!("restarting failed thread: {}", error)
                    }
                }
            }
        }
    });

    Ok(())
}

pub fn handle_notifications<F, I>(
    wm: Arc<Mutex<WindowManager>>,
    display_provider: F,
) -> color_eyre::Result<()>
where
    F: Fn() -> I + Copy,
    I: Iterator<Item = Result<win32_display_data::Device, win32_display_data::Error>>,
{
    handle_notifications_from(wm, display_provider, event_rx())
}

fn handle_notifications_from<F, I>(
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
        if !matches!(
            notification,
            MonitorNotification::EnteringSuspendedState | MonitorNotification::SessionLocked
        ) {
            wm.sync_desktop_count()?;
        }

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
                let mut attached = attached_display_devices(display_provider)?;
                if attached.is_empty() {
                    continue 'receiver;
                }
                if wm
                    .monitors()
                    .iter()
                    .any(|m| !attached.iter().any(|a| same_device(m, a)))
                {
                    // Keep the existing grace period for transient disconnects without
                    // holding WM/cache locks or minimizing any application.
                    drop(wm);
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    attached = attached_display_devices(display_provider)?;
                    wm = wm_arc.lock();
                    if attached.is_empty() {
                        continue 'receiver;
                    }
                }
                let mut cache = MONITOR_CACHE
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock();
                reconcile_attached_monitors(&mut wm, attached, &mut cache)?;
            }
        }

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

pub(crate) fn same_device(a: &Monitor, b: &Monitor) -> bool {
    a.serial_number_id
        .as_ref()
        .filter(|id| !id.is_empty())
        .is_some_and(|id| b.serial_number_id.as_ref() == Some(id))
        || (!a.device_id.is_empty() && a.device_id != "UNKNOWN" && a.device_id == b.device_id)
        || (a.device_id == "UNKNOWN" && b.device_id == "UNKNOWN" && a.id == b.id)
}

/// Reconcile a complete physical monitor snapshot while native desktops own membership.
pub(crate) fn reconcile_attached_monitors(
    wm: &mut WindowManager,
    attached: Vec<Monitor>,
    cache: &mut HashMap<String, Monitor>,
) -> color_eyre::Result<()> {
    use color_eyre::eyre::OptionExt;
    if attached.is_empty() {
        color_eyre::eyre::bail!("refusing an empty monitor snapshot");
    }
    let handles = attached
        .iter()
        .map(|m| m.id)
        .collect::<std::collections::HashSet<_>>();
    if handles.len() != attached.len() {
        color_eyre::eyre::bail!("duplicate monitor handles in snapshot");
    }
    wm.sync_desktop_count()?;
    let previous = State::from(wm.as_ref());
    let old_ids = wm.desktop_ids.clone();
    let old_count = wm.desktop_count;
    let old_focus = wm.focused_desktop_idx;
    let old_names = wm.workspace_names.clone();
    let old_map = wm.monitor_usr_idx_map.clone();
    let focused = wm.focused_monitor().cloned();
    let mut next_cache = cache.clone();
    for monitor in wm.monitors() {
        if !attached.iter().any(|a| same_device(monitor, a)) {
            let preferred = DISPLAY_INDEX_PREFERENCES
                .read()
                .values()
                .find(|id| {
                    **id == monitor.device_id || monitor.serial_number_id.as_ref() == Some(*id)
                })
                .cloned()
                .unwrap_or_else(|| {
                    monitor
                        .serial_number_id
                        .clone()
                        .unwrap_or_else(|| monitor.device_id.clone())
                });
            next_cache.insert(preferred, monitor.clone());
        }
    }
    let mut ordered = std::collections::BTreeMap::new();
    let mut unpreferred = Vec::new();
    for metadata in attached {
        let existing = wm
            .monitors()
            .iter()
            .find(|m| same_device(m, &metadata))
            .cloned();
        let cached_key = next_cache
            .keys()
            .find(|id| {
                **id == metadata.device_id || metadata.serial_number_id.as_ref() == Some(*id)
            })
            .cloned();
        let mut monitor = existing
            .clone()
            .or_else(|| {
                cached_key
                    .as_ref()
                    .and_then(|key| next_cache.get(key).cloned())
            })
            .unwrap_or_else(|| metadata.clone());
        if existing.is_none() {
            // The current association takes precedence over stale windows in a cache.
            for workspace in monitor.workspaces_mut() {
                for hwnd in WindowManager::workspace_hwnds(workspace) {
                    if wm.known_hwnds.contains_key(&hwnd) {
                        workspace.detach_window_for_desktop_move(hwnd);
                    }
                }
            }
        }
        monitor.id = metadata.id;
        monitor.name = metadata.name;
        monitor.device = metadata.device;
        monitor.device_id = metadata.device_id;
        monitor.serial_number_id = metadata.serial_number_id;
        monitor.size = metadata.size;
        monitor.work_area_size = metadata.work_area_size;
        if let Some(key) = cached_key {
            next_cache.remove(&key);
        }
        let preference = DISPLAY_INDEX_PREFERENCES
            .read()
            .iter()
            .find_map(|(&i, id)| {
                (monitor.device_id == *id || monitor.serial_number_id.as_ref() == Some(id))
                    .then_some(i)
            })
            .or_else(|| {
                crate::MONITOR_INDEX_PREFERENCES
                    .lock()
                    .iter()
                    .find_map(|(&i, area)| (*area == monitor.size).then_some(i))
            });
        if let Some(i) = preference
            && !ordered.contains_key(&i)
        {
            ordered.insert(i, monitor);
        } else {
            unpreferred.push(monitor);
        }
    }
    for monitor in unpreferred {
        let idx = (0..)
            .find(|i| !ordered.contains_key(i))
            .ok_or_eyre("no free monitor index")?;
        ordered.insert(idx, monitor);
    }
    wm.monitor_usr_idx_map = ordered
        .keys()
        .enumerate()
        .map(|(physical, &user)| (user, physical))
        .collect();
    *wm.monitors_mut() = ordered.into_values().collect();
    let selected = focused
        .as_ref()
        .and_then(|old| wm.monitors().iter().position(|m| same_device(old, m)))
        .unwrap_or(0);
    wm.focus_monitor(selected)?;
    let result = wm
        .sync_desktop_count()
        .and_then(|_| wm.preserve_windows_after_monitor_change(&previous));
    if let Err(error) = result {
        wm.monitors = previous.monitors;
        wm.monitor_usr_idx_map = old_map;
        wm.desktop_ids = old_ids;
        wm.desktop_count = old_count;
        wm.focused_desktop_idx = old_focus;
        wm.workspace_names = old_names;
        wm.update_known_hwnds();
        return Err(error);
    }
    // Preserve external rules for surviving physical monitors, remapping their indices.
    let mut rules = WORKSPACE_MATCHING_RULES.lock();
    rules.retain_mut(|rule| {
        let target = previous
            .monitors
            .elements()
            .get(rule.monitor_index)
            .and_then(|old| wm.monitors().iter().position(|m| same_device(old, m)));
        if let Some(index) = target
            && rule.workspace_index < wm.desktop_count
        {
            rule.monitor_index = index;
            true
        } else {
            false
        }
    });
    for (m, monitor) in wm.monitors().iter().enumerate() {
        for (d, workspace) in monitor.workspaces().iter().enumerate() {
            if let Some(config) = &workspace.workspace_config {
                for (initial, matching) in [
                    (false, config.workspace_rules.as_ref()),
                    (true, config.initial_workspace_rules.as_ref()),
                ] {
                    if let Some(matching) = matching {
                        for matching_rule in matching {
                            let rule = WorkspaceMatchingRule {
                                monitor_index: m,
                                workspace_index: d,
                                matching_rule: matching_rule.clone(),
                                initial_only: initial,
                            };
                            if !rules.contains(&rule) {
                                rules.push(rule);
                            }
                        }
                    }
                }
            }
        }
    }
    drop(rules);
    *cache = next_cache;
    wm.update_known_hwnds();
    border_manager::send_notification(None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_manager_event::WindowManagerEvent;
    use crossbeam_channel::Sender;
    use crossbeam_channel::bounded;
    use std::path::PathBuf;
    use uuid::Uuid;
    use windows::Win32::Devices::Display::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY;
    // NOTE: Using RECT instead of RECT since I get a mismatched type error. Can be updated if
    // needed.
    use windows::Win32::Foundation::RECT;

    // Creating a Mock Display Provider
    #[derive(Clone)]
    struct MockDevice {
        hmonitor: isize,
        device_path: String,
        device_name: String,
        device_description: String,
        serial_number_id: Option<String>,
        size: RECT,
        work_area_size: RECT,
        device_key: String,
        output_technology: Option<DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY>,
    }

    impl From<MockDevice> for win32_display_data::Device {
        fn from(mock: MockDevice) -> Self {
            win32_display_data::Device {
                hmonitor: mock.hmonitor,
                device_path: mock.device_path,
                device_name: mock.device_name,
                device_description: mock.device_description,
                serial_number_id: mock.serial_number_id,
                size: mock.size,
                work_area_size: mock.work_area_size,
                device_key: mock.device_key,
                output_technology: mock.output_technology,
            }
        }
    }

    // Creating a Window Manager Instance
    struct TestContext {
        socket_path: Option<PathBuf>,
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            if let Some(socket_path) = &self.socket_path {
                // Clean up the socket file
                if let Err(e) = std::fs::remove_file(socket_path) {
                    tracing::warn!("Failed to remove socket file: {}", e);
                }
            }
        }
    }

    fn setup_window_manager() -> (WindowManager, TestContext) {
        let (_sender, receiver): (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) =
            bounded(1);

        // Temporary socket path for testing
        let socket_name = format!("komorebi-test-{}.sock", Uuid::new_v4());
        let socket_path = PathBuf::from(socket_name);

        // Create a new WindowManager instance
        let wm = match WindowManager::new(receiver, Some(socket_path.clone())) {
            Ok(manager) => manager,
            Err(e) => {
                panic!("Failed to create WindowManager: {e}");
            }
        };

        (
            wm,
            TestContext {
                socket_path: Some(socket_path),
            },
        )
    }

    #[test]
    fn test_send_notification() {
        // Create a monitor notification
        let notification = MonitorNotification::ResolutionScalingChanged;

        // Use the send_notification function to send the notification
        send_notification(notification);

        // Receive the notification from the channel
        let received = event_rx().try_recv();

        // Check if we received the notification and if it matches what we sent
        match received {
            Ok(notification) => {
                assert_eq!(notification, MonitorNotification::ResolutionScalingChanged);
            }
            Err(e) => panic!("Failed to receive MonitorNotification: {e}"),
        }
    }

    #[test]
    fn test_channel_bounded_capacity() {
        let (_, receiver) = channel();

        // Fill the channel to its capacity (20 messages)
        for _ in 0..20 {
            send_notification(MonitorNotification::WorkAreaChanged);
        }

        // Attempt to send another message (should be dropped)
        send_notification(MonitorNotification::ResolutionScalingChanged);

        // Verify the channel contains only the first 20 messages
        for _ in 0..20 {
            let notification = match receiver.try_recv() {
                Ok(notification) => notification,
                Err(e) => panic!("Failed to receive MonitorNotification: {e}"),
            };
            assert_eq!(
                notification,
                MonitorNotification::WorkAreaChanged,
                "Unexpected notification in the channel"
            );
        }

        // Verify that no additional messages are in the channel
        assert!(
            receiver.try_recv().is_err(),
            "Channel should be empty after consuming all messages"
        );
    }

    #[test]
    fn test_insert_in_monitor_cache() {
        let m = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "Test Monitor".to_string(),
            "Test Device".to_string(),
            "Test Device ID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        // Insert the monitor into the cache
        insert_in_monitor_cache("TestMonitorID", m.clone());

        // Retrieve the monitor from the cache
        let cache = MONITOR_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock();
        let retrieved_monitor = cache.get("TestMonitorID");

        // Check that the monitor was inserted correctly and matches the expected value
        assert_eq!(retrieved_monitor, Some(&m));
    }

    #[test]
    fn test_insert_two_monitors_cache() {
        let m1 = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "Test Monitor".to_string(),
            "Test Device".to_string(),
            "Test Device ID".to_string(),
            Some("TestMonitorID".to_string()),
        );

        let m2 = monitor::new(
            0,
            Rect::default(),
            Rect::default(),
            "Test Monitor 2".to_string(),
            "Test Device 2".to_string(),
            "Test Device ID 2".to_string(),
            Some("TestMonitorID2".to_string()),
        );

        // Insert the first monitor into the cache
        insert_in_monitor_cache("TestMonitorID", m1.clone());

        // Insert the second monitor into the cache
        insert_in_monitor_cache("TestMonitorID2", m2.clone());

        // Retrieve the cache to check if the first and second monitors are present
        let cache = MONITOR_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock();

        // Check if Monitor 1 was found in the cache
        assert_eq!(
            cache.get("TestMonitorID"),
            Some(&m1),
            "Monitor cache should contain monitor 1"
        );

        // Check if Monitor 2 was found in the cache
        assert_eq!(
            cache.get("TestMonitorID2"),
            Some(&m2),
            "Monitor cache should contain monitor 2"
        );
    }

    #[test]
    fn test_listen_for_notifications() {
        let (wm, _test_context) = setup_window_manager();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let listener = std::thread::spawn(move || {
            handle_notifications_from(
                Arc::new(Mutex::new(wm)),
                std::iter::empty::<Result<win32_display_data::Device, win32_display_data::Error>>,
                receiver,
            )
        });
        // The active test listener must not consume any message from the production channel.
        for _ in 0..20 {
            test_send_notification();
        }
        // Disconnecting the private channel must stop the loop and release the WM.
        drop(sender);
        listener
            .join()
            .expect("listener panicked")
            .expect("listener failed");
    }

    #[test]
    fn test_attached_display_devices() {
        // Define mock display data
        let mock_monitor = MockDevice {
            hmonitor: 1,
            device_path: String::from(
                "\\\\?\\DISPLAY#ABC123#4&123456&0&UID0#{saucepackets-4321-5678-2468-abc123456789}",
            ),
            device_name: String::from("\\\\.\\DISPLAY1"),
            device_description: String::from("Display description"),
            serial_number_id: Some(String::from("SaucePackets123")),
            device_key: String::from("Mock Key"),
            size: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            work_area_size: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            output_technology: Some(DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY(0)),
        };

        // Create a closure to simulate the display provider
        let display_provider = || {
            vec![Ok::<win32_display_data::Device, win32_display_data::Error>(
                win32_display_data::Device::from(mock_monitor.clone()),
            )]
            .into_iter()
        };

        // Should contain the mock monitor
        let result = attached_display_devices(display_provider).ok();
        if let Some(monitors) = result {
            // Check Number of monitors
            assert_eq!(monitors.len(), 1, "Expected one monitor");

            // hmonitor
            assert_eq!(monitors[0].id, 1);

            // device name
            assert_eq!(monitors[0].name, String::from("DISPLAY1"));

            // Device
            assert_eq!(monitors[0].device, String::from("ABC123"));

            // Device ID
            assert_eq!(
                monitors[0].device_id,
                String::from("ABC123-4&123456&0&UID0")
            );

            // Check monitor serial number id
            assert_eq!(
                monitors[0].serial_number_id,
                Some(String::from("SaucePackets123")),
            );
        } else {
            panic!("No monitors found");
        }
    }
}
