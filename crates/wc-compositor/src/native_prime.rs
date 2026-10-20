//! Hyper Linux's native linux-dmabuf adapter. Ordinary wl_shm keeps its normal
//! SCM_RIGHTS path. Tokens select broker-owned IOSurfaces; frame pages are never
//! copied by this protocol adapter.
use crate::state::State;
use crate::surface::{BufferSpec, PixelError, PixelFormat, PixelSource};
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use smithay::reexports::wayland_server::protocol::wl_buffer::{self, WlBuffer};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use sv_drm::native_prime::{Imported, Registry, SyncPage};

const ARGB: u32 = u32::from_le_bytes(*b"AR24");
const XRGB: u32 = u32::from_le_bytes(*b"XR24");
// Linux dev_t for the runtime's synthetic /dev/dri/renderD128 (226:128).
const MAIN_DEVICE: u64 = 0xe280;

pub struct Protocol {
    pub registry: Registry,
    table: smithay::utils::SealedFile,
}
impl Protocol {
    pub fn new(dh: &DisplayHandle) -> io::Result<Self> {
        let mut table = Vec::new();
        for format in [ARGB, XRGB] {
            table.extend_from_slice(&format.to_ne_bytes());
            table.extend_from_slice(&0u32.to_ne_bytes());
            table.extend_from_slice(&0u64.to_ne_bytes());
        }
        let table = smithay::utils::SealedFile::with_data(c"wc-native-prime-formats", &table)?;
        dh.create_global::<State, ZwpLinuxDmabufV1, _>(4, ());
        Ok(Self {
            registry: Registry::default(),
            table,
        })
    }
    fn feedback(&self, feedback: &ZwpLinuxDmabufFeedbackV1) {
        feedback.format_table(
            wayland_backend::os::BorrowedWireFd::Open(self.table.as_fd()),
            32,
        );
        feedback.main_device(MAIN_DEVICE.to_ne_bytes().to_vec());
        feedback.tranche_target_device(MAIN_DEVICE.to_ne_bytes().to_vec());
        feedback.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
        feedback.tranche_formats([0u16.to_ne_bytes(), 1u16.to_ne_bytes()].concat());
        feedback.tranche_done();
        feedback.done();
    }
}
#[derive(Default)]
pub struct Params(Mutex<Parameters>);
#[derive(Default)]
struct Parameters {
    plane: Option<Plane>,
    used: bool,
    broken: bool,
}
struct Plane {
    imported: Arc<Imported>,
    offset: u32,
    stride: u32,
    modifier: u64,
}
pub struct Buffer {
    imported: Arc<Imported>,
    surface: Arc<wc_handle::iosection::Surface>,
    spec: BufferSpec,
    sync: Option<SyncPage>,
    mapping: wc_handle::Mapping,
}
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
impl Parameters {
    fn create(&mut self, width: i32, height: i32, format: u32, flags: u32) -> io::Result<Buffer> {
        if self.used {
            return Err(invalid("params already used"));
        }
        self.used = true;
        if self.broken || width <= 0 || height <= 0 || flags != 0 || ![ARGB, XRGB].contains(&format)
        {
            return Err(invalid("unsupported geometry, flags or format"));
        }
        let plane = self
            .plane
            .take()
            .ok_or_else(|| invalid("missing plane zero"))?;
        if plane.modifier != 0 || plane.offset != 0 || plane.stride > i32::MAX as u32 {
            return Err(invalid("only offset-zero linear BGRA supported"));
        }
        plane.imported.validate()?;
        let token = plane.imported.token();
        let surface = wc_handle::iosection::surface(&token.name)
            .ok_or_else(|| invalid("section not owned by live broker owner"))?;
        if surface.plane_count() != 0
            || surface.bytes_per_element() != 4
            || surface.pixel_format() != u32::from_be_bytes(*b"BGRA")
            || surface.width() != width as usize
            || surface.height() != height as usize
            || surface.bytes_per_row() != plane.stride as usize
        {
            return Err(invalid("IOSurface geometry differs from token buffer"));
        }
        let spec = BufferSpec {
            width,
            height,
            stride: plane.stride as i32,
            format: if format == ARGB {
                PixelFormat::Argb8888
            } else {
                PixelFormat::Xrgb8888
            },
        };
        let need = spec
            .required_len()
            .ok_or_else(|| invalid("buffer extent overflow"))?;
        if need as u64 > token.size || need > surface.len() || token.size > surface.len() as u64 {
            return Err(invalid("allocation extent mismatch"));
        }
        let sync = SyncPage::open(plane.imported.clone())?;
        let handle = wc_handle::Handle::from_descriptor(wc_handle::Descriptor::SharedFile {
            path: token.name.clone(),
            size: token.size,
        });
        let mapping = handle.map()?;
        Ok(Buffer {
            imported: plane.imported,
            surface: Arc::new(surface),
            spec,
            sync,
            mapping,
        })
    }
}
impl Buffer {
    fn ready(&self) -> bool {
        self.sync.as_ref().is_none_or(|page| {
            matches!(
                crate::gpu_fence::wait(&self.imported.token().record, page.load()),
                crate::gpu_fence::Waited::Done(_)
            )
        })
    }
}
/// Source extraction retains both the original identity and broker-resolved
/// surface. The renderer holds this tuple until its actual GPU completion.
pub fn gpu_source(
    buffer: &WlBuffer,
) -> Option<(
    Arc<wc_handle::iosection::Surface>,
    BufferSpec,
    Arc<Imported>,
)> {
    let source = buffer.data::<Buffer>()?;
    if !source.ready() {
        return None;
    }
    Some((source.surface.clone(), source.spec, source.imported.clone()))
}
pub struct Source;
impl PixelSource for Source {
    fn name(&self) -> &'static str {
        "native_prime"
    }
    fn with_pixels(
        &self,
        buffer: &WlBuffer,
        f: &mut dyn FnMut(&[u8], BufferSpec),
    ) -> Result<(), PixelError> {
        let source = buffer.data::<Buffer>().ok_or(PixelError::UnknownSource)?;
        if !source.ready() {
            return Err(PixelError::Unreadable("producer fence incomplete".into()));
        }
        let need = source
            .spec
            .required_len()
            .ok_or_else(|| PixelError::Unreadable("buffer extent".into()))?;
        let bytes = source
            .mapping
            .as_slice()
            .get(..need)
            .ok_or_else(|| PixelError::Unreadable("allocation extent".into()))?;
        f(bytes, source.spec);
        Ok(())
    }
}

impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpLinuxDmabufV1>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        let global = data.init(resource, ());
        if global.version() < 4 {
            for format in [ARGB, XRGB] {
                global.format(format);
                if global.version() >= 3 {
                    global.modifier(format, 0, 0);
                }
            }
        }
    }
}
impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_dmabuf_v1::Request::Destroy => {}
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                data.init(params_id, Params::default());
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id }
            | zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => {
                let feedback = data.init(id, ());
                state.native_prime.feedback(&feedback);
            }
            _ => {}
        }
    }
}
impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpLinuxDmabufFeedbackV1,
        _: zwp_linux_dmabuf_feedback_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
fn flags(flags: WEnum<zwp_linux_buffer_params_v1::Flags>) -> u32 {
    match flags {
        WEnum::Value(value) => value.bits(),
        WEnum::Unknown(value) => value,
    }
}
impl Dispatch<ZwpLinuxBufferParamsV1, Params> for State {
    fn request(
        state: &mut Self,
        client: &Client,
        params: &ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &Params,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let mut parameters = data.0.lock().unwrap_or_else(|error| {
            let mut value = error.into_inner();
            value.broken = true;
            value
        });
        match request {
            zwp_linux_buffer_params_v1::Request::Destroy => {}
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                if parameters.used || plane_idx != 0 || parameters.plane.is_some() {
                    parameters.broken = true;
                    return;
                }
                let wayland_backend::os::WireFd::Open(fd) = fd else {
                    parameters.broken = true;
                    return;
                };
                match state.native_prime.registry.import(fd.as_fd()) {
                    Ok(imported) => {
                        parameters.plane = Some(Plane {
                            imported,
                            offset,
                            stride,
                            modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                        })
                    }
                    Err(_) => parameters.broken = true,
                }
            }
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                flags: value,
            } => match parameters.create(width, height, format, flags(value)) {
                Ok(source) => match client.create_resource::<WlBuffer, Buffer, Self>(dh, 1, source)
                {
                    Ok(buffer) => params.created(&buffer),
                    Err(_) => params.failed(),
                },
                Err(_) => params.failed(),
            },
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags: value,
            } => match parameters.create(width, height, format, flags(value)) {
                Ok(source) => {
                    init.init(buffer_id, source);
                }
                Err(error) => {
                    init.post_error(
                        buffer_id,
                        zwp_linux_buffer_params_v1::Error::InvalidWlBuffer,
                        error.to_string(),
                    );
                }
            },
            _ => {}
        }
    }
}
impl Dispatch<WlBuffer, Buffer> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlBuffer,
        _: wl_buffer::Request,
        _: &Buffer,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
    fn destroyed(
        state: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        buffer: &WlBuffer,
        _: &Buffer,
    ) {
        smithay::wayland::buffer::BufferHandler::buffer_destroyed(state, buffer);
    }
}

/// The endpoint shares the compositor's registry. Its owner connections are
/// ordinary native UID-authenticated sockets, never a separate policy service.
/// The existing compositor accept turn calls `accept_pending`; no new timer.
pub struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    registry: Registry,
    workers: Vec<(UnixStream, std::thread::JoinHandle<()>)>,
}
impl Endpoint {
    pub fn bind(wayland_socket: &Path, registry: Registry) -> io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let mut path = wayland_socket.as_os_str().to_os_string();
        path.push(".prime");
        let path = PathBuf::from(path);
        // The caller already holds ListeningSocket's exclusive Wayland
        // name lock. Only that owner may replace its crashed endpoint.
        // Refuse non-sockets and foreign owners even under that lock.
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() && meta.uid() == unsafe { libc::getuid() } => {
                std::fs::remove_file(&path)?
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "foreign PRIME endpoint",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        let result = (|| {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(Self {
            listener,
            path,
            registry,
            workers: Vec::new(),
        })
    }
    pub fn accept_pending(&mut self) -> io::Result<()> {
        // Bound live workers by removing completed owner connections on each
        // existing accept turn. Each remaining connection owns live tokens.
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].1.is_finished() {
                let (_, worker) = self.workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let cancel = stream.try_clone()?;
                    let registry = self.registry.clone();
                    let worker = std::thread::spawn(move || {
                        let _ = sv_drm::native_prime::serve_connection(
                            stream,
                            unsafe { libc::getuid() },
                            &registry,
                        );
                    });
                    self.workers.push((cancel, worker));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        for (stream, _) in &self.workers {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        for (_, worker) in self.workers.drain(..) {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
