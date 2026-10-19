//! Per-device audio naming: every host output and input is its own sink or
//! source in Linux, named after the host device.
//!
//! The samples, the devices' comings and goings and the defaults travel on
//! the sound ring (sv-transport `soundring`, design §103.16); wc-proxy's
//! `sound.rs` makes each stream a pipe sink or source and names it with
//! [`node_name`]. What is here is the naming, adopted from the real-Mac
//! prototype (linux-vm-agent `guest/tunnels.rs`): the slugs and the
//! order-independent plan, with real Windows and macOS device names as tests.

use std::collections::HashSet;


/// Which way a device's samples go, seen from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    /// A host output (speakers, headphones): a Linux sink, Linux to host.
    Output,
    /// A host input (a microphone): a Linux source, host to Linux.
    Input,
}

impl Dir {
    /// The PipeWire word: an output is a sink, an input a source.
    pub fn pw(self) -> &'static str {
        match self {
            Dir::Output => "sink",
            Dir::Input => "source",
        }
    }
    pub fn wire(self) -> &'static str {
        match self {
            Dir::Output => "out",
            Dir::Input => "in",
        }
    }
    pub fn from_wire(s: &str) -> Option<Dir> {
        match s {
            "out" => Some(Dir::Output),
            "in" => Some(Dir::Input),
            _ => None,
        }
    }
}

/// One host device, as the host enumerates it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Device {
    pub dir: Dir,
    /// The host's stable id: an `IMMDevice` id on Windows
    /// (`{0.0.0.00000000}.{guid}`), a CoreAudio device UID on macOS.
    pub id: String,
    /// What a person calls it: "Speakers (Realtek(R) Audio)", "WH-1000XM5".
    pub description: String,
    /// Channels and rate of the host's mix format, when known.
    pub channels: u16,
    pub rate: u32,
}

/// The host's whole audio state: its devices and its two defaults, by id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub devices: Vec<Device>,
    pub default_output: Option<String>,
    pub default_input: Option<String>,
}

/// A node in Linux for one host device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub dir: Dir,
    pub host_id: String,
    /// `<prefix>.sink.<slug>` / `<prefix>.source.<slug>`.
    pub node_name: String,
    pub description: String,
    /// The node's stream on the sound ring: 0.. in plan order. Stable for as
    /// long as the device set is.
    pub stream: u16,
    pub channels: u16,
    pub rate: u32,
}

/// Lowercase ASCII letters, digits and `-`; anything else becomes one `_`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() { "device".to_string() } else { out.to_string() }
}

/// Slugs in input order; repeats get `_2`, `_3`, ... skipping taken names.
pub fn assign_slugs<'a>(descriptions: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut used = HashSet::new();
    let mut out = Vec::new();
    for d in descriptions {
        let base = slugify(d);
        let mut slug = base.clone();
        let mut n = 2;
        while used.contains(&slug) {
            slug = format!("{base}_{n}");
            n += 1;
        }
        used.insert(slug.clone());
        out.push(slug);
    }
    out
}

/// The nodes for a host state. `prefix` names the host ("win", "mac").
/// Sorted by (direction, description, id), so node names and stream numbers
/// do not depend on the host's enumeration order.
pub fn plan(state: &State, prefix: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut stream = 0u16;
    for dir in [Dir::Output, Dir::Input] {
        let mut ds: Vec<&Device> = state.devices.iter().filter(|d| d.dir == dir).collect();
        ds.sort_by(|a, b| (&a.description, &a.id).cmp(&(&b.description, &b.id)));
        ds.dedup_by(|a, b| a.id == b.id);
        let slugs = assign_slugs(ds.iter().map(|d| d.description.as_str()));
        for (d, slug) in ds.into_iter().zip(slugs) {
            nodes.push(Node {
                dir,
                host_id: d.id.clone(),
                node_name: format!("{prefix}.{}.{slug}", dir.pw()),
                description: d.description.clone(),
                stream,
                channels: if d.channels == 0 { 2 } else { d.channels.min(8) },
                rate: if d.rate == 0 { 48_000 } else { d.rate },
            });
            stream += 1;
        }
    }
    nodes
}

