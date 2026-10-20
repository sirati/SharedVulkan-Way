//! Native Windows socket facade over the shared broker BAR connection.
//! Clients keep using ordinary socket reads/writes and Handle::create_section.
//! Only wire bytes pass through the pumps; pool pixels are allocated directly
//! from the transport's original BAR by the installed section provider.
use super::Stream;
use std::io::{self, Read, Write};
use std::net::Shutdown;

pub(super) fn connect() -> io::Result<Stream> {
    if wc_cygwin_link::wc_bridge_open() != 0 {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "broker BAR Wayland attachment refused"));
    }
    if let Err(error) = wc_handle::section_provider::install(wc_cygwin_link::native_pool_handle) {
        wc_cygwin_link::wc_bridge_close();
        return Err(error);
    }
    let (client, bridge) = Stream::pair().inspect_err(|_| wc_cygwin_link::wc_bridge_close())?;
    let mut incoming = bridge.try_clone().inspect_err(|_| wc_cygwin_link::wc_bridge_close())?;
    let mut outgoing = bridge.try_clone().inspect_err(|_| wc_cygwin_link::wc_bridge_close())?;
    std::thread::Builder::new().name("wl-bar-tx".into()).spawn(move || {
        let mut bytes = [0u8; 65536];
        'pump: loop {
            let count = match incoming.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut sent = 0;
            while sent < count {
                // SAFETY: current initialized prefix; bridge retains no pointer.
                let n = unsafe { wc_cygwin_link::wc_bridge_write(bytes[sent..count].as_ptr(), count - sent) };
                if n <= 0 { break 'pump; }
                sent += n as usize;
            }
        }
        wc_cygwin_link::wc_bridge_close();
        let _ = incoming.shutdown(Shutdown::Both);
    }).inspect_err(|_| wc_cygwin_link::wc_bridge_close())?;
    if let Err(error) = std::thread::Builder::new().name("wl-bar-rx".into()).spawn(move || {
        let mut bytes = [0u8; 65536];
        loop {
            // SAFETY: writable buffer; bridge retains no pointer.
            let n = unsafe { wc_cygwin_link::wc_bridge_read(bytes.as_mut_ptr(), bytes.len()) };
            if n <= 0 || outgoing.write_all(&bytes[..n as usize]).is_err() { break; }
        }
        wc_cygwin_link::wc_bridge_close();
        let _ = outgoing.shutdown(Shutdown::Both);
    }) {
        wc_cygwin_link::wc_bridge_close();
        let _ = bridge.shutdown(Shutdown::Both);
        return Err(error);
    }
    Ok(client)
}
