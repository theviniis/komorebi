//! The only module in komorebi allowed to talk to the Windows virtual desktop API.
//!
//! Everything else goes through [`DesktopBackend`], which is [`WinvdBackend`] in
//! production and a fake backend in tests.

use color_eyre::eyre;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;
use windows_058::Win32::Foundation::HWND as WinvdHwnd;

pub trait DesktopBackend: Send + Sync {
    fn desktop_count(&self) -> eyre::Result<usize>;
    fn current_desktop_idx(&self) -> eyre::Result<usize>;
    fn switch_desktop(&self, idx: usize) -> eyre::Result<()>;
    fn desktop_idx_for_window(&self, hwnd: isize) -> eyre::Result<usize>;
    fn move_window_to_desktop(&self, hwnd: isize, idx: usize) -> eyre::Result<()>;
    fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>>;
    fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()>;
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
    Moved {
        desktop: usize,
        old: usize,
        new: usize,
    },
    Renamed {
        desktop: usize,
    },
}

static EVENT_THREAD: OnceLock<winvd::DesktopEventThread> = OnceLock::new();

/// Starts listening for native desktop events, translating them to
/// [`DesktopEventKind`] and invoking `cb` on a dedicated thread.
///
/// This is the only place in komorebi that touches
/// `winvd::listen_desktop_events`.
pub fn spawn_event_thread(cb: impl Fn(DesktopEventKind) + Send + 'static) -> eyre::Result<()> {
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
            new: event_desktop_idx(&new)?,
            old: event_desktop_idx(&old)?,
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
            desktop: event_desktop_idx(&desktop)?,
        }),
        winvd::DesktopEvent::DesktopWallpaperChanged(..)
        | winvd::DesktopEvent::WindowChanged(_) => None,
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
        switch_calls: usize,
        window_desktops: HashMap<isize, usize>,
        moved: Vec<(isize, usize)>,
        names: HashMap<usize, String>,
    }

    /// In-memory [`DesktopBackend`] for tests.
    #[derive(Debug)]
    pub struct FakeDesktopBackend {
        count: usize,
        state: Mutex<FakeState>,
    }

    impl FakeDesktopBackend {
        #[must_use]
        pub fn new(count: usize) -> Self {
            Self {
                count,
                state: Mutex::new(FakeState::default()),
            }
        }

        pub fn set_window_desktop(&self, hwnd: isize, idx: usize) {
            self.state.lock().window_desktops.insert(hwnd, idx);
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
            if idx >= self.count {
                eyre::bail!("desktop {idx} does not exist (count: {})", self.count);
            }

            Ok(())
        }
    }

    impl DesktopBackend for FakeDesktopBackend {
        fn desktop_count(&self) -> eyre::Result<usize> {
            Ok(self.count)
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
            state.window_desktops.insert(hwnd, idx);
            state.moved.push((hwnd, idx));

            Ok(())
        }

        fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>> {
            Ok(self.state.lock().names.get(&idx).cloned())
        }

        fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()> {
            self.ensure_desktop_exists(idx)?;
            self.state.lock().names.insert(idx, name.to_string());

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::collections::HashMap;

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
    }

    #[test]
    fn installed_backend_is_returned_by_backend() {
        let fake: Arc<dyn DesktopBackend> = Arc::new(FakeDesktopBackend::new(1));
        set_backend(Some(fake.clone()));
        assert!(Arc::ptr_eq(&backend(), &fake));
        set_backend(None);
    }

    #[test]
    fn desktop_events_are_translated_to_kinds() {
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
