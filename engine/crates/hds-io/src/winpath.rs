//! Classification of Windows path strings. Pure string work on UTF-16 units,
//! compiled and tested on every platform; the Windows-only code decides what
//! to open on the basis of these answers.

// Some of this is only called from the Windows-only modules.
#![cfg_attr(not(windows), allow(dead_code))]

use std::net::{Ipv4Addr, Ipv6Addr};

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const COLON: u16 = b':' as u16;

fn is_sep(unit: u16) -> bool {
    unit == BACKSLASH || unit == SLASH
}

fn is_ascii_letter(unit: u16) -> bool {
    (b'a' as u16..=b'z' as u16).contains(&unit) || (b'A' as u16..=b'Z' as u16).contains(&unit)
}

fn eq_ignore_ascii_case(units: &[u16], text: &str) -> bool {
    units.len() == text.len()
        && units
            .iter()
            .zip(text.bytes())
            .all(|(&u, b)| u < 128 && (u as u8).eq_ignore_ascii_case(&b))
}

/// What kind of path a string is, as far as the engine cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WinPath {
    /// `X:\...`: absolute on a drive letter.
    Drive,
    /// `\\server\share\...`
    Unc {
        /// The server component.
        server: String,
    },
    /// `\\?\X:\...`
    VerbatimDrive,
    /// `\\?\UNC\server\share\...`
    VerbatimUnc {
        /// The server component.
        server: String,
    },
    /// Not an absolute drive or UNC path; the text says why.
    Invalid(&'static str),
}

impl WinPath {
    /// True for the two verbatim (`\\?\`) forms, which Windows does not normalise.
    pub(crate) fn is_verbatim(&self) -> bool {
        matches!(self, WinPath::VerbatimDrive | WinPath::VerbatimUnc { .. })
    }

    /// The server of a UNC path.
    pub(crate) fn server(&self) -> Option<&str> {
        match self {
            WinPath::Unc { server } | WinPath::VerbatimUnc { server } => Some(server),
            _ => None,
        }
    }
}

/// Splits `server\share...` and requires both components to be non-empty.
fn unc_server(rest: &[u16]) -> Option<String> {
    let server_end = rest.iter().position(|&u| is_sep(u))?;
    let server = &rest[..server_end];
    let after = &rest[server_end + 1..];
    let share_end = after.iter().position(|&u| is_sep(u)).unwrap_or(after.len());
    if server.is_empty() || share_end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(server))
}

/// Classifies a path string. Only [`WinPath::Drive`], [`WinPath::Unc`] and
/// their verbatim spellings name a file or directory by an absolute path;
/// relative paths, drive-relative paths (`C:foo`), root-relative paths
/// (`\foo`) and everything in the device namespaces (`\\.\`, `\\?\Volume{..}`,
/// `\\?\GLOBALROOT`, a bare `\\?\C:`) are [`WinPath::Invalid`].
pub(crate) fn classify(path: &[u16]) -> WinPath {
    if path.is_empty() {
        return WinPath::Invalid("the path is empty");
    }
    if path.contains(&0) {
        return WinPath::Invalid("the path contains a NUL character");
    }
    if path.len() >= 2 && is_sep(path[0]) && is_sep(path[1]) {
        let rest = &path[2..];
        let first_len = rest.iter().position(|&u| is_sep(u)).unwrap_or(rest.len());
        let first = &rest[..first_len];
        if first == [b'.' as u16] {
            return WinPath::Invalid("device paths (\\\\.\\...) are not accepted");
        }
        if first == [b'?' as u16] {
            // Windows only treats the exact spelling `\\?\` as verbatim.
            if path[..2] != [BACKSLASH, BACKSLASH] || rest.get(1) != Some(&BACKSLASH) {
                return WinPath::Invalid("device paths are not accepted");
            }
            let tail = &rest[2..];
            if tail.len() >= 3
                && is_ascii_letter(tail[0])
                && tail[1] == COLON
                && tail[2] == BACKSLASH
            {
                return WinPath::VerbatimDrive;
            }
            if tail.len() > 4 && eq_ignore_ascii_case(&tail[..3], "UNC") && tail[3] == BACKSLASH {
                return match unc_server(&tail[4..]) {
                    Some(server) => WinPath::VerbatimUnc { server },
                    None => WinPath::Invalid("the UNC path names no share"),
                };
            }
            return WinPath::Invalid("device paths (\\\\?\\...) are not accepted");
        }
        return match unc_server(rest) {
            Some(server) => WinPath::Unc { server },
            None => WinPath::Invalid("the UNC path names no share"),
        };
    }
    if path.len() >= 2 && is_ascii_letter(path[0]) && path[1] == COLON {
        return if path.len() >= 3 && is_sep(path[2]) {
            WinPath::Drive
        } else {
            WinPath::Invalid("the path is relative to the current directory of a drive")
        };
    }
    if is_sep(path[0]) {
        return WinPath::Invalid("the path names no drive");
    }
    WinPath::Invalid("the path is relative")
}

