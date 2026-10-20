//! Authenticated leases for the original joined video allocation.
//! This registry neither allocates native memory nor substitutes a new surface.
use crate::{
    native_video_image::Image,
    native_video_shared_policy::{authorize, validate_layout},
    video_nv12::{Layout, macos::Picture},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use wc_handle::iosection::Surface;

struct Registration {
    name: String,
    nonce: [u8; 16],
    size: u64,
    validity: Arc<crate::native_video_shared_policy::Revocation>,
    // Image exclusively owns VkImage/VkDeviceMemory. The other owners retain
    // the same underlying IOSurface independently, including across withdrawal.
    _image: Arc<Image>,
    picture: Arc<Picture>,
    _surface: Surface,
}
type Registry = HashMap<String, Weak<Registration>>;
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A counted lease, independent of application memory handles. Keep a lease
/// through every accepted use. It does not prove GPU completion or prevent
/// producer reuse; those contracts remain with the producer/consumer owners.
#[derive(Clone)]
pub(crate) struct Lease(Arc<Registration>);
impl Lease {
    pub(crate) fn name(&self) -> &str {
        &self.0.name
    }
    pub(crate) fn nonce(&self) -> [u8; 16] {
        self.0.nonce
    }
    pub(crate) fn size(&self) -> u64 {
        self.0.size
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        // Weak upgrades fail once final destruction begins. Do not hold this
        // mutex across the Mach send, CF releases, or native image destruction.
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.name);
        crate::iosurface::retire(&self.name);
    }
}
fn identity() -> Result<(String, [u8; 16]), String> {
    let mut random = [0u8; 32];
    // SAFETY: a writable 32-byte buffer, below getentropy's 256-byte limit.
    if unsafe { libc::getentropy(random.as_mut_ptr().cast(), random.len()) } != 0 {
        return Err(format!(
            "NV12 export entropy: {}",
            std::io::Error::last_os_error()
        ));
    }
    let counter = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| "NV12 export identity counter exhausted")?;
    let mut tag = String::with_capacity(32);
    use std::fmt::Write;
    for value in &random[..16] {
        write!(&mut tag, "{value:02x}").expect("String write");
    }
    // At most 62 ASCII bytes, within the existing Mach name capability field.
    let name = format!("ios.{:x}.{counter:x}.{tag}", std::process::id());
    let nonce = random[16..]
        .try_into()
        .map_err(|_| "NV12 export nonce length")?;
    Ok((name, nonce))
}

fn same_geometry(picture: &Picture, surface: &Surface) -> Result<u64, String> {
    let layout = picture.layout();
    let original = surface
        .nv12_geometry()
        .map_err(|error| format!("NV12 export geometry: {error:?}"))?
        .surface();
    validate_layout(
        layout,
        Layout {
            width: original.width,
            height: original.height,
            allocation_size: original.allocation_size,
            planes: original.planes.map(|plane| crate::video_nv12::Plane {
                width: plane.width,
                height: plane.height,
                offset: plane.offset,
                stride: plane.bytes_per_row,
            }),
        },
    )
}
/// Register already allocated original storage, including before native bind.
/// Announcement uses the existing broker owner connection and audited Mach
/// endpoint. No token becomes resolvable locally until authenticated ADD ACK.
pub(crate) fn register(image: Arc<Image>) -> Result<Lease, String> {
    let picture = image
        .picture()
        .map_err(|error| format!("NV12 export needs allocated original picture: {error:?}"))?;
    let surface = picture.retain_surface();
    let size = same_geometry(&picture, &surface)?;
    let extent = image.extent();
    if extent.width as usize != picture.layout().width
        || extent.height as usize != picture.layout().height
    {
        return Err("NV12 export image differs from original picture extent".into());
    }
    let (name, nonce) = identity()?;
    let validity = crate::iosurface::announce_nv12(&name, &surface)?;
    let registration = Arc::new(Registration {
        name,
        nonce,
        size,
        validity,
        _image: image,
        picture,
        _surface: surface,
    });
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(registration.name.clone(), Arc::downgrade(&registration));
    Ok(Lease(registration))
}

/// Resolve only an ACKed export still held by a live lease in this process.
/// The returned picture is the same allocation owner, never a decoder output
/// substituted for the target. The requested bound may be smaller, never larger.
pub(crate) fn resolve(
    name: &str,
    size: u64,
    nonce: &[u8; 16],
) -> Result<(Lease, Arc<Picture>), String> {
    let registration = {
        let index = registry().lock().unwrap_or_else(|e| e.into_inner());
        index.get(name).and_then(Weak::upgrade)
    }
    .ok_or("NV12 export is not held by a live lease in this process")?;
    if !registration.validity.active() || !crate::iosurface::nv12_endpoint_alive() {
        return Err("NV12 export owner was revoked".into());
    }
    authorize(&registration.nonce, registration.size, nonce, size)?;
    let picture = Arc::clone(&registration.picture);
    Ok((Lease(registration), picture))
}
