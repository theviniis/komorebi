//! The only module in komorebi allowed to talk to the Windows virtual desktop API.
//!
//! Everything else goes through [`DesktopBackend`], which is [`WinvdBackend`] in
//! production and a fake backend in tests.

use color_eyre::eyre;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;
use windows_058::Win32::Foundation::HWND as WinvdHwnd;

/// Stable native identity; indices are only positions in a snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DesktopId(pub u128);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopInfo {
    pub id: DesktopId,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopSnapshot {
    pub desktops: Vec<DesktopInfo>,
    pub current: DesktopId,
}

pub trait DesktopBackend: Send + Sync {
    fn snapshot(&self) -> eyre::Result<DesktopSnapshot>;
    fn current_desktop_id(&self) -> eyre::Result<DesktopId>;
    fn desktop_id_for_window(&self, hwnd: isize) -> eyre::Result<DesktopId>;
    fn desktop_count(&self) -> eyre::Result<usize>;
    fn current_desktop_idx(&self) -> eyre::Result<usize>;
    fn switch_desktop(&self, idx: usize) -> eyre::Result<()>;
    fn switch_desktop_id(&self, id: DesktopId) -> eyre::Result<()> {
        let snapshot = self.snapshot()?;
        let idx = snapshot
            .desktops
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| eyre::eyre!("desktop identity no longer exists"))?;
        self.switch_desktop(idx)
    }
    fn move_window_to_desktop_id(&self, hwnd: isize, id: DesktopId) -> eyre::Result<()> {
        let snapshot = self.snapshot()?;
        let idx = snapshot
            .desktops
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| eyre::eyre!("desktop identity no longer exists"))?;
        self.move_window_to_desktop(hwnd, idx)
    }
    fn desktop_idx_for_window(&self, hwnd: isize) -> eyre::Result<usize>;
    fn move_window_to_desktop(&self, hwnd: isize, idx: usize) -> eyre::Result<()>;
    fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>>;
    fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()>;
    fn set_desktop_name_id(&self, id: DesktopId, name: &str) -> eyre::Result<()> {
        let snapshot = self.snapshot()?;
        let idx = snapshot
            .desktops
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| eyre::eyre!("desktop identity no longer exists"))?;
        self.set_desktop_name(idx, name)
    }
}

/// Real backend, delegating to `winvd`.
#[derive(Clone, Copy, Debug, Default)]
pub struct WinvdBackend;

static DEFAULT_BACKEND: OnceLock<Arc<dyn DesktopBackend>> = OnceLock::new();

fn installed_backend() -> &'static Mutex<Option<Arc<dyn DesktopBackend>>> {
    static INSTALLED_BACKEND: OnceLock<Mutex<Option<Arc<dyn DesktopBackend>>>> = OnceLock::new();
    INSTALLED_BACKEND.get_or_init(|| Mutex::new(None))
}

/// Installs a backend for tests, or restores the real backend with `None`.
pub fn set_backend(b: Option<Arc<dyn DesktopBackend>>) {
    *installed_backend().lock() = b;
}

/// Returns the installed backend, or the real one if none was installed.
#[must_use]
pub fn backend() -> Arc<dyn DesktopBackend> {
    let installed = installed_backend().lock().clone();
    if let Some(backend) = installed {
        return backend;
    }

    DEFAULT_BACKEND
        .get_or_init(|| Arc::new(WinvdBackend))
        .clone()
}

/// `winvd` reports desktops as `u32`, komorebi uses `usize` throughout.
#[must_use]
pub fn desktop_idx_from_winvd(v: u32) -> usize {
    v as usize
}

/// `winvd` accepts `i32` desktop indices.
pub fn desktop_idx_to_winvd(idx: usize) -> eyre::Result<i32> {
    i32::try_from(idx)
        .map_err(|_| eyre::eyre!("desktop index {idx} does not fit into a winvd desktop index"))
}

/// Logs before the error is propagated, see the crate conventions.
fn winvd_error(context: &str, error: winvd::Error) -> eyre::Report {
    tracing::error!("{context}: {error:?}");
    eyre::eyre!("{context}: {error:?}")
}

/// `winvd` links against `windows` 0.58 while komorebi uses 0.62, so the handle
/// has to be built from the plain `isize` komorebi passes around. This is the
/// only place in komorebi that ever names the foreign `HWND` type.
fn winvd_hwnd(hwnd: isize) -> WinvdHwnd {
    WinvdHwnd(hwnd as *mut _)
}

