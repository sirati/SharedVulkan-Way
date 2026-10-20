//! Producer death certificates from retained, authenticated kernel objects.
//!
//! This module never reads a PID or a client-writable ownership word. The
//! broker captures its own allocation record in the callback. That callback
//! may retire only the producer hold; pending transfers and consumer leases
//! need their independent completion proof. An unregistered allocation stays
//! retained. Broker restart also retains allocations until ownership can be
//! established again.
use super::ident::Owned;
use std::io;
use std::sync::Arc;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

/// Evidence that the exact retained process object became signaled.
///
/// Only the privileged broker creates this value. It is not serializable and
/// no client request, timer, process identifier or shared word can create it.
/// A callback must still compare its trusted allocation generation before
/// changing a directory that could have been reused.
pub struct ProducerDeath {
    _process: Arc<Owned>,
}

pub(super) fn watch(
    process: Arc<Owned>,
    retire: Box<dyn FnOnce(ProducerDeath) + Send>,
) -> io::Result<()> {
    let retained = Arc::clone(&process);
    if !super::watch_process(
        process,
        Box::new(move || {
            // SAFETY: this is the exact process HANDLE retained at authentication.
            // Zero timeout only queries its kernel state, it is not a death timeout.
            if unsafe { WaitForSingleObject(retained.0, 0) } == WAIT_OBJECT_0 {
                retire(ProducerDeath { _process: retained });
            }
            // A failed wait retains the allocation. It never means completion.
        }),
    ) {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

    #[test]
    fn producer_wait_child() {
        if std::env::var_os("WC_BROKER_RETIREMENT_TEST_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn producer_retirement_waits_for_actual_process_exit() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sys::windows::retirement::tests::producer_wait_child",
            ])
            .env("WC_BROKER_RETIREMENT_TEST_CHILD", "1")
            .spawn()
            .unwrap();
        // SAFETY: the child is alive, and only synchronization access is used.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, child.id()) };
        assert!(!handle.is_null());
        let (send, recv) = mpsc::channel();
        watch(
            Arc::new(Owned(handle)),
            Box::new(move |proof| {
                drop(proof);
                send.send(()).unwrap();
            }),
        )
        .unwrap();
        assert!(recv.recv_timeout(Duration::from_millis(100)).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        recv.recv_timeout(Duration::from_secs(5)).unwrap();
        super::super::reap();
    }
}
