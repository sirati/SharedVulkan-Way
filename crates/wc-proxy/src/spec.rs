//! The protocol's argument signatures, read from the XML rather than written
//! out by hand.
//!
//! # Why a table at all
//!
//! docs/design.md §2.3 records the one thing we escaped that waypipe could not:
//!
//! > "the type is on the wire. This is the cost waypipe could not avoid — it
//! > needed a full protocol parser because one cannot determine which passed
//! > file descriptor corresponds to which message without parsing the message."
//!
//! That escape is real, and it is real **on the Windows side of the boundary**,
//! where a descriptor is self-describing bytes in the message body. It does not
//! reach this side. The proxy's client is a stock libwayland client whose
//! `wl_shm.create_pool` carries an fd in a `SCM_RIGHTS` control message and
//! zero bytes of body, so to know that this particular message consumes one fd,
//! the proxy has to know that `wl_shm.create_pool`'s second argument is an `h`.
//! Which means the proxy pays exactly the cost §2.3 says the Windows dialect
//! avoids — **once, on the host side, where the client is** — and the Windows
//! compositor still does not.
//!
//! That asymmetry is the design working, not a hole in it. The parser is here
//! because this is the side that has fds.
//!
//! # Why the XML and not a `match`
//!
//! Twelve interfaces would fit in a hand-written table and the end goal does
//! not stop at twelve. `docs/goal-firefox.md` asks for Firefox, which binds
//! `wl_seat`, `wl_output`, `wl_data_device_manager`, `zwp_primary_selection`,
//! `xdg_decoration`, `wp_viewporter`, `wp_fractional_scale` and more, and every
//! one of those is a set of opcodes whose argument lists must be exactly right
//! or the byte stream desynchronises silently — the failure mode §8.9 calls
//! "precisely the class of bug that presents as §2.4's silent
//! desynchronisation".
//!
//! Reading the same XML the code generator reads means a protocol is added by
//! copying a file, and means the table cannot disagree with the compositor's
//! idea of the protocol, because both come from upstream's own description.
//!
//! # What is deliberately not modelled
//!
//! Enums, ranges, `since` versions and descriptions. The proxy needs to walk a
//! message body and know how long each argument is and which ones are fds,
//! object ids or new object ids. Anything else is the compositor's business,
//! and modelling it here would be a second implementation of the protocol to
//! keep in step with the first.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

/// The wire-level kinds. Wayland's `enum` and `fixed` are `int`-shaped on the
/// wire and are folded into it here, because nothing in this crate cares about
/// the difference between a fixed-point number and a signed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// 4 bytes: `int`, `uint`, `fixed`, `enum`.
    Word,
    /// 4-byte length (including the trailing NUL), the bytes, padded to 4.
    /// A length of 0 is a null string and occupies only the length word.
    Str,
    /// 4-byte length, the bytes, padded to 4. Never null.
    Array,
    /// 4 bytes: an existing object's id. 0 is a null object.
    Object,
    /// 4 bytes: an id the sender is allocating.
    NewId,
    /// `wl_registry.bind`'s untyped `new_id`, which is a *string*, a *uint*
    /// version and then the id — three arguments' worth of bytes for one
    /// argument in the XML. libwayland special-cases it and so must anything
    /// that walks the wire; it is the only place in the core protocol where the
    /// encoding of an argument depends on its `interface` attribute being
    /// absent.
    NewIdUntyped,
    /// A file descriptor. **Zero bytes of body on Unix**, which is the whole
    /// reason this table has to exist, and an `Array`-shaped descriptor on the
    /// Windows side of the boundary (docs/design.md §2.3).
    Fd,
}

/// One argument of one message.
#[derive(Clone, Debug)]
pub struct Arg {
    pub kind: ArgKind,
    /// For `NewId`, the interface the new object will have. `None` only for
    /// [`ArgKind::NewIdUntyped`], where it arrives on the wire instead.
    pub interface: Option<String>,
}

/// One request or event.
#[derive(Clone, Debug)]
pub struct Message {
    pub name: String,
    pub args: Vec<Arg>,
    /// `type="destructor"`. Recorded but not acted on: the id is retired when
    /// the server says so with `wl_display.delete_id`, which is the only moment
    /// both sides agree on.
    pub destructor: bool,
}

impl Message {
    /// Whether this message carries any `h` argument, which is the only
    /// question the hot path asks.
    pub fn has_fd(&self) -> bool {
        self.args.iter().any(|a| a.kind == ArgKind::Fd)
    }

    pub fn fd_count(&self) -> usize {
        self.args.iter().filter(|a| a.kind == ArgKind::Fd).count()
    }
}

/// One interface: its requests (client to server) and events (server to
/// client), in opcode order.
#[derive(Clone, Debug, Default)]
pub struct Interface {
    pub name: String,
    pub requests: Vec<Message>,
    pub events: Vec<Message>,
}

/// Every interface the proxy knows how to walk.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    interfaces: HashMap<String, Interface>,
}

