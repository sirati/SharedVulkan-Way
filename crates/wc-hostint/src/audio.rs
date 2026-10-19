//! Per-device audio: every host output and input becomes its own PipeWire
//! sink or source in Linux, named after the host device, and Linux's defaults
//! follow the host's.
//!
//! Adopted from the real-Mac prototype (linux-vm-agent `guest/tunnels.rs`):
//! the slugs, the order-independent plan, the reconciler that restarts the
//! child PipeWire only when the device *set* changes, and the `pw-metadata`
//! argv. What differs is how a node's samples move. The prototype tunnelled
//! each node to a PulseAudio on the Mac over vsock; here nothing crosses but
//! shared memory, so each node is a `libpipewire-module-pipe-tunnel` on a
//! FIFO in the session's run directory, and `wc-proxy` copies between that
//! FIFO and the node's stream on the sound ring. The FIFO is local to Linux.

use std::collections::HashSet;
use std::fmt::Write;

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

/// SPA-JSON string literal.
fn spa_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The FIFO a node's samples pass through, in `dir` (the session's run
/// directory). Named by stream number: the description is not a file name.
pub fn fifo_path(dir: &str, n: &Node) -> String {
    format!("{dir}/{}-{}.fifo", n.dir.pw(), n.stream)
}

/// The fixed sample format on every FIFO and ring stream. Rate and channels
/// follow the host device; the sample type is always s16le, which is what
/// both WASAPI shared mode and CoreAudio convert from without a fuss.
pub const FORMAT: &str = "S16LE";

/// A client-only PipeWire instance (no server socket of its own; it connects
/// to the session's daemon) hosting one pipe-tunnel node per host device.
pub fn render_config(nodes: &[Node], fifo_dir: &str) -> String {
    let mut s = String::from(
        "# Generated by wc-proxy (wc-hostint::audio). Do not edit.
context.properties = {
    log.level = 0
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-rt
        args = { }
        flags = [ ifexists nofail ]
    }
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
",
    );
    for n in nodes {
        let _ = write!(
            s,
            "    {{ name = libpipewire-module-pipe-tunnel
        args = {{
            tunnel.mode = {mode}
            pipe.filename = {fifo}
            audio.format = {FORMAT}
            audio.rate = {rate}
            audio.channels = {ch}
            stream.props = {{
                node.name = {name}
                node.description = {desc}
                media.class = {class}
                wc.host.id = {id}
            }}
        }}
    }}
",
            mode = n.dir.pw(),
            fifo = spa_str(&fifo_path(fifo_dir, n)),
            rate = n.rate,
            ch = n.channels,
            name = spa_str(&n.node_name),
            desc = spa_str(&n.description),
            class = spa_str(match n.dir {
                Dir::Output => "Audio/Sink",
                Dir::Input => "Audio/Source",
            }),
            id = spa_str(&n.host_id),
        );
    }
    s.push_str("]\n");
    s
}

/// Configured defaults, as node names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Defaults {
    pub sink: Option<String>,
    pub source: Option<String>,
}

fn node_for(nodes: &[Node], dir: Dir, id: Option<&str>) -> Option<String> {
    let want = id?;
    nodes.iter().find(|n| n.dir == dir && n.host_id == want).map(|n| n.node_name.clone())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    /// A new node set: the child PipeWire must be (re)started, or stopped if
    /// the set is empty. It applies the defaults itself once its nodes exist.
    pub restart: Option<Vec<Node>>,
    /// Only the defaults changed: set them on the running nodes.
    pub apply_defaults: bool,
}

/// Turns each host state into what the Linux side must do. The child is
/// restarted only when the device set changes, never for a default.
#[derive(Default)]
pub struct Reconciler {
    prefix: String,
    nodes: Option<Vec<Node>>,
    defaults: Defaults,
}

impl Reconciler {
    pub fn new(prefix: &str) -> Self {
        Self { prefix: prefix.into(), ..Self::default() }
    }

    pub fn update(&mut self, state: &State) -> Decision {
        let nodes = plan(state, &self.prefix);
        let defaults = Defaults {
            sink: node_for(&nodes, Dir::Output, state.default_output.as_deref()),
            source: node_for(&nodes, Dir::Input, state.default_input.as_deref()),
        };
        let restart = self.nodes.as_ref() != Some(&nodes);
        let changed = defaults != self.defaults;
        self.nodes = Some(nodes.clone());
        self.defaults = defaults;
        let any = self.defaults.sink.is_some() || self.defaults.source.is_some();
        Decision { restart: restart.then_some(nodes), apply_defaults: !restart && changed && any }
    }

    pub fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    pub fn nodes(&self) -> &[Node] {
        self.nodes.as_deref().unwrap_or(&[])
    }
}

/// `node.name`s of the nodes in `pw-dump` output. A hand parse of the one
/// shape needed, so this crate needs no JSON library: every object's
/// `"type": "PipeWire:Interface:Node"` and the `"node.name"` that follows it
/// before the next object's type.
pub fn pw_dump_node_names(json: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for obj in json.split("\"type\"").skip(1) {
        let Some(rest) = obj.trim_start().strip_prefix(':') else { continue };
        if !rest.trim_start().starts_with("\"PipeWire:Interface:Node\"") {
            continue;
        }
        if let Some(i) = rest.find("\"node.name\"") {
            let r = rest[i + "\"node.name\"".len()..].trim_start();
            if let Some(r) = r.strip_prefix(':') {
                if let Some(v) = json_string(r.trim_start()) {
                    out.insert(v);
                }
            }
        }
    }
    out
}