impl DesktopBackend for WinvdBackend {
    fn set_desktop_name_id(&self, id: DesktopId, name: &str) -> eyre::Result<()> {
        winvd::get_desktop(windows_058::core::GUID::from_u128(id.0))
            .set_name(name)
            .map_err(|e| winvd_error("failed to name desktop by identity", e))
    }

    fn switch_desktop_id(&self, id: DesktopId) -> eyre::Result<()> {
        winvd::switch_desktop(windows_058::core::GUID::from_u128(id.0))
            .map_err(|e| winvd_error("failed to switch desktop by identity", e))
    }
    fn move_window_to_desktop_id(&self, hwnd: isize, id: DesktopId) -> eyre::Result<()> {
        winvd::move_window_to_desktop(windows_058::core::GUID::from_u128(id.0), &winvd_hwnd(hwnd))
            .map_err(|e| winvd_error("failed to move window by desktop identity", e))
    }

    fn current_desktop_id(&self) -> eyre::Result<DesktopId> {
        winvd::get_current_desktop()
            .and_then(|d| d.get_id())
            .map(|id| DesktopId(id.to_u128()))
            .map_err(|e| winvd_error("failed to identify current desktop", e))
    }

    fn snapshot(&self) -> eyre::Result<DesktopSnapshot> {
        // Retry a changing native list instead of mixing positions from different orders.
        for _ in 0..3 {
            let read = || -> Result<Vec<DesktopInfo>, winvd::Error> {
                winvd::get_desktops()?
                    .into_iter()
                    .map(|desktop| {
                        let id = DesktopId(desktop.get_id()?.to_u128());
                        let name = desktop.get_name()?;
                        Ok(DesktopInfo {
                            id,
                            name: (!name.is_empty()).then_some(name),
                        })
                    })
                    .collect()
            };
            let desktops = read().map_err(|e| winvd_error("failed to list desktops", e))?;
            let current = DesktopId(
                winvd::get_current_desktop()
                    .and_then(|d| d.get_id())
                    .map_err(|e| winvd_error("failed to identify current desktop", e))?
                    .to_u128(),
            );
            let verified = read().map_err(|e| winvd_error("failed to verify desktop list", e))?;
            let verified_current = DesktopId(
                winvd::get_current_desktop()
                    .and_then(|d| d.get_id())
                    .map_err(|e| winvd_error("failed to verify current desktop", e))?
                    .to_u128(),
            );
            if desktops == verified
                && current == verified_current
                && desktops.iter().any(|d| d.id == current)
            {
                return Ok(DesktopSnapshot { desktops, current });
            }
        }
        eyre::bail!("native desktop list changed while reading its snapshot")
    }

    fn desktop_id_for_window(&self, hwnd: isize) -> eyre::Result<DesktopId> {
        winvd::get_desktop_by_window(winvd_hwnd(hwnd))
            .and_then(|d| d.get_id())
            .map(|id| DesktopId(id.to_u128()))
            .map_err(|e| winvd_error("failed to identify window desktop", e))
    }

    fn desktop_count(&self) -> eyre::Result<usize> {
        winvd::get_desktop_count()
            .map(desktop_idx_from_winvd)
            .map_err(|error| winvd_error("failed to get the desktop count", error))
    }

    fn current_desktop_idx(&self) -> eyre::Result<usize> {
        winvd::get_current_desktop()
            .and_then(|desktop| desktop.get_index())
            .map(desktop_idx_from_winvd)
            .map_err(|error| winvd_error("failed to get the current desktop", error))
    }

    fn switch_desktop(&self, idx: usize) -> eyre::Result<()> {
        winvd::switch_desktop(desktop_idx_to_winvd(idx)?)
            .map_err(|error| winvd_error(&format!("failed to switch to desktop {idx}"), error))
    }

    fn desktop_idx_for_window(&self, hwnd: isize) -> eyre::Result<usize> {
        winvd::get_desktop_by_window(winvd_hwnd(hwnd))
            .and_then(|desktop| desktop.get_index())
            .map(desktop_idx_from_winvd)
            .map_err(|error| {
                winvd_error(
                    &format!("failed to get the desktop of window {hwnd}"),
                    error,
                )
            })
    }

