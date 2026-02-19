use crate::TermWindow;
use crate::termwindow::TermWindowNotif;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource};
use mux::Mux;
use mux::pane::Pane;
use std::sync::Arc;
use window::{Clipboard, WindowOps};

impl TermWindow {
    pub fn copy_to_clipboard(&self, clipboard: ClipboardCopyDestination, text: String) {
        let clipboard = match clipboard {
            ClipboardCopyDestination::Clipboard => [Some(Clipboard::Clipboard), None],
            ClipboardCopyDestination::PrimarySelection => [Some(Clipboard::PrimarySelection), None],
            ClipboardCopyDestination::ClipboardAndPrimarySelection => [
                Some(Clipboard::Clipboard),
                Some(Clipboard::PrimarySelection),
            ],
        };
        for &c in &clipboard {
            if let Some(c) = c {
                self.window.as_ref().unwrap().set_clipboard(c, text.clone());
            }
        }
    }

    pub fn paste_from_clipboard(&mut self, pane: &Arc<dyn Pane>, clipboard: ClipboardPasteSource) {
        let pane_id = pane.pane_id();
        log::trace!(
            "paste_from_clipboard in pane {} {:?}",
            pane.pane_id(),
            clipboard
        );
        let window = self.window.as_ref().unwrap().clone();
        let clipboard = match clipboard {
            ClipboardPasteSource::Clipboard => Clipboard::Clipboard,
            ClipboardPasteSource::PrimarySelection => Clipboard::PrimarySelection,
        };
        let future = window.get_clipboard(clipboard);
        promise::spawn::spawn(async move {
            if let Ok(clip) = future.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                    // Get the pane, resolving overlay if present
                    let pane = {
                        let state = myself.pane_state(pane_id);
                        state
                            .overlay
                            .as_ref()
                            .map(|overlay| overlay.pane.clone())
                    }
                    .or_else(|| {
                        let mux = Mux::get();
                        mux.get_pane(pane_id)
                    });

                    if let Some(pane) = pane {
                        // F03: Handle WouldBlock for paste operations instead of silently dropping
                        if let Err(e) = pane.send_paste(&clip) {
                            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                                if io_err.kind() == std::io::ErrorKind::WouldBlock {
                                    myself.queue_input_op(
                                        pane_id,
                                        crate::termwindow::InputOp::Paste(clip.clone()),
                                    );
                                    log::warn!(
                                        "Paste to pane {} got WouldBlock, queued for retry ({} bytes)",
                                        pane_id,
                                        clip.len()
                                    );
                                } else {
                                    log::error!("Paste to pane {} failed: {:?}", pane_id, e);
                                }
                            } else {
                                log::error!("Paste to pane {} failed: {:?}", pane_id, e);
                            }
                        }
                    }
                })));
            }
        })
        .detach();
        self.maybe_scroll_to_bottom_for_input(&pane);
    }
}