/// The JSON string literal at the start of `s`.
fn json_string(s: &str) -> Option<String> {
    let mut it = s.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = it.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

fn json_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// argv (without the program) for `pw-metadata` setting a configured default.
pub fn metadata_args(dir: Dir, node_name: &str) -> Vec<String> {
    vec![
        "-n".into(),
        "default".into(),
        "0".into(),
        format!("default.configured.audio.{}", dir.pw()),
        format!("{{\"name\":{}}}", json_quote(node_name)),
        "Spa:String:JSON".into(),
    ]
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

    #[test]
    fn config_golden() {
        let state = State {
            devices: vec![
                dev(Dir::Output, "o1", "WH-1000XM5"),
                Device { channels: 1, rate: 44_100, ..dev(Dir::Input, "i1", "MacBook \"Neo\" Microphone") },
            ],
            ..State::default()
        };
        let got = render_config(&plan(&state, "mac"), "/run/user/1000/wc/s1");
        let want = r#"# Generated by wc-proxy (wc-hostint::audio). Do not edit.
context.properties = {
    log.level = 0
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-rt
        args = { }
        flags = [ ifexists nofail ]
    }
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-pipe-tunnel
        args = {
            tunnel.mode = sink
            pipe.filename = "/run/user/1000/wc/s1/sink-0.fifo"
            audio.format = S16LE
            audio.rate = 48000
            audio.channels = 2
            stream.props = {
                node.name = "mac.sink.wh-1000xm5"
                node.description = "WH-1000XM5"
                media.class = "Audio/Sink"
                wc.host.id = "o1"
            }
        }
    }
    { name = libpipewire-module-pipe-tunnel
        args = {
            tunnel.mode = source
            pipe.filename = "/run/user/1000/wc/s1/source-1.fifo"
            audio.format = S16LE
            audio.rate = 44100
            audio.channels = 1
            stream.props = {
                node.name = "mac.source.macbook_neo_microphone"
                node.description = "MacBook \"Neo\" Microphone"
                media.class = "Audio/Source"
                wc.host.id = "i1"
            }
        }
    }
]
"#;
        assert_eq!(got, want);
    }

    #[test]
    fn only_device_set_changes_restart() {
        let mut r = Reconciler::new("mac");
        let s = mac();

        let d = r.update(&s);
        assert_eq!(d.restart.as_ref().map(Vec::len), Some(4));
        assert!(!d.apply_defaults, "a restart applies the defaults itself");
        assert_eq!(r.defaults().sink.as_deref(), Some("mac.sink.wh-1000xm5"));
        assert_eq!(r.defaults().source.as_deref(), Some("mac.source.macbook_neo_microphone"));

        assert_eq!(r.update(&s), Decision { restart: None, apply_defaults: false });

        let mut reordered = s.clone();
        reordered.devices.reverse();
        assert_eq!(r.update(&reordered), Decision { restart: None, apply_defaults: false });

        // Only the default changes: no restart, apply defaults.
        let mut s2 = s.clone();
        s2.default_output = Some("BuiltInSpeakerDevice".into());
        assert_eq!(r.update(&s2), Decision { restart: None, apply_defaults: true });
        assert_eq!(r.defaults().sink.as_deref(), Some("mac.sink.macbook_neo_speakers"));

        // Defaults become unknown: nothing to apply.
        let mut s3 = s2.clone();
        s3.default_output = None;
        s3.default_input = None;
        assert_eq!(r.update(&s3), Decision { restart: None, apply_defaults: false });

        // A device is plugged in: restart.
        let mut s4 = s2.clone();
        s4.devices.push(dev(Dir::Output, "airpods", "AirPods Pro"));
        assert_eq!(r.update(&s4).restart.map(|t| t.len()), Some(5));

        // A rename renames the node: restart.
        let mut s5 = s4.clone();
        s5.devices[4].description = "Kitchen".into();
        assert!(r.update(&s5).restart.is_some());

        // All gone: restart with nothing (stop the child).
        assert_eq!(r.update(&State::default()).restart, Some(vec![]));
    }

    #[test]
    fn pw_dump_parsing() {
        let json = r#"[
          {"id":0,"type":"PipeWire:Interface:Core","info":{"props":{"node.name":"nope"}}},
          {"id":40,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mac.sink.wh-1000xm5","media.class":"Audio/Sink"}}},
          {"id":41,"type":"PipeWire:Interface:Node","info":{"props":{}}},
          {"id":42,"type":"PipeWire:Interface:Metadata","props":{"metadata.name":"default"}}
        ]"#;
        let names = pw_dump_node_names(json);
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["mac.sink.wh-1000xm5"]);
        assert!(pw_dump_node_names("not json").is_empty());
        // pw-dump's real layout: pretty-printed, with spaces around ':'.
        let pretty = "[\n  {\n    \"id\": 40,\n    \"type\": \"PipeWire:Interface:Node\",\n    \"info\": { \"props\": { \"node.name\": \"win.sink.a\\u00e4\" } }\n  }\n]";
        assert_eq!(pw_dump_node_names(pretty).into_iter().collect::<Vec<_>>(), ["win.sink.aä"]);
    }

    #[test]
    fn metadata_argv() {
        assert_eq!(
            metadata_args(Dir::Input, "mac.source.x\"y"),
            ["-n", "default", "0", "default.configured.audio.source", r#"{"name":"mac.source.x\"y"}"#, "Spa:String:JSON"]
        );
    }
}
