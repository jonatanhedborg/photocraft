//! File drag-and-drop on native Wayland, where winit 0.30 reports none: `photocraft-wayland-dnd`
//! listens on winit's connection, and each frame its state becomes egui's `hovered_files` /
//! `dropped_files`, so drops go through the same routing as on X11, Windows and macOS
//! (`file_open::DropTarget`: canvas, tab strip or elsewhere). The listener also knows where the
//! drag is, which is the drop position on Wayland.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use photocraft_ui_egui::{CursorPosFn, FileDragsFn};
use photocraft_wayland_dnd::Listener;

/// A file dropped on the window (an absolute path; the UI reads it from there).
#[derive(Debug)]
struct DroppedPath(PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// The file-drag and drop-position services for a native Wayland window (`None` elsewhere, or
/// when the compositor offers no drag-and-drop: then drops do nothing, as before).
pub fn services(cc: &eframe::CreationContext<'_>) -> Option<(FileDragsFn, CursorPosFn)> {
    let ctx = cc.egui_ctx.clone();
    let listener = match Listener::start(cc, move || ctx.request_repaint()) {
        Ok(l) => Rc::new(l?),
        Err(e) => {
            log::warn!("file drag-and-drop is off: {e}");
            return None;
        }
    };
    let drags = listener.clone();
    let drags: FileDragsFn = Box::new(move |raw: &mut egui::RawInput| apply(drags.poll(), raw));
    // Surface-local logical pixels are egui points times the UI zoom.
    let pos: CursorPosFn = Box::new(move |ctx: &egui::Context| listener.position().map(|(x, y)| egui::pos2(x as f32, y as f32) / ctx.zoom_factor()));
    Some((drags, pos))
}

/// Add the listener's state to a frame's input, the way egui-winit reports drags elsewhere.
fn apply(drops: photocraft_wayland_dnd::Drops, raw: &mut egui::RawInput) {
    if drops.hovering && raw.hovered_files.is_empty() {
        raw.hovered_files.push(egui::HoveredFile::default());
    }
    if !drops.dropped.is_empty() {
        raw.hovered_files.clear();
        raw.dropped_files.extend(drops.dropped.into_iter().map(|p| -> egui::DroppedFileHandle { std::sync::Arc::new(DroppedPath(p)) }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_wayland_dnd::Drops;

    #[test]
    fn hovering_and_drops_become_egui_input() {
        let mut raw = egui::RawInput::default();
        apply(Drops::default(), &mut raw);
        assert!(raw.hovered_files.is_empty() && raw.dropped_files.is_empty());

        apply(Drops { hovering: true, position: Some((10.0, 20.0)), dropped: Vec::new() }, &mut raw);
        assert_eq!(raw.hovered_files.len(), 1);
        // Still hovering next frame: one entry, not one per frame.
        apply(Drops { hovering: true, position: Some((11.0, 20.0)), dropped: Vec::new() }, &mut raw);
        assert_eq!(raw.hovered_files.len(), 1);

        let files = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.psd")];
        apply(Drops { hovering: false, position: Some((11.0, 20.0)), dropped: files.clone() }, &mut raw);
        assert!(raw.hovered_files.is_empty(), "a drop ends the hover");
        assert_eq!(raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>(), files);
    }

    #[test]
    fn a_dropped_path_reads_its_file_or_reports_why_not() {
        let dir = std::env::temp_dir().join(format!("photocraft-wl-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.png");
        std::fs::write(&file, b"png").unwrap();
        assert_eq!(egui::DroppedFile::bytes(&DroppedPath(file.clone())).unwrap(), b"png");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(egui::DroppedFile::bytes(&DroppedPath(file)).is_err());
    }
}
