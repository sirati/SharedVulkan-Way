//! Offline ownership tests against the real Mach adapter with native VM stubs.
//! These prove Rust lifetime/cleanup rules, not macOS kernel export support.
#![cfg(target_os = "linux")]
#[path = "../src/mach.rs"]
#[allow(dead_code)]
mod mach;
use std::{
    alloc::Layout,
    sync::{Arc, Mutex},
};
use sv_transport::region::SharedRegion;

#[derive(Debug, PartialEq)]
enum Event {
    Map {
        copy: i32,
        inherit: i32,
        protection: i32,
    },
    Unmap,
    Right(u32),
}
struct State {
    events: Vec<Event>,
    export_code: i32,
    export_port: u32,
    grow: bool,
}
static STATE: Mutex<State> = Mutex::new(State {
    events: Vec::new(),
    export_code: 0,
    export_port: 17,
    grow: false,
});
#[unsafe(export_name = "mach_task_self_")]
static TASK: u32 = 1;
#[unsafe(export_name = "mach_port_deallocate")]
extern "C" fn deallocate(_task: u32, port: u32) -> i32 {
    STATE.lock().unwrap().events.push(Event::Right(port));
    0
}
#[unsafe(export_name = "mach_make_memory_entry_64")]
extern "C" fn export(
    _task: u32,
    size: &mut u64,
    _address: u64,
    _permission: i32,
    entry: &mut u32,
    _parent: u32,
) -> i32 {
    let state = STATE.lock().unwrap();
    *entry = state.export_port;
    if state.grow {
        *size += 4096;
    }
    state.export_code
}
#[unsafe(export_name = "mach_vm_map")]
extern "C" fn map(
    _task: u32,
    address: &mut u64,
    len: u64,
    _mask: u64,
    _flags: i32,
    _entry: u32,
    _offset: u64,
    copy: i32,
    protection: i32,
    _maximum: i32,
    inherit: i32,
) -> i32 {
    let layout = Layout::from_size_align(len as usize, 4096).unwrap();
    let pointer = unsafe { std::alloc::alloc_zeroed(layout) };
    if pointer.is_null() {
        return 3;
    }
    *address = pointer as u64;
    STATE.lock().unwrap().events.push(Event::Map {
        copy,
        inherit,
        protection,
    });
    0
}
#[unsafe(export_name = "mach_vm_deallocate")]
extern "C" fn unmap(_task: u32, address: u64, len: u64) -> i32 {
    unsafe {
        std::alloc::dealloc(
            address as *mut u8,
            Layout::from_size_align(len as usize, 4096).unwrap(),
        );
    }
    STATE.lock().unwrap().events.push(Event::Unmap);
    0
}

#[test]
fn mapped_slices_retain_the_vm_view_and_right_and_export_errors_drop_rights() {
    // Real native backing supplied by the stub lets the retained slice remain
    // readable after its parent/owner variables have been dropped.
    let owner =
        Arc::new(mach::Mapping::new(unsafe { mach::SendRight::from_raw(42) }, 8192).unwrap());
    let parent = Arc::new(unsafe {
        SharedRegion::from_raw_owned(owner.as_ptr(), owner.len(), owner.clone())
    });
    let child = parent.slice(4096, 4096).unwrap();
    drop(parent);
    drop(owner);
    child.write_bytes(0, b"still mapped").unwrap();
    assert!(child.equals(0, b"still mapped").unwrap());
    assert_eq!(
        STATE.lock().unwrap().events,
        vec![Event::Map {
            copy: 0,
            inherit: 2,
            protection: 3
        }]
    );
    drop(child);
    assert_eq!(
        STATE.lock().unwrap().events,
        vec![
            Event::Map {
                copy: 0,
                inherit: 2,
                protection: 3
            },
            Event::Unmap,
            Event::Right(42)
        ]
    );

    let backing = SharedRegion::heap(4096).unwrap();
    let address = unsafe { backing.base_ptr() } as u64;
    {
        let mut state = STATE.lock().unwrap();
        state.events.clear();
        state.export_code = 5;
        state.export_port = 123;
    }
    assert!(unsafe { mach::memory_entry(address, 4096) }.is_err());
    assert_eq!(STATE.lock().unwrap().events, vec![Event::Right(123)]);
    {
        let mut state = STATE.lock().unwrap();
        state.events.clear();
        state.export_code = 0;
        state.export_port = 124;
        state.grow = true;
    }
    assert!(unsafe { mach::memory_entry(address, 4096) }.is_err());
    assert_eq!(STATE.lock().unwrap().events, vec![Event::Right(124)]);
    {
        let mut state = STATE.lock().unwrap();
        state.events.clear();
        state.export_port = 0;
        state.grow = false;
    }
    assert!(unsafe { mach::memory_entry(address, 4096) }.is_err());
    assert!(STATE.lock().unwrap().events.is_empty());
    let raw = unsafe { mach::SendRight::from_raw(55) }.into_raw();
    assert_eq!(raw, 55);
    assert!(STATE.lock().unwrap().events.is_empty());
    drop(unsafe { mach::SendRight::from_raw(raw) });
    assert_eq!(STATE.lock().unwrap().events, vec![Event::Right(55)]);
}