    fn move_window_to_desktop(&self, hwnd: isize, idx: usize) -> eyre::Result<()> {
        winvd::move_window_to_desktop(desktop_idx_to_winvd(idx)?, &winvd_hwnd(hwnd)).map_err(
            |error| {
                winvd_error(
                    &format!("failed to move window {hwnd} to desktop {idx}"),
                    error,
                )
            },
        )
    }

    fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>> {
        let name = winvd::get_desktop(desktop_idx_to_winvd(idx)?)
            .get_name()
            .map_err(|error| {
                winvd_error(&format!("failed to get the name of desktop {idx}"), error)
            })?;

        Ok(if name.is_empty() { None } else { Some(name) })
    }

    fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()> {
        winvd::get_desktop(desktop_idx_to_winvd(idx)?)
            .set_name(name)
            .map_err(|error| {
                winvd_error(&format!("failed to set the name of desktop {idx}"), error)
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopEventKind {
    Changed {
        new: usize,
        old: usize,
    },
    CountChanged,
    WindowChanged {
        hwnd: isize,
    },
    Moved {
        /// The desktop's new index. Always equal to `new`; it exists as a named
        /// field for ergonomics at call sites.
        desktop: usize,
        old: usize,
        new: usize,
    },
    Renamed {
        desktop: usize,
    },
}

static EVENT_THREAD: OnceLock<winvd::DesktopEventThread> = OnceLock::new();

fn event_thread_start_lock() -> &'static Mutex<()> {
    static EVENT_THREAD_START: OnceLock<Mutex<()>> = OnceLock::new();
    EVENT_THREAD_START.get_or_init(|| Mutex::new(()))
}

/// Starts listening for native desktop events, translating them to
/// [`DesktopEventKind`] and invoking `cb` on a dedicated thread.
///
/// This is the only place in komorebi that touches
/// `winvd::listen_desktop_events`.
pub fn spawn_event_thread(cb: impl Fn(DesktopEventKind) + Send + 'static) -> eyre::Result<()> {
    // The listener has to be created before the `OnceLock` slot can be
    // claimed, so the check-then-start-then-set sequence is serialized behind a
    // lock. Without it two concurrent callers could both pass the check and
    // start listeners, and the loser's listener would leak after `set` fails.
    let _start_guard = event_thread_start_lock().lock();

    if EVENT_THREAD.get().is_some() {
        eyre::bail!("the desktop event thread is already running");
    }

    let (tx, rx) = std::sync::mpsc::channel::<winvd::DesktopEvent>();
    let thread = winvd::listen_desktop_events(tx)
        .map_err(|error| winvd_error("failed to listen for desktop events", error))?;

    if EVENT_THREAD.set(thread).is_err() {
        eyre::bail!("the desktop event thread is already running");
    }

    std::thread::spawn(move || {
        for event in rx {
            if let Some(kind) = desktop_event_kind(event) {
                cb(kind);
            }
        }
    });

    Ok(())
}

fn desktop_event_kind(event: winvd::DesktopEvent) -> Option<DesktopEventKind> {
    match event {
        winvd::DesktopEvent::DesktopChanged { new, old } => Some(DesktopEventKind::Changed {
            new: event_desktop_idx(&new).unwrap_or_default(),
            old: event_desktop_idx(&old).unwrap_or_default(),
        }),
        winvd::DesktopEvent::DesktopCreated(_) | winvd::DesktopEvent::DesktopDestroyed { .. } => {
            Some(DesktopEventKind::CountChanged)
        }
        // After the move the desktop object resolves to `new_index`, which is
        // also the position its workspace ends up in.
        winvd::DesktopEvent::DesktopMoved {
            old_index,
            new_index,
            ..
        } => {
            let old = usize::try_from(old_index).ok()?;
            let new = usize::try_from(new_index).ok()?;

            Some(DesktopEventKind::Moved {
                desktop: new,
                old,
                new,
            })
        }
        winvd::DesktopEvent::DesktopNameChanged(desktop, _) => Some(DesktopEventKind::Renamed {
            desktop: event_desktop_idx(&desktop).unwrap_or_default(),
        }),
        winvd::DesktopEvent::WindowChanged(hwnd) => Some(DesktopEventKind::WindowChanged {
            hwnd: hwnd.0 as isize,
        }),
        winvd::DesktopEvent::DesktopWallpaperChanged(..) => None,
    }
}

fn event_desktop_idx(desktop: &winvd::Desktop) -> Option<usize> {
    match desktop.get_index() {
        Ok(idx) => Some(desktop_idx_from_winvd(idx)),
        Err(error) => {
            tracing::error!("failed to read the index of a desktop event: {error:?}");
            None
        }
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct FakeState {
        current: usize,
        ids: Vec<DesktopId>,
        next_id: u128,
        snapshot_override: Option<DesktopSnapshot>,
        switch_calls: usize,
        window_desktops: HashMap<isize, usize>,
        moved: Vec<(isize, usize)>,
        names: HashMap<usize, String>,
        fail_move: Option<isize>,
        fail_after_move: Option<isize>,
        reorder_after_move: Option<(usize, usize)>,
    }

    /// In-memory [`DesktopBackend`] for tests.
    #[derive(Debug)]
    pub struct FakeDesktopBackend {
        state: Mutex<FakeState>,
    }

    impl FakeDesktopBackend {
        #[must_use]
        pub fn new(count: usize) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    ids: (1..=count as u128).map(DesktopId).collect(),
                    next_id: count as u128 + 1,
                    ..Default::default()
                }),
            }
        }

        pub fn set_window_desktop(&self, hwnd: isize, idx: usize) {
            self.state.lock().window_desktops.insert(hwnd, idx);
        }

        pub fn set_current_desktop(&self, idx: usize) {
            self.ensure_desktop_exists(idx).unwrap();
            self.state.lock().current = idx;
        }

        pub fn create_desktop(&self, idx: usize) -> DesktopId {
            let mut state = self.state.lock();
            let id = DesktopId(state.next_id);
            state.next_id += 1;
            state.ids.insert(idx, id);
            if state.current >= idx {
                state.current += 1;
            }
            for value in state.window_desktops.values_mut() {
                if *value >= idx {
                    *value += 1;
                }
            }
            state.names = std::mem::take(&mut state.names)
                .into_iter()
                .map(|(i, n)| (if i >= idx { i + 1 } else { i }, n))
                .collect();
            id
        }

        pub fn move_desktop(&self, old: usize, new: usize) {
            let mut state = self.state.lock();
            let remap = |i: usize| {
                if i == old {
                    new
                } else if old < new && i > old && i <= new {
                    i - 1
                } else if new < old && i >= new && i < old {
                    i + 1
                } else {
                    i
                }
            };
            let id = state.ids.remove(old);
            state.ids.insert(new, id);
            state.current = remap(state.current);
            for value in state.window_desktops.values_mut() {
                *value = remap(*value);
            }
            state.names = std::mem::take(&mut state.names)
                .into_iter()
                .map(|(i, n)| (remap(i), n))
                .collect();
        }

        /// fallback is expressed in the order before removal, just as native identity is.
        pub fn destroy_desktop(&self, idx: usize, fallback: usize) {
            let mut state = self.state.lock();
            assert!(state.ids.len() > 1 && idx != fallback);
            let remap = |i: usize| {
                let i = if i == idx { fallback } else { i };
                if i > idx { i - 1 } else { i }
            };
            state.ids.remove(idx);
            state.current = remap(state.current);
            for value in state.window_desktops.values_mut() {
                *value = remap(*value);
            }
            state.names = std::mem::take(&mut state.names)
                .into_iter()
                .filter(|(i, _)| *i != idx)
                .map(|(i, n)| (remap(i), n))
                .collect();
        }

        pub fn set_snapshot_override(&self, snapshot: Option<DesktopSnapshot>) {
            self.state.lock().snapshot_override = snapshot;
        }

        pub fn reorder_after_next_move(&self, old: usize, new: usize) {
            self.state.lock().reorder_after_move = Some((old, new));
        }

        pub fn fail_move_for(&self, hwnd: isize) {
            self.state.lock().fail_move = Some(hwnd);
        }

        pub fn fail_after_move_for(&self, hwnd: isize) {
            self.state.lock().fail_after_move = Some(hwnd);
        }

        #[must_use]
        pub fn switch_calls(&self) -> usize {
            self.state.lock().switch_calls
        }

        #[must_use]
        pub fn moved(&self) -> Vec<(isize, usize)> {
            self.state.lock().moved.clone()
        }

        #[must_use]
        pub fn names(&self) -> HashMap<usize, String> {
            self.state.lock().names.clone()
        }

        fn ensure_desktop_exists(&self, idx: usize) -> eyre::Result<()> {
            let count = self.state.lock().ids.len();
            if idx >= count {
                eyre::bail!("desktop {idx} does not exist (count: {})", count);
            }

            Ok(())
        }
    }