/// Parses the only accepted name of a disk source, `\\.\PhysicalDrive<N>`
/// (the word is case-insensitive, `N` is decimal without sign or padding),
/// and returns `N`.
pub(crate) fn parse_physical_drive(path: &str) -> Option<u32> {
    const PREFIX: &str = r"\\.\PhysicalDrive";
    let head = path.get(..PREFIX.len())?;
    if !head.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let digits = &path[PREFIX.len()..];
    let plain = !digits.is_empty()
        && digits.len() <= 4
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'));
    if !plain {
        return None;
    }
    digits.parse().ok()
}

/// The canonical spelling of a disk path.
pub(crate) fn physical_drive_path(number: u32) -> String {
    format!(r"\\.\PhysicalDrive{number}")
}

/// Length of `\\?\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
const VOLUME_DEVICE_UNITS: usize = 48;

/// If `path` begins with a volume GUID name, `\\?\Volume{GUID}`, returns that
/// prefix: the name of the volume *device* (with a backslash appended it
/// would be the volume's root directory).
pub(crate) fn volume_device_prefix(path: &[u16]) -> Option<&[u16]> {
    const PREFIX: &str = r"\\?\Volume{";
    let device = path.get(..VOLUME_DEVICE_UNITS)?;
    if !eq_ignore_ascii_case(&device[..PREFIX.len()], PREFIX)
        || device[VOLUME_DEVICE_UNITS - 1] != b'}' as u16
    {
        return None;
    }
    let guid = &device[PREFIX.len()..VOLUME_DEVICE_UNITS - 1];
    let well_formed = guid.iter().enumerate().all(|(i, &u)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            u == b'-' as u16
        } else {
            u8::try_from(u).is_ok_and(|b| b.is_ascii_hexdigit())
        }
    });
    well_formed.then_some(device)
}

/// True when `path` is exactly the name of a volume device,
/// `\\?\Volume{GUID}`, without anything behind the closing brace.
pub(crate) fn is_volume_guid_device(path: &[u16]) -> bool {
    path.len() == VOLUME_DEVICE_UNITS && volume_device_prefix(path).is_some()
}