/// What went wrong reading a protocol file.
#[derive(Debug)]
pub struct SpecError(String);

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol XML: {}", self.0)
    }
}

impl std::error::Error for SpecError {}

impl Spec {
    /// The protocols compiled into this binary.
    ///
    /// Vendored under `protocols/` rather than read from the filesystem at
    /// startup, for the reason `vendor/` exists at all: a proxy whose idea of
    /// the wire format depends on which `wayland-protocols` happens to be
    /// installed is a proxy that desynchronises on somebody else's machine. The
    /// files are byte copies — `wayland.xml` from `vendor/wayland-server`, so
    /// it is literally the same description the compositor was generated from.
    pub fn builtin() -> Result<Spec, SpecError> {
        let mut spec = Spec::default();
        spec.load(include_str!("../protocols/wayland.xml"))?;
        spec.load(include_str!("../protocols/xdg-shell.xml"))?;
        // ADDED FOR STAGE 9, and this is what §9.3 promised adding a protocol
        // would cost: copy a file, add a line.
        //
        // Both decoration protocols, because the compositor advertises both to
        // find out which one Firefox uses (docs/design.md §13.7) and a global
        // that is advertised can be bound. The failure without them is not
        // subtle and is not silent — the proxy refuses the connection with
        // `org_kde_kwin_server_decoration_manager has no message 0` — which is
        // §9.3's design working: an unknown interface cannot be walked, so it
        // says so instead of desynchronising.
        spec.load(include_str!("../protocols/xdg-decoration-unstable-v1.xml"))?;
        spec.load(include_str!("../protocols/server-decoration.xml"))?;
        Ok(spec)
    }