    impl DesktopBackend for FakeDesktopBackend {
        fn current_desktop_id(&self) -> eyre::Result<DesktopId> {
            let state = self.state.lock();
            state
                .ids
                .get(state.current)
                .copied()
                .ok_or_else(|| eyre::eyre!("invalid fake current desktop"))
        }
        fn snapshot(&self) -> eyre::Result<DesktopSnapshot> {
            let state = self.state.lock();
            if let Some(snapshot) = &state.snapshot_override {
                return Ok(snapshot.clone());
            }
            let current = *state
                .ids
                .get(state.current)
                .ok_or_else(|| eyre::eyre!("invalid fake current desktop"))?;
            Ok(DesktopSnapshot {
                current,
                desktops: state
                    .ids
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| DesktopInfo {
                        id,
                        name: state.names.get(&i).filter(|s| !s.is_empty()).cloned(),
                    })
                    .collect(),
            })
        }
        fn desktop_id_for_window(&self, hwnd: isize) -> eyre::Result<DesktopId> {
            let state = self.state.lock();
            state
                .window_desktops
                .get(&hwnd)
                .and_then(|&i| state.ids.get(i))
                .copied()
                .ok_or_else(|| eyre::eyre!("window {hwnd} is not on any desktop"))
        }
        fn desktop_count(&self) -> eyre::Result<usize> {
            Ok(self.state.lock().ids.len())
        }

