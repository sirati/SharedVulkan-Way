//! The microphone permission (TCC), through AVFoundation.
//!
//! Opening an input AudioQueue without the permission makes macOS show its
//! dialog, and while that dialog is up CoreAudio's queue server holds every
//! AudioQueue start in the process, the outputs' too (measured on the
//! Sonoma VM: the sound ring's thread stuck in `AudioQueueStartWithFlags`
//! for an output). So the input asks first, here, where asking involves no
//! queue: `+[AVCaptureDevice authorizationStatusForMediaType:]`, and if
//! undecided `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`,
//! which shows the same dialog. The input queue is opened only once granted.

use std::sync::mpsc::sync_channel;
use std::time::Duration;

use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};

use super::plan::MicAuth;

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeAudio: &'static AnyObject;
}

fn capture_device() -> Option<&'static AnyClass> {
    AnyClass::get(c"AVCaptureDevice")
}

/// The microphone permission of this process, now. Never shows anything.
pub fn authorization() -> MicAuth {
    let Some(cls) = capture_device() else { return MicAuth::Denied };
    // SAFETY: a class method taking an AVMediaType (an NSString) and
    // returning an AVAuthorizationStatus (NSInteger).
    let status: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: AVMediaTypeAudio] };
    MicAuth::from_status(status)
}

/// Ask for the microphone (macOS shows its dialog the first time) and wait
/// for the answer, at most `wait`. Call it on a thread of its own: it blocks
/// until the user answers. True if granted.
pub fn request(wait: Duration) -> bool {
    let Some(cls) = capture_device() else { return false };
    let (tx, rx) = sync_channel::<bool>(1);
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.try_send(granted.as_bool());
    });
    // SAFETY: a class method taking an AVMediaType and a `void (^)(BOOL)`
    // block, which it copies; the block is called once, on any queue.
    let () = unsafe { msg_send![cls, requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*block] };
    rx.recv_timeout(wait).unwrap_or(false)
}
