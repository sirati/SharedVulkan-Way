//! Emission boundary for complete protocol messages carrying owned resources.
use std::io::{self, Write};

/// A failed frame and the number of bytes already accepted by its stream.
#[derive(Debug)]
pub struct FrameError {
    pub error: io::Error,
    pub emitted: usize,
}

/// Write one framed message while the caller retains its transfer guard.
/// On failure close synchronously before returning. If `emitted` is nonzero,
/// commit the reservation conservatively: a peer may already have consumed a
/// descriptor. Zero emitted bytes permit rollback. Never retry a failed frame
/// on another connection with the same reservation.
pub fn write_frame(writer: &mut impl Write, bytes: &[u8], abort: impl FnOnce()) -> Result<(), FrameError> {
    let mut emitted = 0;
    while emitted < bytes.len() {
        match writer.write(&bytes[emitted..]) {
            Ok(0) => {
                abort();
                return Err(FrameError { error: io::ErrorKind::WriteZero.into(), emitted });
            }
            Ok(count) => emitted += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                abort();
                return Err(FrameError { error, emitted });
            }
        }
    }
    Ok(())
}

/// Prepare one descriptor and preserve its lifetime across the complete frame.
/// A first-write failure rolls back. A partially emitted frame conservatively
/// retains the pending reference because the peer may already have accepted it.
pub fn send_handle_frame(writer: &mut impl Write, handle: &wc_handle::Handle,
    frame: impl FnOnce(&[u8]) -> Vec<u8>, abort: impl FnOnce()) -> io::Result<()> {
    let transfer = handle.prepare_transfer()?;
    let bytes = frame(transfer.bytes());
    match write_frame(writer, &bytes, abort) {
        Ok(()) => { transfer.commit(); Ok(()) }
        Err(failed) => {
            if failed.emitted > 0 { transfer.commit(); }
            Err(failed.error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct FailAfter { remaining: usize, written: Vec<u8> }
    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 { return Err(io::ErrorKind::BrokenPipe.into()); }
            let count = self.remaining.min(bytes.len());
            self.written.extend_from_slice(&bytes[..count]);
            self.remaining -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    #[test]
    fn failed_and_partial_frames_report_when_a_transfer_must_remain_reserved() {
        for accepted in [0, 1, 7] {
            let mut writer = FailAfter { remaining: accepted, written: Vec::new() };
            let aborted = Cell::new(false);
            let error = write_frame(&mut writer, b"full framed message", || aborted.set(true)).unwrap_err();
            assert_eq!(error.emitted, accepted);
            assert!(aborted.get());
            assert_eq!(writer.written.len(), accepted);
        }
    }
    #[test]
    fn successful_frame_never_aborts() {
        let mut writer = FailAfter { remaining: usize::MAX, written: Vec::new() };
        write_frame(&mut writer, b"frame", || panic!("successful transfer aborted")).unwrap();
        assert_eq!(writer.written, b"frame");
    }
    #[test]
    fn interrupted_write_is_retried_and_zero_write_aborts() {
        struct InterruptedOnce { interrupted: bool, bytes: Vec<u8> }
        impl Write for InterruptedOnce {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let mut writer = InterruptedOnce { interrupted: false, bytes: Vec::new() };
        write_frame(&mut writer, b"retry", || panic!("interruption aborted transfer")).unwrap();
        assert_eq!(writer.bytes, b"retry");
        let mut empty = &mut [0u8; 0][..];
        let aborted = Cell::new(false);
        let error = write_frame(&mut empty, b"frame", || aborted.set(true)).unwrap_err();
        assert_eq!(error.error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(error.emitted, 0);
        assert!(aborted.get());
    }

    #[test]
    fn actual_bar_transfers_roll_back_only_before_emission() {
        use std::sync::Arc;
        use wc_handle::{Descriptor, Handle, bar_lease::{self, Directory, Role, DIRECTORY_BYTES}};
        let backing = Arc::new(sv_transport::region::SharedRegion::heap(1 << 20).unwrap());
        let base = unsafe { backing.base_ptr() };
        // SAFETY: retained exclusive heap mapping, aligned metadata and disjoint
        // guest writable range. This fixture has no external peer or GPU.
        let directory = unsafe { Directory::initialize(base, base, base.add(DIRECTORY_BYTES),
            Role::Guest, 1 << 20, 1 << 19, 9913, backing.clone()) }.unwrap();
        bar_lease::register(9913, directory.clone()).unwrap();
        for emitted in [0, 7] {
            let handle = Handle::create_bar(9913, Role::Guest, 4096, 65536).unwrap();
            let descriptor = handle.descriptor().clone();
            let Descriptor::BarLease { slot, .. } = descriptor else { panic!("not a BAR lease") };
            let mut writer = FailAfter { remaining: emitted, written: Vec::new() };
            let aborted = Cell::new(false);
            assert!(send_handle_frame(&mut writer, &handle, |bytes| bytes.to_vec(), || aborted.set(true)).is_err());
            assert!(aborted.get());
            drop(handle);
            let other = directory.allocate(Role::Guest, 4096, 65536).unwrap();
            if emitted == 0 {
                assert_eq!(other.key().slot, slot, "unsent transfer did not roll back");
                assert!(Handle::receive_descriptor(descriptor).is_err(), "unsent descriptor retained a transfer");
            } else {
                assert_ne!(other.key().slot, slot, "partially emitted transfer freed its extent");
                let consumer = Handle::receive_descriptor(descriptor).unwrap();
                let mapping = consumer.map().unwrap();
                drop(consumer);
                assert_eq!(mapping.as_slice().len(), 4096);
                drop(other);
                let still_held = directory.allocate(Role::Guest, 4096, 65536).unwrap();
                assert_ne!(still_held.key().slot, slot, "mapping lost its consumer lease");
                drop(still_held);
                drop(mapping);
                assert_eq!(directory.allocate(Role::Guest, 4096, 65536).unwrap().key().slot, slot,
                    "legitimate consumer release did not permit reuse");
                continue;
            }
            drop(other);
        }
    }

}