        fn current_desktop_idx(&self) -> eyre::Result<usize> {
            Ok(self.state.lock().current)
        }

        fn switch_desktop(&self, idx: usize) -> eyre::Result<()> {
            self.ensure_desktop_exists(idx)?;

            let mut state = self.state.lock();
            state.current = idx;
            state.switch_calls += 1;

            Ok(())
        }

        fn desktop_idx_for_window(&self, hwnd: isize) -> eyre::Result<usize> {
            self.state
                .lock()
                .window_desktops
                .get(&hwnd)
                .copied()
                .ok_or_else(|| eyre::eyre!("window {hwnd} is not on any desktop"))
        }

        fn move_window_to_desktop(&self, hwnd: isize, idx: usize) -> eyre::Result<()> {
            self.ensure_desktop_exists(idx)?;

            let mut state = self.state.lock();
            if state.fail_move == Some(hwnd) {
                eyre::bail!("injected move failure for {hwnd}");
            }
            state.window_desktops.insert(hwnd, idx);
            state.moved.push((hwnd, idx));
            let fail_after = state.fail_after_move == Some(hwnd);
            let reorder = state.reorder_after_move.take();
            drop(state);
            if let Some((old, new)) = reorder {
                self.move_desktop(old, new);
            }
            if fail_after {
                eyre::bail!("injected error after native move for {hwnd}");
            }

            Ok(())
        }

        fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>> {
            self.ensure_desktop_exists(idx)?;
            Ok(self.state.lock().names.get(&idx).cloned())
        }

        fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()> {
            self.ensure_desktop_exists(idx)?;
            self.state.lock().names.insert(idx, name.to_string());

            Ok(())
        }
    }

    /// Restores the previous backend when dropped, so a failed assertion
    /// cannot leave a fake installed for other tests.
    pub struct InstalledBackendGuard {
        previous: Option<Arc<dyn DesktopBackend>>,
    }

    impl InstalledBackendGuard {
        pub fn new() -> Self {
            let previous = installed_backend().lock().clone();
            Self { previous }
        }
    }

    impl Drop for InstalledBackendGuard {
        fn drop(&mut self) {
            *installed_backend().lock() = self.previous.clone();
        }
    }

    impl Default for InstalledBackendGuard {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn fake_native_lifecycle_keeps_identities_independent_from_indices() {
        let fake = FakeDesktopBackend::new(3);
        fake.set_window_desktop(9, 1);
        fake.set_current_desktop(1);
        fake.move_desktop(0, 2);
        let snapshot = fake.snapshot().unwrap();
        assert_eq!(
            snapshot.desktops.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![DesktopId(2), DesktopId(3), DesktopId(1)]
        );
        assert_eq!(snapshot.current, DesktopId(2));
        assert_eq!(fake.desktop_id_for_window(9).unwrap(), DesktopId(2));
        let created = fake.create_desktop(1);
        assert_eq!(created, DesktopId(4));
        fake.destroy_desktop(0, 2);
        let snapshot = fake.snapshot().unwrap();
        assert_eq!(
            snapshot.desktops.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![DesktopId(4), DesktopId(3), DesktopId(1)]
        );
        assert_eq!(snapshot.current, DesktopId(3));
        assert_eq!(fake.desktop_id_for_window(9).unwrap(), DesktopId(3));
        assert!(fake.moved().is_empty());
        assert_eq!(fake.switch_calls(), 0);
    }

    #[test]
    fn desktop_index_conversions_roundtrip() {
        assert_eq!(desktop_idx_from_winvd(0), 0);
        assert_eq!(desktop_idx_to_winvd(2).unwrap(), 2);
        assert!(desktop_idx_to_winvd(usize::MAX).is_err());
    }

    #[test]
    fn fake_backend_records_operations() {
        let fake = FakeDesktopBackend::new(3);
        fake.set_window_desktop(7, 2);
        assert_eq!(fake.desktop_count().unwrap(), 3);
        assert_eq!(fake.desktop_idx_for_window(7).unwrap(), 2);
        fake.switch_desktop(1).unwrap();
        assert_eq!(fake.switch_calls(), 1);
    }

    #[test]
    fn fake_backend_records_moves_and_names() {
        let fake = FakeDesktopBackend::new(2);
        fake.move_window_to_desktop(11, 1).unwrap();
        fake.set_desktop_name(1, "web").unwrap();
        assert_eq!(fake.moved(), vec![(11, 1)]);
        assert_eq!(fake.names(), HashMap::from([(1, "web".to_string())]));
    }

    #[test]
    fn fake_backend_rejects_unknown_desktops() {
        let fake = FakeDesktopBackend::new(2);
        assert!(fake.switch_desktop(2).is_err());
        assert!(fake.move_window_to_desktop(11, 2).is_err());
        assert!(fake.desktop_idx_for_window(11).is_err());
        assert!(fake.desktop_name(2).is_err());
    }

    /// Restores the real backend even if the test panics, so a failed assertion
    /// cannot leave a fake installed for every other test in the crate.
    struct InstalledBackendGuard;

    impl Drop for InstalledBackendGuard {
        fn drop(&mut self) {
            set_backend(None);
        }
    }

    #[test]
    fn installed_backend_is_returned_by_backend() {
        let fake: Arc<dyn DesktopBackend> = Arc::new(FakeDesktopBackend::new(1));
        set_backend(Some(fake.clone()));
        let _guard = InstalledBackendGuard;
        assert!(Arc::ptr_eq(&backend(), &fake));
    }

    #[test]
    fn desktop_events_are_translated_to_kinds() {
        assert_eq!(
            desktop_event_kind(winvd::DesktopEvent::WindowChanged(winvd_hwnd(42))),
            Some(DesktopEventKind::WindowChanged { hwnd: 42 })
        );
        assert_eq!(
            desktop_event_kind(winvd::DesktopEvent::DesktopMoved {
                desktop: winvd::get_desktop(0),
                old_index: 0,
                new_index: 2,
            }),
            Some(DesktopEventKind::Moved {
                desktop: 2,
                old: 0,
                new: 2,
            })
        );
        assert_eq!(
            desktop_event_kind(winvd::DesktopEvent::DesktopCreated(winvd::get_desktop(0))),
            Some(DesktopEventKind::CountChanged)
        );
        assert_eq!(
            desktop_event_kind(winvd::DesktopEvent::DesktopDestroyed {
                destroyed: winvd::get_desktop(0),
                fallback: winvd::get_desktop(0),
            }),
            Some(DesktopEventKind::CountChanged)
        );
        assert_eq!(
            desktop_event_kind(winvd::DesktopEvent::DesktopWallpaperChanged(
                winvd::get_desktop(0),
                String::new(),
            )),
            None
        );
    }
}
