//! Adopting winit's `wl_display` connection. The only module in the workspace besides
//! `photocraft-tablet`'s `macos.rs` that allows `unsafe`: one foreign-pointer call that
//! wayland-client can't prove sound on its own.

#![allow(unsafe_code)]

use raw_window_handle::{DisplayHandle, RawDisplayHandle};
use wayland_backend::client::Backend;
use wayland_client::Connection;

/// A connection over winit's `wl_display` (`None` when the display isn't Wayland). Objects
/// created through it belong to the app's client, so the compositor sends them the drag events
/// for the app's window.
pub(crate) fn connection(display: DisplayHandle<'_>) -> Option<Connection> {
    let RawDisplayHandle::Wayland(h) = display.as_raw() else { return None };
    // SAFETY: `h.display` is winit's live `wl_display*` (raw-window-handle hands out only valid
    // handles). The backend must not outlive it. winit keeps the display for the event loop's
    // lifetime, and this backend is used only by the `Listener`'s thread, which `Listener::drop`
    // stops and joins; the app owns the listener and is dropped before eframe drops the event
    // loop. libwayland itself is thread-safe, and the backend reads on its own event queue.
    let backend = unsafe { Backend::from_foreign_display(h.display.as_ptr().cast()) };
    Some(Connection::from_backend(backend))
}
