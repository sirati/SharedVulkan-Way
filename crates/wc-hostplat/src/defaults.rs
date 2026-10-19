//! A record of the host's default audio endpoints, per direction and role,
//! as `wc-hostint-probe --get-defaults` prints it and `--restore-defaults`
//! reads it back: one line each, `<render|capture> <role> <endpoint-id|->`.
//! Windows has three roles (`eConsole`, `eMultimedia`, `eCommunications`)
//! and a test that switches the default must put every one of them back.

/// `EDataFlow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Render,
    Capture,
}

/// `ERole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

pub const FLOWS: [Flow; 2] = [Flow::Render, Flow::Capture];
pub const ROLES: [Role; 3] = [Role::Console, Role::Multimedia, Role::Communications];

impl Flow {
    pub fn word(self) -> &'static str {
        match self {
            Flow::Render => "render",
            Flow::Capture => "capture",
        }
    }
    pub fn parse(s: &str) -> Option<Flow> {
        FLOWS.into_iter().find(|f| f.word() == s)
    }
    /// The `EDataFlow` value.
    pub fn value(self) -> i32 {
        match self {
            Flow::Render => 0,
            Flow::Capture => 1,
        }
    }
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Console => "console",
            Role::Multimedia => "multimedia",
            Role::Communications => "communications",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        ROLES.into_iter().find(|r| r.word() == s)
    }
    /// The `ERole` value.
    pub fn value(self) -> i32 {
        match self {
            Role::Console => 0,
            Role::Multimedia => 1,
            Role::Communications => 2,
        }
    }
    /// `console`, `multimedia`, `communications`, or `all`.
    pub fn parse_many(s: &str) -> Option<Vec<Role>> {
        if s == "all" { Some(ROLES.to_vec()) } else { Role::parse(s).map(|r| vec![r]) }
    }
}

/// One default: `None` when the direction has no device at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Default {
    pub flow: Flow,
    pub role: Role,
    pub id: Option<String>,
}

/// An endpoint id as Windows writes them: `{0.0.0.00000000}.{guid}`, one
/// word of printable ASCII.
pub fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && s != "-" && s.chars().all(|c| c.is_ascii_graphic())
}

pub fn format(ds: &[Default]) -> String {
    ds.iter().map(|d| format!("{} {} {}\n", d.flow.word(), d.role.word(), d.id.as_deref().unwrap_or("-"))).collect()
}

/// Read [`format`]'s output back. Blank lines and `#` comments are skipped;
/// anything else that does not parse is an error naming its line.
pub fn parse(text: &str) -> Result<Vec<Default>, String> {
    let mut out = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let w: Vec<&str> = l.split_whitespace().collect();
        let bad = || format!("line {}: expected `<render|capture> <role> <endpoint-id|->`", n + 1);
        let [f, r, id] = w.as_slice() else { return Err(bad()) };
        let flow = Flow::parse(f).ok_or_else(bad)?;
        let role = Role::parse(r).ok_or_else(bad)?;
        let id = match *id {
            "-" => None,
            i if id_ok(i) => Some(i.to_string()),
            _ => return Err(bad()),
        };
        out.push(Default { flow, role, id });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_errors() {
        let ds = vec![
            Default { flow: Flow::Render, role: Role::Console, id: Some("{0.0.0.00000000}.{e8cd0507-c75f-49a0-a4eb-0178ba4d06bf}".into()) },
            Default { flow: Flow::Render, role: Role::Communications, id: Some("{0.0.0.00000000}.{aa}".into()) },
            Default { flow: Flow::Capture, role: Role::Multimedia, id: None },
        ];
        let t = format(&ds);
        assert_eq!(t.lines().nth(2), Some("capture multimedia -"));
        assert_eq!(parse(&format!("# saved\n\n{t}")).unwrap(), ds);
        assert!(parse("render loud {x}").is_err());
        assert!(parse("render console").is_err());
        assert!(parse("speaker console {x}").is_err());
        assert_eq!(Role::parse_many("all").unwrap().len(), 3);
        assert_eq!(Role::parse_many("communications").unwrap(), [Role::Communications]);
        assert!(Role::parse_many("everything").is_none());
        assert_eq!((Flow::Capture.value(), Role::Communications.value()), (1, 2));
    }
}
