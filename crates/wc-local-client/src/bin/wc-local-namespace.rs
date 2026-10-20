#[cfg(windows)]
fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(base) = args.next() else {
        eprintln!("usage: wc-local-namespace BASE_DIRECTORY");
        std::process::exit(2)
    };
    if args.next().is_some() {
        std::process::exit(2);
    }
    match wc_local_client::namespace::prepare(std::path::Path::new(&base)) {
        Ok(path) => println!("{}", path.display()),
        Err(error) => {
            eprintln!("wc-local-namespace: {error}");
            std::process::exit(2);
        }
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows local namespace only");
    std::process::exit(2);
}
