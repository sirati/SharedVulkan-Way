use std::ffi::CString;
#[cfg(unix)]
use crate::utils::os::AsFd;

use sha2::{Digest, Sha256};
use tracing::error;
use xkbcommon::xkb::{self, Keymap, KEYMAP_FORMAT_TEXT_V1};

use crate::utils::SealedFile;

/// Unique ID for a keymap
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeymapFileId([u8; 32]);

impl KeymapFileId {
    fn for_keymap(keymap: &str) -> Self {
        // Use a hash, so `keymap` events aren't sent when keymap hasn't changed, particularly
        // with `virtual-keyboard-unstable-v1`.
        Self(Sha256::digest(keymap).as_slice().try_into().unwrap())
    }
}

/// Wraps an XKB keymap into a sealed file or stores as just a string for sending to WlKeyboard over an fd
#[derive(Debug)]
pub struct KeymapFile {
    sealed: Option<SealedFile>,
    keymap: String,
    id: KeymapFileId,
}

impl KeymapFile {
    /// Turn the keymap into a string using KEYMAP_FORMAT_TEXT_V1, create a sealed file for it, and store the string
    pub fn new(keymap: &Keymap) -> Self {
        let name = c"smithay-keymap";
        let keymap = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1);
        let sealed = SealedFile::with_content(name, &CString::new(keymap.as_str()).unwrap());

        if let Err(err) = sealed.as_ref() {
            error!("Error when creating sealed keymap file: {}", err);
        }

        Self {
            id: KeymapFileId::for_keymap(&keymap),
            sealed: sealed.ok(),
            keymap,
        }
    }

    #[cfg(feature = "wayland_frontend")]
    pub(crate) fn change_keymap(&mut self, keymap: &Keymap) {
        let keymap = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        let name = c"smithay-keymap-file";
        let sealed = SealedFile::with_content(name, &CString::new(keymap.clone()).unwrap());

        if let Err(err) = sealed.as_ref() {
            error!("Error when creating sealed keymap file: {}", err);
        }

        self.id = KeymapFileId::for_keymap(&keymap);
        self.sealed = sealed.ok();
        self.keymap = keymap;
    }

    #[cfg(feature = "wayland_frontend")]
    /// Run a closure with the file descriptor to ensure safety
    pub fn with_fd<F>(
        &self,
        encoding: crate::utils::os::FdEncoding,
        supports_sealed: bool,
        cb: F,
    ) -> Result<(), std::io::Error>
    where
        F: FnOnce(crate::utils::os::BorrowedProtoFd<'_>, usize),
    {
        // The "unsealed" fallback below hands the client a plain `tempfile`
        // descriptor, so it exists only for the out-of-band dialect — an in-band
        // peer is handed a *name* and there is nothing to select between. A
        // client on version < 7 of `wl_keyboard` behind a proxy gets the sealed
        // object anyway; that is more permissive than the protocol requires
        // rather than less, since `keymap` says nothing about sealing.
        //
        // §80 turned this from a `cfg` into a test of the connection. It used to
        // be Windows-that-has-no-tempfile-path versus Unix-that-does; it is now
        // in-band-peer versus out-of-band-peer, and Windows is simply a platform
        // on which every peer is the former.
        let sealed = self.sealed.as_ref();
        let in_band = matches!(
            encoding,
            crate::utils::os::FdEncoding::InBand
                | crate::utils::os::FdEncoding::InBandArena(_)
                | crate::utils::os::FdEncoding::InBandLease(_)
        );
        let unsealed_ok = supports_sealed || in_band;
        if let Some(file) = unsealed_ok.then_some(sealed).flatten() {
            return file.with_proto_fd(encoding, |fd| cb(fd, file.size()));
        }
        if in_band {
            return Err(std::io::Error::other(
                "no keymap object to name for an in-band peer: creating it failed earlier",
            ));
        }

        #[cfg(unix)]
        {
            use std::{io::Write, path::PathBuf};
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);

            let mut file = tempfile::tempfile_in(dir)?;
            file.write_all(self.keymap.as_bytes())?;
            file.flush()?;

            cb(
                crate::utils::os::BorrowedProtoFd::Open(file.as_fd()),
                self.keymap.len(),
            );
            Ok(())
        }
        #[cfg(windows)]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "there is no out-of-band descriptor on this platform",
            ))
        }
    }

    /// Send the keymap contained within to a WlKeyboard
    pub fn send(
        &self,
        keyboard: &wayland_server::protocol::wl_keyboard::WlKeyboard,
    ) -> Result<(), std::io::Error> {
        use wayland_server::{protocol::wl_keyboard::KeymapFormat, Resource};

        self.with_fd(keyboard.fd_encoding(), keyboard.version() >= 7, |fd, size| {
            keyboard.keymap(KeymapFormat::XkbV1, fd, size as u32);
        })
    }

    /// Get this keymap's unique ID.
    pub(crate) fn id(&self) -> KeymapFileId {
        self.id
    }
}
