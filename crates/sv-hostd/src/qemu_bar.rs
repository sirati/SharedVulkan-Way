//! Read-only QMP identity checks for an existing host-side ivshmem BAR.
//! No backend is created, resized, or selected by a pathname supplied by a client.
use crate::qemu::Qmp;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub struct QemuBar {
    pub file: File,
    pub pid: u32,
    pub len: u64,
    pub device: String,
    pub backend: String,
    pub fd_path: PathBuf,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn reply(qmp: &mut Qmp, command: Value) -> io::Result<Value> {
    let text = qmp.command(&command.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    value
        .get("return")
        .cloned()
        .ok_or_else(|| invalid("QMP omitted return value"))
}

fn find_device<'a>(value: &'a Value, id: &str, matches: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            if object.get("qdev_id").and_then(Value::as_str) == Some(id) {
                matches.push(value);
            }
            for child in object.values() {
                find_device(child, id, matches);
            }
        }
        Value::Array(array) => {
            for child in array {
                find_device(child, id, matches);
            }
        }
        _ => {}
    }
}

fn device_size(pci: &Value, id: &str, slot: u8) -> io::Result<u64> {
    let mut matches = Vec::new();
    find_device(pci, id, &mut matches);
    if matches.len() != 1 {
        return Err(invalid("QMP device identity is absent or ambiguous"));
    }
    let device = matches[0];
    if device["slot"].as_u64() != Some(u64::from(slot))
        || device["function"].as_u64() != Some(0)
        || device["id"]["vendor"].as_u64() != Some(0x1af4)
        || device["id"]["device"].as_u64() != Some(0x1110)
    {
        return Err(invalid(
            "QMP PCI slot/function/vendor/device differs from ivshmem identity",
        ));
    }
    let bars = device["regions"]
        .as_array()
        .ok_or_else(|| invalid("QMP omitted PCI BARs"))?;
    let matches: Vec<_> = bars
        .iter()
        .filter(|bar| bar["bar"].as_u64() == Some(2))
        .collect();
    if matches.len() != 1 || matches[0]["type"].as_str() != Some("memory") {
        return Err(invalid("QMP ivshmem BAR2 is absent or ambiguous"));
    }
    matches[0]["size"]
        .as_u64()
        .filter(|size| *size > 0)
        .ok_or_else(|| invalid("QMP ivshmem BAR2 has no valid size"))
}

