//! Portable retained-original-FD fixture. It needs no GPU, compositor or VM.
//! Passing this on Darwin establishes native socket/identity behavior only.
#[cfg(unix)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::FileExt;
    use std::os::unix::net::UnixStream;
    use sv_drm::native_prime::{Registry, register, serve_connection};
    use sv_drm::token_wire::Token;

    let token = Token {
        size: 16384,
        nonce: [23; 16],
        name: "/sv-shared-native-prime-fixture".into(),
        record: String::new(),
    };
    let header = token.encode();
    let path = std::env::temp_dir().join(format!("sv-native-prime-fixture-{}", std::process::id()));
    let original = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    original.set_len(token.size)?;
    original.write_all_at(&header, 0)?;
    let alias = original.try_clone()?;
    let registry = Registry::default();
    assert!(registry.import(alias.as_fd()).is_err());
    let (mut client, server) = UnixStream::pair()?;
    let peer = registry.clone();
    let worker =
        std::thread::spawn(move || serve_connection(server, unsafe { libc::geteuid() }, &peer));
    register(&mut client, original.as_fd(), &header)?;
    let consumer = registry.import(alias.as_fd())?;
    original.write_all_at(b"X", 0)?;
    assert!(registry.import(alias.as_fd()).is_err());
    original.write_all_at(&header, 0)?;
    consumer.validate()?;
    drop(client);
    worker
        .join()
        .map_err(|_| "registration worker panicked")??;
    assert!(registry.import(alias.as_fd()).is_err());
    drop(alias);
    drop(original);
    assert_eq!(consumer.token(), &token);
    assert!(unsafe { libc::fcntl(consumer.fd().as_raw_fd(), libc::F_GETFD) } >= 0);
    consumer.validate()?;
    println!("native PRIME retained-FD fixture passed");
    Ok(())
}
fn main() {
    #[cfg(unix)]
    if let Err(error) = run() {
        eprintln!("native PRIME fixture: {error}");
        std::process::exit(1);
    }
}
