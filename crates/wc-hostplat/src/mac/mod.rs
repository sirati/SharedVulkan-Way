//! macOS: CoreAudio devices and streams, Launch Services handlers, `open`
//! (docs/waycompose-design.md §106, the macOS host).
//!
//! * [`audio`]: the devices (UID, name, channels, rate) and the defaults,
//!   and a listener that says when either changed.
//! * [`may_capture`]: the microphone only in the main process
//!   (`org.waycompose.host`, TCC rule 1); the sound ring's recorder
//!   (`wc-launch` `sound_mac.rs`) asks before it opens one.
//! * [`open`]: the `Opener`, `/usr/bin/open` with an argv.
//! * [`handlers`]: Launch Services' applications per type, as offers.
//! * [`plan`]: what all of them decide, pure; compiled and tested on every
//!   target (the rest only on macOS).
//!
//! The main process wires them to the `OWNER_HOSTINT` ring slot
//! (`wc-launch`'s `mac_hostint.rs`).

pub mod plan;

#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod cf;
#[cfg(target_os = "macos")]
pub mod handlers;
#[cfg(target_os = "macos")]
pub mod open;

/// Whether this process may open a microphone: only the main process,
/// whose bundle is `org.waycompose.host` (TCC rule 1). Started any other
/// way (the launcher's fallback direct spawn, a face) it may not, and
/// macOS would otherwise ask about, and keep the grant under, whichever
/// process it takes as responsible.
#[cfg(target_os = "macos")]
pub fn may_capture() -> Result<(), String> {
    match cf::main_bundle_id() {
        Some(id) if id == plan::HOST_BUNDLE_ID => Ok(()),
        other => Err(format!(
            "the microphone is opened only by the main process ({}); this process is {}",
            plan::HOST_BUNDLE_ID,
            other.as_deref().unwrap_or("not a bundle")
        )),
    }
}

/// A plain-text report of what this Mac offers: its audio devices and
/// defaults, this process's bundle and whether it may capture, the browser,
/// and every handler with its types (bundle ids included: the report is
/// for the person testing, on the host, and never crosses).
#[cfg(target_os = "macos")]
pub fn probe() -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let st = audio::state();
    let _ = writeln!(s, "audio: {} device entries", st.devices.len());
    for d in &st.devices {
        let _ = writeln!(s, "  {:?} uid={:?} name={:?} channels={} rate={}", d.dir, d.id, d.description, d.channels, d.rate);
    }
    let _ = writeln!(s, "  default output: {:?}", st.default_output);
    let _ = writeln!(s, "  default input:  {:?}", st.default_input);
    let nodes = wc_hostint::audio::plan(&st, "mac");
    for n in &nodes {
        let _ = writeln!(s, "  node {} stream {} ({} ch, {} Hz)", n.node_name, n.stream, n.channels, n.rate);
    }
    let _ = writeln!(s, "bundle: {:?}; capture: {:?}", cf::main_bundle_id(), may_capture());
    let _ = writeln!(s, "browser {:?} installed: {}", plan::DEFAULT_BROWSER, open::app_installed(plan::DEFAULT_BROWSER));
    let hs = handlers::handlers();
    let _ = writeln!(s, "handlers: {}", hs.len());
    for h in &hs {
        let _ = writeln!(
            s,
            "  {} {:?} ({}) mimes={:?} schemes={:?} default_for={:?}",
            h.offer.token, h.offer.name, h.app, h.offer.mimes, h.offer.schemes, h.offer.default_for
        );
    }
    for (mime, uti) in plan::types().into_iter().filter(|(m, _)| ["text/plain", "text/html"].contains(m)) {
        let (all, default) = handlers::for_uti(uti);
        let _ = writeln!(s, "  raw {mime} ({uti}): all={all:?} default={default:?}");
    }
    s
}