    /// Merge one protocol description into the table.
    pub fn load(&mut self, xml: &str) -> Result<(), SpecError> {
        let mut reader = Reader::from_str(xml);
        let cfg = reader.config_mut();
        cfg.trim_text(true);
        cfg.expand_empty_elements = true;

        // Where we are: an interface being filled, and within it a message.
        let mut iface: Option<Interface> = None;
        let mut msg: Option<(bool, Message)> = None; // (is_event, message)

        loop {
            match reader.read_event() {
                Err(e) => return Err(SpecError(format!("{e}"))),
                Ok(Event::Eof) => break,
                Ok(Event::Start(e)) => {
                    let tag = e.name();
                    match tag.as_ref() {
                        b"interface" => {
                            let name = attr(&e, b"name")
                                .ok_or_else(|| SpecError("interface with no name".into()))?;
                            iface = Some(Interface { name, ..Interface::default() });
                        }
                        b"request" | b"event" => {
                            let name = attr(&e, b"name").unwrap_or_default();
                            let destructor =
                                attr(&e, b"type").as_deref() == Some("destructor");
                            msg = Some((
                                tag.as_ref() == b"event",
                                Message { name, args: Vec::new(), destructor },
                            ));
                        }
                        b"arg" => {
                            if let Some((_, m)) = msg.as_mut() {
                                let ty = attr(&e, b"type").unwrap_or_default();
                                let interface = attr(&e, b"interface");
                                let kind = match ty.as_str() {
                                    "int" | "uint" | "fixed" | "enum" => ArgKind::Word,
                                    "string" => ArgKind::Str,
                                    "array" => ArgKind::Array,
                                    "object" => ArgKind::Object,
                                    "fd" => ArgKind::Fd,
                                    "new_id" => {
                                        if interface.is_some() {
                                            ArgKind::NewId
                                        } else {
                                            ArgKind::NewIdUntyped
                                        }
                                    }
                                    other => {
                                        return Err(SpecError(format!(
                                            "unknown argument type {other:?} in {}.{}",
                                            iface.as_ref().map(|i| i.name.as_str()).unwrap_or("?"),
                                            m.name
                                        )));
                                    }
                                };
                                m.args.push(Arg { kind, interface });
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"request" | b"event" => {
                        if let (Some((is_event, m)), Some(i)) = (msg.take(), iface.as_mut()) {
                            if is_event {
                                i.events.push(m);
                            } else {
                                i.requests.push(m);
                            }
                        }
                    }
                    b"interface" => {
                        if let Some(i) = iface.take() {
                            self.interfaces.insert(i.name.clone(), i);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    pub fn interface(&self, name: &str) -> Option<&Interface> {
        self.interfaces.get(name)
    }

    /// The request at `opcode` on `interface`, if both are known.
    pub fn request(&self, interface: &str, opcode: u16) -> Option<&Message> {
        self.interfaces.get(interface)?.requests.get(opcode as usize)
    }

    /// The event at `opcode` on `interface`, if both are known.
    pub fn event(&self, interface: &str, opcode: u16) -> Option<&Message> {
        self.interfaces.get(interface)?.events.get(opcode as usize)
    }

    pub fn len(&self) -> usize {
        self.interfaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty()
    }

    /// Every interface name, sorted. For the startup log, which is how an
    /// operator finds out that the protocol a client is about to bind is one
    /// this proxy cannot walk.
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.interfaces.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == key).and_then(|a| {
        String::from_utf8(a.value.into_owned()).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec::builtin().expect("the vendored protocol XML must parse")
    }

    #[test]
    fn the_builtin_protocols_parse() {
        let s = spec();
        // 23 in wayland.xml, 5 in xdg-shell.xml, 2 in each of the two
        // decoration protocols, counted with grep. A number rather than "not
        // empty", so that a parser that silently dropped a trailing interface
        // would be caught.
        assert_eq!(s.len(), 32, "interfaces: {:?}", s.names());
    }

    /// The message this whole crate exists for. `wl_shm.create_pool(new_id id,
    /// fd, int size)` — opcode 0, three arguments, the middle one an `h`.
    #[test]
    fn create_pool_is_the_shape_the_proxy_assumes() {
        let s = spec();
        let m = s.request("wl_shm", 0).expect("wl_shm has a request 0");
        assert_eq!(m.name, "create_pool");
        assert_eq!(m.fd_count(), 1);
        assert_eq!(
            m.args.iter().map(|a| a.kind).collect::<Vec<_>>(),
            [ArgKind::NewId, ArgKind::Fd, ArgKind::Word]
        );
        assert_eq!(m.args[0].interface.as_deref(), Some("wl_shm_pool"));
    }

    /// The one argument whose encoding depends on an attribute being *absent*.
    #[test]
    fn registry_bind_is_the_untyped_new_id() {
        let s = spec();
        let m = s.request("wl_registry", 0).unwrap();
        assert_eq!(m.name, "bind");
        assert_eq!(
            m.args.iter().map(|a| a.kind).collect::<Vec<_>>(),
            [ArgKind::Word, ArgKind::NewIdUntyped]
        );
    }

    /// The reverse direction's `h`: an event, not a request. If the proxy only
    /// looked at requests it would forward these bytes untouched and the client
    /// would read a keymap length as a message header.
    #[test]
    fn the_reverse_direction_has_fds_too() {
        let s = spec();
        let m = s.event("wl_keyboard", 0).unwrap();
        assert_eq!(m.name, "keymap");
        assert_eq!(m.fd_count(), 1);
        // And the whole set, so that adding a protocol with a new one is a
        // visible change to this list rather than a silent one.
        let mut with_fds = Vec::new();
        for name in s.names() {
            let i = s.interface(name).unwrap();
            for m in &i.requests {
                if m.has_fd() {
                    with_fds.push(format!("{name}.{} (request)", m.name));
                }
            }
            for m in &i.events {
                if m.has_fd() {
                    with_fds.push(format!("{name}.{} (event)", m.name));
                }
            }
        }
        with_fds.sort();
        assert_eq!(
            with_fds,
            [
                "wl_data_offer.receive (request)",
                "wl_data_source.send (event)",
                "wl_keyboard.keymap (event)",
                "wl_shm.create_pool (request)",
            ],
            "the set of messages carrying an h changed"
        );
    }

    #[test]
    fn opcodes_are_positional_and_line_up_with_the_hand_written_client() {
        // Cross-check against `wc-shmclient`'s hand-written constants, which
        // were verified against the same XML by eye in stage 3. Two
        // independent transcriptions agreeing is worth more than either.
        let s = spec();
        assert_eq!(s.request("wl_display", 0).unwrap().name, "sync");
        assert_eq!(s.request("wl_display", 1).unwrap().name, "get_registry");
        assert_eq!(s.event("wl_display", 0).unwrap().name, "error");
        assert_eq!(s.event("wl_display", 1).unwrap().name, "delete_id");
        assert_eq!(s.request("wl_compositor", 0).unwrap().name, "create_surface");
        assert_eq!(s.request("wl_shm_pool", 0).unwrap().name, "create_buffer");
        assert_eq!(s.event("wl_buffer", 0).unwrap().name, "release");
        assert_eq!(s.request("wl_surface", 1).unwrap().name, "attach");
        assert_eq!(s.request("wl_surface", 2).unwrap().name, "damage");
        assert_eq!(s.request("wl_surface", 3).unwrap().name, "frame");
        assert_eq!(s.request("wl_surface", 6).unwrap().name, "commit");
        assert_eq!(s.request("xdg_wm_base", 2).unwrap().name, "get_xdg_surface");
        assert_eq!(s.request("xdg_surface", 1).unwrap().name, "get_toplevel");
        assert_eq!(s.event("xdg_toplevel", 1).unwrap().name, "close");
    }

    #[test]
    fn an_unknown_interface_is_none_rather_than_a_panic() {
        let s = spec();
        assert!(s.request("zwp_nonexistent_v99", 0).is_none());
        assert!(s.request("wl_shm", 9999).is_none());
        assert!(s.event("wl_shm", 9999).is_none());
    }

    #[test]
    fn malformed_xml_is_an_error() {
        let mut s = Spec::default();
        assert!(s.load("<protocol><interface name=\"x\"><request name=\"r\"><arg type=\"nope\"/></request></interface></protocol>").is_err());
    }
}
