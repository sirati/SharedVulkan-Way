//! Which interface each object id has.
//!
//! The proxy needs this for one reason: an opcode means nothing on its own. To
//! know that message 0 on object 5 is `wl_shm.create_pool` and therefore
//! carries an `h`, the proxy must know that object 5 is a `wl_shm`. libwayland
//! knows because it created the proxy object; a compositor knows because it
//! created the resource; a byte forwarder in the middle knows only what it has
//! watched go past.
//!
//! # How an id gets a type, exhaustively
//!
//! 1. **Object 1 is `wl_display`.** The one hard-coded fact in the protocol.
//! 2. **A `new_id` argument with an `interface` attribute.** The XML says what
//!    the new object will be, so watching the message is enough.
//! 3. **`wl_registry.bind`'s untyped `new_id`**, where the interface name is a
//!    string in the message body. The only case where the wire carries the type
//!    rather than the table.
//!
//! There is no fourth. That is a stronger statement than it looks: it means a
//! proxy that watches every message in both directions has complete knowledge
//! of the object graph, and it is why this can be a byte forwarder rather than
//! a pair of protocol implementations.
//!
//! # How an id stops having one
//!
//! **`wl_display.delete_id`, and nothing else.** A client sending a destructor
//! request does not free the id — the server does, when it gets there, and it
//! says so. Retiring the id early would mean the proxy and the compositor
//! disagreed about the object graph for exactly as long as the round trip
//! takes, which is precisely when a well-behaved client is allowed to have
//! messages in flight for it.

use std::collections::HashMap;

/// Object ids at or above this belong to the server. Below it, the client. The
/// split matters here only for the log, but it is the reason two independent
/// allocators can share one id space without agreeing on anything.
pub const SERVER_ID_BASE: u32 = 0xff00_0000;

#[derive(Debug, Default)]
pub struct Objects {
    map: HashMap<u32, String>,
    /// Ids seen that the table had no interface for. Counted rather than
    /// listed: it is a number that should be zero, and a proxy that forwards a
    /// message on an object of unknown type is forwarding bytes it cannot
    /// check.
    unknown: u64,
}

impl Objects {
    /// A fresh connection: object 1 is `wl_display` and nothing else exists.
    pub fn new() -> Objects {
        let mut map = HashMap::new();
        map.insert(1u32, "wl_display".to_owned());
        Objects { map, unknown: 0 }
    }

    pub fn interface(&self, id: u32) -> Option<&str> {
        self.map.get(&id).map(String::as_str)
    }

    pub fn record(&mut self, id: u32, interface: &str) {
        self.map.insert(id, interface.to_owned());
    }

    pub fn forget(&mut self, id: u32) {
        self.map.remove(&id);
    }

    pub fn note_unknown(&mut self) {
        self.unknown += 1;
    }

    pub fn unknown(&self) -> u64 {
        self.unknown
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_table_knows_only_the_display() {
        let o = Objects::new();
        assert_eq!(o.interface(1), Some("wl_display"));
        assert_eq!(o.interface(2), None);
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn ids_can_be_recorded_forgotten_and_reused_with_another_type() {
        // The reuse case is real: `wl_callback` ids are one-shot and a client
        // that reuses the number for a `wl_buffer` is within its rights once
        // `delete_id` has arrived. A table that refused to overwrite would then
        // walk the new object's messages with the old object's signatures.
        let mut o = Objects::new();
        o.record(13, "wl_callback");
        assert_eq!(o.interface(13), Some("wl_callback"));
        o.forget(13);
        assert_eq!(o.interface(13), None);
        o.record(13, "wl_buffer");
        assert_eq!(o.interface(13), Some("wl_buffer"));
    }

    #[test]
    fn server_ids_are_above_the_base() {
        assert_eq!(SERVER_ID_BASE, 0xff000000);
    }
}
