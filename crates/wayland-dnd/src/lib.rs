//! File drag-and-drop on native Wayland, which the windowing layer (winit 0.30) doesn't deliver:
//! no hover, no drop, no position. On X11, Windows and macOS winit reports drops itself.
//!
//! [`Listener::start`] binds `wl_data_device` for every seat on **winit's own** `wl_display`
//! connection (drag events name the window's `wl_surface`, and the compositor sends them only to
//! the client that owns it), on a private event queue served by a background thread. A drag
//! that carries file URIs (`text/uri-list`) over the app's window is accepted as a copy; on drop
//! the URI list is read through a pipe and the local paths are handed to the UI with
//! [`Listener::poll`], together with whether files are hovering and where the pointer is.
//!
//! Sharing winit's connection means adopting a foreign `wl_display*`, so `foreign.rs` is the
//! only module that allows `unsafe` (craftrules never-crash: an isolated helper crate). The URI
//! list parsing ([`uri_list`]) and the bounded pipe read ([`collect`]) are pure and tested on
//! every platform.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod collect;
pub mod uri_list;

#[cfg(target_os = "linux")]
mod foreign;
#[cfg(target_os = "linux")]
mod listener;
#[cfg(target_os = "linux")]
pub use listener::Listener;

/// The drag state the UI reads each frame ([`Listener::poll`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Drops {
    /// Files are being dragged over the window.
    pub hovering: bool,
    /// The pointer during the last drag over the window (kept after its drop), in surface-local
    /// logical pixels: divide by the UI zoom factor for egui points.
    pub position: Option<(f64, f64)>,
    /// Files dropped since the last poll, as absolute local paths.
    pub dropped: Vec<std::path::PathBuf>,
}