/// The name of one more stream's sink or source, given the names already in
/// use: `<prefix>.<sink|source>.<slug>`, then `_2`, `_3`, ... until it is
/// free. For the sound ring, whose devices arrive one at a time, so the
/// name depends on the arrival order only when two devices share a slug.
pub fn node_name(prefix: &str, dir: Dir, description: &str, taken: &[String]) -> String {
    let base = format!("{prefix}.{}.{}", dir.pw(), slugify(description));
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(dir: Dir, id: &str, d: &str) -> Device {
        Device { dir, id: id.into(), description: d.into(), channels: 2, rate: 48_000 }
    }

    /// Real devices: the prototype's Mac, and the descriptions Windows gives
    /// common endpoints ("<endpoint> (<adapter>)", localised).
    fn mac() -> State {
        State {
            devices: vec![
                dev(Dir::Output, "BuiltInSpeakerDevice", "MacBook Neo Speakers"),
                dev(Dir::Output, "AC-80-0A-00-00-01:output", "WH-1000XM5"),
                dev(Dir::Input, "BuiltInMicrophoneDevice", "MacBook Neo Microphone"),
                dev(Dir::Input, "AC-80-0A-00-00-01:input", "WH-1000XM5"),
            ],
            default_output: Some("AC-80-0A-00-00-01:output".into()),
            default_input: Some("BuiltInMicrophoneDevice".into()),
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("WH-1000XM5"), "wh-1000xm5");
        assert_eq!(slugify("MacBook Neo Speakers"), "macbook_neo_speakers");
        // The Sonoma VM's only device, QEMU's usb-audio, as CoreAudio names it
        // (kAudioObjectPropertyName), with UID
        // "AppleUSBAudioEngine:QEMU:QEMU USB Audio:1-0000:00:01.0-3:1".
        assert_eq!(slugify("Audio Output - Disabled"), "audio_output_-_disabled");
        assert_eq!(slugify("  Ünïcode  / weird!! "), "n_code_weird");
        assert_eq!(slugify("***"), "device");
        assert_eq!(slugify("a.b\"c"), "a_b_c");
        // Windows endpoint names.
        assert_eq!(slugify("Speakers (Realtek(R) Audio)"), "speakers_realtek_r_audio");
        assert_eq!(slugify("Lautsprecher (2- High Definition Audio-Gerät)"), "lautsprecher_2-_high_definition_audio-ger_t");
        assert_eq!(slugify("Microphone Array (Intel® Smart Sound Technology for Digital Microphones)"),
            "microphone_array_intel_smart_sound_technology_for_digital_microphones");
        assert_eq!(slugify("Kopfhörer (WH-1000XM5 Stereo)"), "kopfh_rer_wh-1000xm5_stereo");
    }

    #[test]
    fn slug_dedup() {
        assert_eq!(
            assign_slugs(["USB Audio", "USB Audio", "USB-Audio", "usb audio"]),
            ["usb_audio", "usb_audio_2", "usb-audio", "usb_audio_3"]
        );
        // A literal "x_2" description must not collide with a generated suffix.
        assert_eq!(assign_slugs(["x_2", "x", "x"]), ["x_2", "x", "x_3"]);
        assert_eq!(assign_slugs(["", "!"]), ["device", "device_2"]);
    }

    #[test]
    fn plan_is_order_independent_and_numbers_streams() {
        let mut s = mac();
        let a = plan(&s, "mac");
        s.devices.reverse();
        assert_eq!(plan(&s, "mac"), a);
        let names: Vec<_> = a.iter().map(|n| (n.node_name.as_str(), n.stream)).collect();
        assert_eq!(
            names,
            [
                ("mac.sink.macbook_neo_speakers", 0),
                ("mac.sink.wh-1000xm5", 1),
                ("mac.source.macbook_neo_microphone", 2),
                ("mac.source.wh-1000xm5", 3)
            ]
        );
    }

    #[test]
    fn two_identical_windows_devices_get_numbered() {
        let s = State {
            devices: vec![
                dev(Dir::Output, "{0.0.0.00000000}.{b}", "Speakers (USB Audio Device)"),
                dev(Dir::Output, "{0.0.0.00000000}.{a}", "Speakers (USB Audio Device)"),
            ],
            ..State::default()
        };
        let p = plan(&s, "win");
        assert_eq!(p[0].node_name, "win.sink.speakers_usb_audio_device");
        assert_eq!(p[0].host_id, "{0.0.0.00000000}.{a}", "ties broken by id, so stable");
        assert_eq!(p[1].node_name, "win.sink.speakers_usb_audio_device_2");
    }

    /// The test guest's real endpoints (QEMU's HDA, German Windows), as
    /// `wc-hostint-probe` enumerated them: the friendly names keep the
    /// "2- " instance prefix Windows adds, and the slug keeps its `-`.
    #[test]
    fn the_test_guest_s_real_endpoints() {
        let s = State {
            devices: vec![
                dev(Dir::Output, "{0.0.0.00000000}.{e8cd0507-c75f-49a0-a4eb-0178ba4d06bf}", "Lautsprecher (2- High Definition Audio Device)"),
                dev(Dir::Input, "{0.0.1.00000000}.{d1580365-c840-42bc-9a5c-a1982d29fd36}", "Eingang (2- High Definition Audio Device)"),
            ],
            default_output: Some("{0.0.0.00000000}.{e8cd0507-c75f-49a0-a4eb-0178ba4d06bf}".into()),
            default_input: Some("{0.0.1.00000000}.{d1580365-c840-42bc-9a5c-a1982d29fd36}".into()),
        };
        let names: Vec<String> = plan(&s, "win").into_iter().map(|n| n.node_name).collect();
        assert_eq!(names, ["win.sink.lautsprecher_2-_high_definition_audio_device", "win.source.eingang_2-_high_definition_audio_device"]);
    }


    #[test]
    fn node_names_are_slugs_deduplicated_against_what_is_in_use() {
        let mut taken: Vec<String> = Vec::new();
        for (dir, d) in [
            (Dir::Output, "Speakers (Realtek(R) Audio)"),
            (Dir::Output, "Speakers (Realtek(R) Audio)"),
            (Dir::Input, "Speakers (Realtek(R) Audio)"),
            (Dir::Output, "Speakers (Realtek(R) Audio)"),
            (Dir::Input, "MacBook Neo Microphone"),
        ] {
            let n = node_name("win", dir, d, &taken);
            taken.push(n);
        }
        assert_eq!(
            taken,
            [
                "win.sink.speakers_realtek_r_audio",
                "win.sink.speakers_realtek_r_audio_2",
                "win.source.speakers_realtek_r_audio",
                "win.sink.speakers_realtek_r_audio_3",
                "win.source.macbook_neo_microphone",
            ]
        );
        // A freed name is used again; a literal suffix does not collide.
        let taken = vec!["mac.sink.x_2".to_string()];
        assert_eq!(node_name("mac", Dir::Output, "x", &taken), "mac.sink.x");
        assert_eq!(node_name("mac", Dir::Output, "x 2", &taken), "mac.sink.x_2_2");
        assert_eq!(node_name("mac", Dir::Output, "***", &[]), "mac.sink.device");
        // The plan's names and the incremental ones agree for distinct slugs.
        let s = State {
            devices: vec![dev(Dir::Output, "a", "WH-1000XM5"), dev(Dir::Input, "b", "MacBook Neo Microphone")],
            ..State::default()
        };
        for n in plan(&s, "mac") {
            assert_eq!(node_name("mac", n.dir, &n.description, &[]), n.node_name);
        }
    }
}
