// Included only in the native Windows bridge module.
fn stream_capability(id: u64) -> Option<wc_handle::bar_stream::Stream> {
    let connection = CONNECTION.get()?;
    if !connection.valid() {
        return None;
    }
    connection
        .handles
        .lock()
        .ok()?
        .get(&id)?
        .open_bar_stream()
        .ok()
}
fn stream_result(result: std::io::Result<usize>) -> isize {
    match result {
        Ok(count) => count as isize,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => -2,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => -3,
        Err(_) => -1,
    }
}
/// Create both directions of a real original-BAR stream.
///
/// # Safety
/// Each output must name a writable u64 object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_bridge_stream_pair(reader: *mut u64, writer: *mut u64) -> i32 {
    if reader.is_null() || writer.is_null() {
        return -1;
    }
    let Some(connection) = CONNECTION.get() else {
        return -1;
    };
    if !connection.valid() {
        return -1;
    }
    let Some(arena) = connection.arena else {
        return -1;
    };
    let Ok((read, write)) = wc_handle::Handle::create_bar_pipe(arena) else {
        return -1;
    };
    let Ok(read_id) = hold_handle(connection, read) else {
        return -1;
    };
    let Ok(write_id) = hold_handle(connection, write) else {
        if let Ok(mut handles) = connection.handles.lock() {
            handles.remove(&read_id);
        }
        return -1;
    };
    // SAFETY: validated caller output contract.
    unsafe {
        reader.write(read_id);
        writer.write(write_id);
    }
    0
}
/// Import only a typed stream in this connection's original BAR.
///
/// # Safety
/// words names seven readable u32s; each output names a writable object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_bridge_stream_import(
    words: *const u32,
    writer: *mut i32,
    id: *mut u64,
) -> i32 {
    if words.is_null() || writer.is_null() || id.is_null() {
        return -1;
    }
    let Some(connection) = CONNECTION.get() else {
        return -1;
    };
    if !connection.valid() {
        return -1;
    }
    let mut bytes = [0; 24];
    // SAFETY: caller supplies seven readable words.
    unsafe {
        if words.read() != 24 {
            return -1;
        }
        for (index, chunk) in bytes.chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&words.add(index + 1).read().to_le_bytes());
        }
    }
    let Ok(descriptor) = wc_handle::Descriptor::decode(&bytes) else {
        return -1;
    };
    let wc_handle::Descriptor::BarStream {
        arena,
        writer: direction,
        ..
    } = descriptor
    else {
        return -1;
    };
    if Some(arena) != connection.arena {
        return -1;
    }
    let Ok(handle) = wc_handle::Handle::receive_descriptor(descriptor) else {
        return -1;
    };
    let Ok(local) = hold_handle(connection, handle) else {
        return -1;
    };
    // SAFETY: validated caller output contract.
    unsafe {
        writer.write(i32::from(direction));
        id.write(local);
    }
    0
}
/// Read bytes with native stream backpressure and EOF semantics.
///
/// # Safety
/// data must name len writable bytes, exclusively borrowed for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_bridge_stream_read(id: u64, data: *mut u8, len: usize) -> isize {
    if data.is_null() || len > isize::MAX as usize {
        return -1;
    }
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    // SAFETY: caller output-buffer contract.
    stream_result(stream.read(unsafe { std::slice::from_raw_parts_mut(data, len) }))
}
/// Write bytes with native stream backpressure and EPIPE semantics.
///
/// # Safety
/// data must name len readable bytes, immutable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_bridge_stream_write(id: u64, data: *const u8, len: usize) -> isize {
    if data.is_null() || len > isize::MAX as usize {
        return -1;
    }
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    // SAFETY: caller input-buffer contract.
    stream_result(stream.write(unsafe { std::slice::from_raw_parts(data, len) }))
}
/// Capture before checking the adapter's local stop flag.
///
/// # Safety
/// epoch must name a writable u32 object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_bridge_stream_epoch(id: u64, epoch: *mut u32) -> i32 {
    if epoch.is_null() {
        return -1;
    }
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    let Ok(value) = stream.wake_epoch() else {
        return -1;
    };
    // SAFETY: caller output contract.
    unsafe {
        epoch.write(value);
    }
    0
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_bridge_stream_wait(id: u64, epoch: u32, milliseconds: u32) -> i32 {
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    match stream.wait_since(epoch, Duration::from_millis(u64::from(milliseconds))) {
        Ok(ready) => i32::from(ready),
        Err(_) => -1,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_bridge_stream_kick(id: u64) -> i32 {
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    stream.interrupt();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_bridge_stream_watch(
    id: u64,
    epoch: u32,
    want_io: i32,
    milliseconds: u32,
) -> i32 {
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    match stream.watch_since(
        epoch,
        want_io != 0,
        Duration::from_millis(u64::from(milliseconds)),
    ) {
        Ok(flags) => flags as i32,
        Err(_) => -1,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn wc_bridge_stream_state(id: u64) -> i32 {
    let Some(stream) = stream_capability(id) else {
        return -1;
    };
    let ready = if stream.writer() {
        stream.writable()
    } else {
        stream.readable()
    };
    match ready {
        Ok(value) => i32::from(value) | (i32::from(stream.peer_closed()) << 1),
        Err(_) => -1,
    }
}