/// True when a UNC server name can only mean this machine: `localhost`, a
/// loopback or unspecified IP address (dotted IPv4, bracketed IPv6 or the
/// `ipv6-literal.net` transcription Windows uses in UNC paths), or one of
/// `own_names` (the computer's NetBIOS and DNS names), compared without
/// regard to ASCII case and to a trailing dot.
pub(crate) fn is_local_server(server: &str, own_names: &[String]) -> bool {
    let name = server.strip_suffix('.').unwrap_or(server);
    if name.is_empty() || name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = name.parse::<Ipv4Addr>() {
        return v4.is_loopback() || v4.is_unspecified();
    }
    let v6_text = if let Some(inner) = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
        Some(inner.to_string())
    } else {
        let lower = name.to_ascii_lowercase();
        // `--1.ipv6-literal.net` is `::1`; a zone index follows an `s`.
        lower
            .strip_suffix(".ipv6-literal.net")
            .map(|lit| lit.split('s').next().unwrap_or("").replace('-', ":"))
    };
    if let Some(text) = v6_text {
        // A literal that cannot be parsed is not known to be remote: stay on the safe side.
        return match text.parse::<Ipv6Addr>() {
            Ok(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6
                        .to_ipv4_mapped()
                        .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
            }
            Err(_) => true,
        };
    }
    own_names.iter().any(|own| {
        let own = own.strip_suffix('.').unwrap_or(own);
        !own.is_empty() && own.eq_ignore_ascii_case(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn kind(path: &str) -> WinPath {
        classify(&units(path))
    }

    fn invalid(path: &str) -> bool {
        matches!(kind(path), WinPath::Invalid(_))
    }

    #[test]
    fn accepts_absolute_drive_and_unc_paths() {
        assert_eq!(kind(r"C:\"), WinPath::Drive);
        assert_eq!(kind(r"d:\Rescued\Photos"), WinPath::Drive);
        assert_eq!(kind("E:/with/forward/slashes"), WinPath::Drive);
        assert_eq!(
            kind(r"\\nas\share"),
            WinPath::Unc {
                server: "nas".into()
            }
        );
        assert_eq!(
            kind(r"\\nas\share\dir\file.img"),
            WinPath::Unc {
                server: "nas".into()
            }
        );
        assert_eq!(
            kind("//192.168.1.20/backup/x"),
            WinPath::Unc {
                server: "192.168.1.20".into()
            }
        );
        assert_eq!(kind(r"\\?\C:\Rescued"), WinPath::VerbatimDrive);
        assert_eq!(kind(r"\\?\C:\"), WinPath::VerbatimDrive);
        assert_eq!(
            kind(r"\\?\UNC\nas\share\dir"),
            WinPath::VerbatimUnc {
                server: "nas".into()
            }
        );
        assert_eq!(
            kind(r"\\?\unc\NAS\share"),
            WinPath::VerbatimUnc {
                server: "NAS".into()
            }
        );
        assert!(kind(r"\\?\C:\x").is_verbatim());
        assert!(!kind(r"C:\x").is_verbatim());
        assert_eq!(kind(r"\\nas\share").server(), Some("nas"));
        assert_eq!(kind(r"C:\x").server(), None);
    }

    #[test]
    fn rejects_relative_device_and_incomplete_paths() {
        for path in [
            "",
            "file.img",
            r"dir\file",
            r".\x",
            r"..\x",
            "C:",
            "C:file",
            r"C:dir\file",
            r"\Windows",
            "/home/user",
            r"\\.\PhysicalDrive1",
            r"\\.\C:",
            r"\\.\C:\dir",
            r"//./C:/dir",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\dir",
            r"\\?\GLOBALROOT\Device\Harddisk0\DR0",
            r"\\?\C:",
            r"\\?\PhysicalDrive0",
            r"//?/C:/dir",
            r"\\?/C:\dir",
            r"\\?\UNC\server",
            r"\\?\UNC\\share",
            r"\\server",
            r"\\server\",
            r"\\\share\x",
            r"\??\C:\dir",
            "C:\\dir\0hidden",
            "1:\\x",
            "é:\\x",
        ] {
            assert!(
                invalid(path),
                "{path:?} must be rejected, got {:?}",
                kind(path)
            );
        }
    }

    #[test]
    fn parses_only_exact_physical_drive_names() {
        assert_eq!(parse_physical_drive(r"\\.\PhysicalDrive0"), Some(0));
        assert_eq!(parse_physical_drive(r"\\.\PhysicalDrive12"), Some(12));
        assert_eq!(parse_physical_drive(r"\\.\PHYSICALDRIVE7"), Some(7));
        assert_eq!(parse_physical_drive(r"\\.\physicaldrive9999"), Some(9999));
        for bad in [
            r"\\.\PhysicalDrive",
            r"\\.\PhysicalDrive01",
            r"\\.\PhysicalDrive+1",
            r"\\.\PhysicalDrive-1",
            r"\\.\PhysicalDrive1 ",
            r"\\.\PhysicalDrive1\",
            r"\\.\PhysicalDrive10000",
            r"\\.\PhysicalDrive1x",
            r"\\.\PhysicalDrive١",
            r"\\?\PhysicalDrive1",
            r"//./PhysicalDrive1",
            r"\\.\E:",
            r"\\.\Harddisk1Partition1",
            r"\\.\CdRom0",
            r"PhysicalDrive1",
            r"C:\PhysicalDrive1",
            "",
        ] {
            assert_eq!(parse_physical_drive(bad), None, "{bad:?}");
        }
        assert_eq!(physical_drive_path(3), r"\\.\PhysicalDrive3");
    }

    #[test]
    fn volume_device_names_are_recognised_exactly() {
        let device = r"\\?\Volume{01234567-89ab-CDEF-0123-456789abcdef}";
        assert!(is_volume_guid_device(&units(device)));
        assert_eq!(
            volume_device_prefix(&units(device)),
            Some(&units(device)[..])
        );
        // With a path behind it, the prefix is the device.
        let with_path = format!(r"{device}\Users\me");
        assert!(!is_volume_guid_device(&units(&with_path)));
        assert_eq!(
            volume_device_prefix(&units(&with_path)),
            Some(&units(device)[..])
        );
        assert_eq!(
            volume_device_prefix(&units(&format!(r"{device}\"))),
            Some(&units(device)[..])
        );
        assert!(is_volume_guid_device(&units(
            r"\\?\volume{00000000-0000-0000-0000-00000000005a}"
        )));
        for not_a_volume in [
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcde}",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdefg}",
            r"\\?\Volume{0123456789ab-cdef-0123-456789abcdef-}",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdeg}",
            r"\\?\Volume(01234567-89ab-cdef-0123-456789abcdef)",
            r"\\.\Volume{01234567-89ab-cdef-0123-456789abcdef}",
            r"\\?\Volumes{01234567-89ab-cdef-0123-456789abcdef}",
            r"\\?\C:",
            r"\\.\PhysicalDrive0",
            r"C:\Volume{01234567-89ab-cdef-0123-456789abcdef}",
            "",
        ] {
            assert!(
                !is_volume_guid_device(&units(not_a_volume)),
                "{not_a_volume}"
            );
            assert_eq!(
                volume_device_prefix(&units(not_a_volume)),
                None,
                "{not_a_volume}"
            );
        }
    }

    #[test]
    fn recognises_names_of_this_machine() {
        let own = vec![
            "WORKSTATION".to_string(),
            "workstation.corp.example".to_string(),
        ];
        for local in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "[::1]",
            "--1.ipv6-literal.net",
            "--1.IPV6-LITERAL.NET",
            "0-0-0-0-0-0-0-1.ipv6-literal.net",
            "--ffff-127.0.0.1.ipv6-literal.net",
            "zz--1.ipv6-literal.net",
            "workstation",
            "Workstation.Corp.Example",
            "workstation.corp.example.",
            "",
        ] {
            assert!(is_local_server(local, &own), "{local:?} is this machine");
        }
        for remote in [
            "nas",
            "192.168.1.20",
            "10.0.0.1",
            "128.0.0.1",
            "workstation2",
            "localhost.example",
            "2001-db8--1.ipv6-literal.net",
            "[2001:db8::1]",
            "fe80--1s4.ipv6-literal.net",
        ] {
            assert!(
                !is_local_server(remote, &own),
                "{remote:?} is another machine"
            );
        }
        assert!(!is_local_server("workstation", &[]));
    }
}