impl QemuBar {
    /// Bind QMP's live PCI device and shared backend to the exact file held
    /// open by the QMP peer process. Retain this object while its mappings live.
    pub fn open(qmp_path: &Path, device: &str, slot: u8, write: bool) -> io::Result<Self> {
        if device.is_empty()
            || !device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(invalid("invalid QEMU device id"));
        }
        let mut qmp = Qmp::connect(qmp_path)?;
        let pid = qmp.peer_pid()?;
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))?;
        let qemu_name = executable.file_name().map(|name| name.to_string_lossy());
        let valid_name = qemu_name.as_deref().is_some_and(|name| {
            let name = name.strip_prefix('.').unwrap_or(name);
            let name = name.strip_suffix("-wrapped").unwrap_or(name);
            name == "qemu-system-x86_64"
        });
        if !valid_name {
            return Err(invalid(format!(
                "QMP peer {pid} is not a supported QEMU system process: {}",
                executable.display()
            )));
        }
        let status = reply(&mut qmp, json!({"execute":"query-status"}))?;
        if status["running"].as_bool() != Some(true) {
            return Err(invalid("QEMU guest is not running"));
        }
        let pci = reply(&mut qmp, json!({"execute":"query-pci"}))?;
        let len = device_size(&pci, device, slot)?;
        let path = format!("/machine/peripheral/{device}");
        let backend_value = reply(
            &mut qmp,
            json!({"execute":"qom-get","arguments":{"path":path,"property":"memdev"}}),
        )?;
        let backend_path = backend_value
            .as_str()
            .ok_or_else(|| invalid("QMP memdev is not a path"))?;
        let backend = backend_path
            .strip_prefix("/objects/")
            .filter(|s| !s.is_empty() && !s.contains('/'))
            .ok_or_else(|| invalid("QMP backend is outside /objects"))?
            .to_owned();
        let memdevs = reply(&mut qmp, json!({"execute":"query-memdev"}))?;
        let matches: Vec<_> = memdevs
            .as_array()
            .ok_or_else(|| invalid("QMP omitted memory backends"))?
            .iter()
            .filter(|entry| entry["id"].as_str() == Some(&backend))
            .collect();
        if matches.len() != 1
            || matches[0]["share"].as_bool() != Some(true)
            || matches[0]["size"].as_u64() != Some(len)
        {
            return Err(invalid(
                "QMP backend is not shared or differs from PCI BAR size",
            ));
        }
        let backing_value = reply(
            &mut qmp,
            json!({"execute":"qom-get","arguments":{"path":backend_path,"property":"mem-path"}}),
        )?;
        let backing = backing_value
            .as_str()
            .ok_or_else(|| invalid("QMP file backend has no mem-path"))?;
        let metadata = std::fs::metadata(backing)?;
        if !metadata.is_file() || metadata.len() != len {
            return Err(invalid("QMP backing length/type differs"));
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
            let entry = entry?;
            // Inspect metadata first; never open unrelated guest disks or devices.
            let Ok(candidate) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if candidate.dev() != metadata.dev()
                || candidate.ino() != metadata.ino()
                || candidate.len() != len
            {
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(true)
                .write(write)
                .open(entry.path())
            else {
                continue;
            };
            let actual = file.metadata()?;
            if actual.dev() == metadata.dev()
                && actual.ino() == metadata.ino()
                && actual.len() == len
            {
                files.push((entry.path(), file));
            }
        }
        let (fd_path, file) = files
            .into_iter()
            .next()
            .ok_or_else(|| invalid("QEMU does not hold the advertised backing file"))?;
        // Re-read the live device after opening, rather than blessing a device
        // that disappeared while its backend was being identified.
        let pci = reply(&mut qmp, json!({"execute":"query-pci"}))?;
        if device_size(&pci, device, slot)? != len {
            return Err(invalid("QEMU BAR identity changed while opening"));
        }
        if write {
            use std::os::fd::AsRawFd;
            // A host initializer keeps exclusive ownership of this backend.
            // Readers use separate read-only mappings; another writer fails.
            // SAFETY: a live retained file descriptor and valid flock flags.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            file,
            pid,
            len,
            device: device.to_owned(),
            backend,
            fd_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pci() -> Value {
        json!([{"devices":[{"qdev_id":"arena","slot":28,"function":0,
            "id":{"vendor":6900,"device":4368},
            "regions":[{"bar":2,"type":"memory","size":67108864}]}]}])
    }
    #[test]
    fn requires_exact_live_device_identity() {
        assert_eq!(device_size(&pci(), "arena", 28).unwrap(), 67108864);
        assert!(device_size(&pci(), "arena", 29).is_err());
        assert!(device_size(&pci(), "other", 28).is_err());
        let mut wrong = pci();
        wrong[0]["devices"][0]["id"]["device"] = json!(1);
        assert!(device_size(&wrong, "arena", 28).is_err());
    }
    #[test]
    fn rejects_ambiguous_or_missing_bar() {
        let mut duplicate = pci();
        let device = duplicate[0]["devices"][0].clone();
        duplicate[0]["devices"].as_array_mut().unwrap().push(device);
        assert!(device_size(&duplicate, "arena", 28).is_err());
        let mut wrong = pci();
        wrong[0]["devices"][0]["regions"] = json!([]);
        assert!(device_size(&wrong, "arena", 28).is_err());
    }
}
