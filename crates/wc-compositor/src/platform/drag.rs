//! System drag and drop, from Win32's side.
//!
//! Windows' drag and drop is OLE: a source calls `DoDragDrop` with an
//! `IDataObject` and an `IDropSource`, a target registers an `IDropTarget` with
//! `RegisterDragDrop`, and the two negotiate a `DROPEFFECT` while the mouse
//! moves. None of that is a window message, which is why it is a module of its
//! own rather than three more arms in [`super::win32`]'s window procedure.
//!
//! # Status
//!
//! **Not implemented, and therefore complete rather than partial.** Every entry
//! point here answers definitively: [`run`] ends the drag with
//! [`dnd_action::NONE`] before returning, [`payload`] answers `None`, and
//! [`cancel`] has nothing to withdraw. No caller is left waiting, which is the
//! property that matters — a Wayland client whose `wl_data_source` never
//! receives `dnd_finished`, or whose `wl_data_offer.receive` pipe is never
//! closed, hangs, and a hung drag is worse than one that does not start.
//!
//! docs/waycompose-design.md §28.8 records what the two halves cost and, in particular,
//! the one thing that has to be measured before either is written: whether
//! `DoDragDrop`'s modal loop starves this process's message pump. The
//! precedent is §8.2 — the wakeup is a `PostMessage` to a window rather than a
//! `PostThreadMessage` precisely because `DefWindowProc`'s modal resize loop
//! discards thread messages — and the same question has to be asked of OLE's
//! loop before a compositor's repaints are put behind it.

use std::rc::Rc;

use super::dnd_action;
use super::win32::Ui;

/// Run a system drag carrying `payloads`, returning the action it ended with.
pub(crate) fn run(_ui: &Rc<Ui>, payloads: &[(String, Vec<u8>)], actions: u32) -> u32 {
    println!(
        "wc-compositor: a client started a drag with {} mime type(s) and actions {actions:#x}; \
         this compositor is not an OLE drag source yet, so the drag ends now with no drop. \
         The client is told so rather than being left waiting (docs/waycompose-design.md §28.8).",
        payloads.len()
    );
    dnd_action::NONE
}

/// The Wayland source withdrew.
pub(crate) fn cancel(_ui: &Rc<Ui>) {}

/// One mime type out of the drag currently over one of our windows.
pub(crate) fn payload(_ui: &Rc<Ui>, _mime: &str) -> Option<Vec<u8>> {
    // Nothing can be dragged onto these windows yet: no `IDropTarget` is
    // registered, so `wl_data_device.enter` is never sent and a client has no
    // offer to receive from. Answering `None` closes the pipe if one somehow
    // asks.
    None
}
