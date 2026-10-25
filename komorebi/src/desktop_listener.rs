//! Desktop event listener that bridges native Windows virtual desktop events
//! to komorebi's window manager event system.

use crate::virtual_desktop::{spawn_event_thread, DesktopEventKind};
use crate::window_manager_event::WindowManagerEvent;
use color_eyre::eyre;
use crossbeam_channel::Sender;

/// Spawn a thread that listens for native virtual desktop events and forwards
/// them as `WindowManagerEvent` to the given channel.
pub fn listen_for_desktop_events(tx: Sender<WindowManagerEvent>) -> eyre::Result<()> {
    spawn_event_thread(move |kind| {
        let event = match kind {
            DesktopEventKind::Changed { new, old } => {
                WindowManagerEvent::DesktopChanged { new, old }
            }
            DesktopEventKind::CountChanged => WindowManagerEvent::DesktopCountChanged,
            DesktopEventKind::Moved { desktop, old, new } => {
                WindowManagerEvent::DesktopMoved { desktop, old, new }
            }
            DesktopEventKind::Renamed { desktop } => WindowManagerEvent::DesktopRenamed { desktop },
        };

        // Send the event; ignore errors if the receiver is gone (shutdown)
        let _ = tx.send(event);
    })?;

    Ok(())
}
