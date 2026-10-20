//! Offline/native peer for the ARM64 hl mapping acceptance test.
//! Only one-byte notifications use the socket. Test data stays in the backing.
#[cfg(unix)]
mod unix {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::{fs::OpenOptionsExt, net::UnixListener};
    use std::sync::atomic::{AtomicU32, Ordering};
    use wc_hyperlinux::Backing;

    pub fn run() -> std::io::Result<()> {
        let args: Vec<_> = std::env::args_os().collect();
        if args.len() != 3 {
            eprintln!("usage: wc-hl-shared-peer BACKING SOCKET");
            std::process::exit(2);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&args[1])?;
        file.set_len(65536)?;
        let _original_mapping = Backing::map(file.as_fd(), 0, 65536, true)?;
        let listener = UnixListener::bind(&args[2])?;
        println!("ready");
        let result = (|| {
            let (mut socket, _) = listener.accept()?;
            wc_hyperlinux::local_fd::send(&socket, file.as_fd())?;
            let guest_fd = wc_hyperlinux::local_fd::receive(&socket)?;
            let received = Backing::map(guest_fd.as_fd(), 0, 65536, true)?;
            // Exercise the mapping imported from the guest's actual SCM_RIGHTS fd.
            let mapping = &received;
            // Kernel-local connection. The socket path's directory is private to
            // the test user; no TCP route or guest application settings are used.
            let mut bell = [0];
            socket.read_exact(&mut bell)?;
            // SAFETY: shared file offset 4096 is aligned and mapped in this process.
            let word =
                unsafe { &*((mapping.info().base as *mut u8).add(4096) as *const AtomicU32) };
            if word.load(Ordering::Acquire) != 0x57434c31 {
                return Err(std::io::Error::other(
                    "guest publication missing from shared backing",
                ));
            }
            word.store(0x57434c32, Ordering::Release);
            socket.write_all(&[1])?;
            socket.read_exact(&mut bell)?;
            if word.load(Ordering::Acquire) != 0x57434c33 {
                return Err(std::io::Error::other("fork child publication missing"));
            }
            word.store(0x57434c34, Ordering::Release);
            socket.write_all(&[1])?;
            socket.read_exact(&mut bell)?;
            if word.load(Ordering::Acquire) != 0x57434c35 {
                return Err(std::io::Error::other("exec client publication missing"));
            }
            word.store(0x57434c36, Ordering::Release);
            socket.write_all(&[1])?;
            Ok(())
        })();
        std::fs::remove_file(&args[2])?;
        std::fs::remove_file(&args[1])?;
        result
    }
}
#[cfg(unix)]
fn main() -> std::io::Result<()> {
    unix::run()
}
#[cfg(not(unix))]
fn main() {
    eprintln!("Hyper Linux requires a Unix host");
    std::process::exit(2);
}
