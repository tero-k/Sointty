//! Browsable locations and native network-drive access.
//!
//! All OS code lives here; the TUI talks to these callbacks only.
//! Windows: `GetLogicalDrives` enumerates assigned letter roots,
//! `WNetGetConnectionW` labels mapped network drives, and mapping goes
//! through `WNetAddConnection2W` with `CONNECT_INTERACTIVE` so the OS
//! credential prompt handles authentication — Sointty never sees or stores
//! credentials. Unix: `/`, home and existing mount roots are listed; mount
//! actions shell out to the OS tool (`gio`/`open`) with the URI as a single
//! argument, never through a shell.

use std::path::PathBuf;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;

use sointty_tui::Location;
/// Platform-specific callbacks used by the TUI.
pub fn map_drive_hook() -> Option<sointty_tui::MapDrive> {
    #[cfg(windows)]
    { Some(Box::new(map_drive)) }
    #[cfg(not(windows))]
    { None }
}

pub fn disconnect_drive_hook() -> Option<sointty_tui::DisconnectDrive> {
    #[cfg(windows)]
    { Some(Box::new(disconnect_drive)) }
    #[cfg(not(windows))]
    { None }
}

pub fn mount_share_hook() -> Option<sointty_tui::MountShare> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    { Some(Box::new(mount_share)) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { None }
}


/// Human-readable mount roots that exist right now.
pub fn list_locations() -> Result<Vec<Location>, String> {
    #[cfg(windows)]
    {
        windows_locations()
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux_locations())
    }
    #[cfg(target_os = "macos")]
    {
        Ok(macos_locations())
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err("locations are not available on this OS".to_owned())
    }
}


/// Validate `\\server\share` (two leading separators, exactly one separator
/// between host and share, no credentials). Returns `(server, share)`.
pub fn parse_unc(input: &str) -> Result<(String, String), String> {
    let rest = input
        .strip_prefix("\\\\")
        .ok_or_else(|| "path must start with \\\\ (e.g. \\\\server\\share)".to_owned())?;
    if rest.contains('@') || rest.contains(';') || rest.contains(':') {
        return Err("credentials in the path are not supported".to_owned());
    }
    let (server, share) = rest
        .split_once('\\')
        .ok_or_else(|| "expected \\\\server\\share".to_owned())?;
    let server = server.trim();
    let share = share.trim();
    if server.is_empty() || share.is_empty() {
        return Err("server and share must both be non-empty".to_owned());
    }
    if share.contains('\\') {
        return Err("only top-level shares are supported (\\\\server\\share)".to_owned());
    }
    Ok((server.to_owned(), share.to_owned()))
}

/// Validate a drive letter for mapping: A-Z and currently unassigned.
pub fn validate_free_letter(letter: char) -> Result<char, String> {
    let letter = letter.to_ascii_uppercase();
    if !letter.is_ascii_uppercase() {
        return Err("drive letter must be A-Z".to_owned());
    }
    #[cfg(windows)]
    if assigned_letters().contains(&letter) {
        return Err(format!("{letter}: is already in use"));
    }
    Ok(letter)
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Letters A-Z with a drive assigned, from the `GetLogicalDrives` bitmask.
#[cfg(windows)]
pub fn letters_from_mask(mask: u32) -> Vec<char> {
    (0..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| (b'A' + bit as u8) as char)
        .collect()
}

#[cfg(windows)]
fn assigned_letters() -> Vec<char> {
    // SAFETY: GetLogicalDrives has no inputs and no failure mode beyond 0.
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    letters_from_mask(mask)
}

#[cfg(windows)]
fn windows_locations() -> Result<Vec<Location>, String> {
    let mut locations = Vec::new();
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        locations.push(Location {
            label: "Home".to_owned(),
            path: home,
            mapped_network: false,
        });
    }
    for letter in assigned_letters() {
        let root = PathBuf::from(format!("{letter}:\\"));
        let remote = mapped_remote_name(letter);
        locations.push(Location {
            label: match &remote {
                Some(unc) => format!("{letter}:  ({unc})"),
                None => format!("{letter}:\\"),
            },
            path: root,
            mapped_network: remote.is_some(),
        });
    }
    Ok(locations)
}

/// `Some(\\server\share)` when `letter` is a mapped network drive.
#[cfg(windows)]
fn mapped_remote_name(letter: char) -> Option<String> {
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::core::PWSTR;
    let name = wide(&format!("{letter}:"));
    let mut buffer = vec![0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: `name` is a valid null-terminated string; `buffer` is writable
    // for `len` wide chars and `len` is updated to the required size.
    let result = unsafe {
        WNetGetConnectionW(
            windows::core::PCWSTR(name.as_ptr()),
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut len,
        )
    };
    // NO_ERROR (0) means success; anything else (not connected, error) maps
    // to None.
    if result.0 != 0 {
        return None;
    }
    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..end]);
    if text.is_empty() { None } else { Some(text) }
}

/// Map `letter` to `\\server\share` via WNet with the OS credential prompt.
#[cfg(windows)]
pub fn map_drive(letter: char, unc: &str) -> Result<PathBuf, String> {
    use windows::Win32::NetworkManagement::WNet::{
        CONNECT_COMMANDLINE, CONNECT_INTERACTIVE, CONNECT_UPDATE_PROFILE, NETRESOURCEW,
        RESOURCETYPE_DISK, WNetAddConnection2W,
    };
    let letter = validate_free_letter(letter)?;
    let (server, share) = parse_unc(unc)?;
    let local = wide(&format!("{letter}:"));
    let remote = wide(&format!("\\\\{server}\\{share}"));
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpLocalName: windows::core::PWSTR(local.as_ptr() as *mut u16),
        lpRemoteName: windows::core::PWSTR(remote.as_ptr() as *mut u16),
        ..Default::default()
    };
    // SAFETY: all pointers reference live null-terminated buffers; null
    // user/password plus CONNECT_INTERACTIVE lets the OS prompt.
    let result = unsafe {
        WNetAddConnection2W(
            &resource,
            None,
            None,
            CONNECT_UPDATE_PROFILE | CONNECT_INTERACTIVE | CONNECT_COMMANDLINE,
        )
    };
    if result.0 != 0 {
        return Err(format!("mapping {letter}: to \\\\{server}\\{share} failed: OS error {}", result.0));
    }
    Ok(PathBuf::from(format!("{letter}:\\")))
}

/// Disconnect a mapped network drive. Open files prevent the disconnect
/// (`force` is never used).
#[cfg(windows)]
pub fn disconnect_drive(letter: char) -> Result<(), String> {
    use windows::Win32::NetworkManagement::WNet::{CONNECT_UPDATE_PROFILE, WNetCancelConnection2W};
    let letter = letter.to_ascii_uppercase();
    if mapped_remote_name(letter).is_none() {
        return Err(format!("{letter}: is not a mapped network drive"));
    }
    let name = wide(&format!("{letter}:"));
    // SAFETY: `name` is a valid null-terminated string; force = false.
    let result = unsafe { WNetCancelConnection2W(windows::core::PCWSTR(name.as_ptr()), CONNECT_UPDATE_PROFILE, false) };
    if result.0 != 0 {
        return Err(format!("disconnecting {letter}: failed: OS error {} (files may be open)", result.0));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_locations() -> Vec<Location> {
    let mut locations = vec![Location {
        label: "/".to_owned(),
        path: PathBuf::from("/"),
        mapped_network: false,
    }];
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        locations.push(Location {
            label: "Home".to_owned(),
            path: home,
            mapped_network: false,
        });
    }
    let mut roots: Vec<PathBuf> = ["/mnt", "/media", "/run/media"]
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .collect();
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let gvfs = Path::new(&runtime).join("gvfs");
        if gvfs.is_dir() {
            roots.push(gvfs);
        }
    }
    for root in roots {
        locations.push(Location {
            label: root.display().to_string(),
            path: root,
            mapped_network: false,
        });
    }
    locations
}

#[cfg(target_os = "macos")]
fn macos_locations() -> Vec<Location> {
    let mut locations = vec![Location {
        label: "/".to_owned(),
        path: PathBuf::from("/"),
        mapped_network: false,
    }];
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        locations.push(Location {
            label: "Home".to_owned(),
            path: home,
            mapped_network: false,
        });
    }
    if Path::new("/Volumes").is_dir() {
        locations.push(Location {
            label: "/Volumes".to_owned(),
            path: PathBuf::from("/Volumes"),
            mapped_network: false,
        });
    }
    locations
}

/// Mount an SMB share through the OS (GVfs on Linux, Finder on macOS).
/// Returns once the OS tool accepted the mount; the share appears as a
/// normal filesystem path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn mount_share(input: &str) -> Result<(), String> {
    let trimmed = input.trim();
    let uri = if trimmed.starts_with("smb://") {
        trimmed.to_owned()
    } else if let Some(rest) = trimmed.strip_prefix("\\\\") {
        format!("smb://{}", rest.replace('\\', "/"))
    } else {
        format!("smb://{trimmed}")
    };
    let host = uri.trim_start_matches("smb://");
    if host.is_empty() || host.starts_with('/') {
        return Err("expected smb://server/share".to_owned());
    }
    #[cfg(target_os = "linux")]
    let program = "gio";
    #[cfg(target_os = "macos")]
    let program = "open";
    if !command_exists(program) {
        return Err(format!("`{program}` is not installed; mount the share with the OS first"));
    }
    #[cfg(target_os = "linux")]
    let args: Vec<String> = vec!["mount".to_owned(), uri];
    #[cfg(target_os = "macos")]
    let args: Vec<String> = vec![uri];
    // The URI is a single argument; no shell is involved.
    let status = std::process::Command::new(program)
        .args(&args)
        .status()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_exists(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_parsing_accepts_server_share() {
        assert_eq!(
            parse_unc("\\\\nas\\music").unwrap(),
            ("nas".to_owned(), "music".to_owned())
        );
    }

    #[test]
    fn unc_parsing_rejects_bad_forms() {
        assert!(parse_unc("").is_err());
        assert!(parse_unc("\\\\").is_err());
        assert!(parse_unc("\\\\nas").is_err());
        assert!(parse_unc("\\\\\\music").is_err());
        assert!(parse_unc("\\\\nas\\").is_err());
        assert!(parse_unc("\\\\user@nas\\music").is_err());
        assert!(parse_unc("\\\\nas:pass\\music").is_err());
        assert!(parse_unc("C:\\music").is_err());
    }

    #[test]
    fn letter_validation_rejects_non_letters() {
        assert!(validate_free_letter('1').is_err());
        assert!(validate_free_letter(' ').is_err());
        assert!(validate_free_letter('\\').is_err());
    }

    #[cfg(windows)]
    #[test]
    fn drive_mask_decodes_letters() {
        assert_eq!(letters_from_mask(0), Vec::<char>::new());
        assert_eq!(letters_from_mask(0b101), vec!['A', 'C']);
        assert_eq!(letters_from_mask(1 << 25), vec!['Z']);
    }

    #[cfg(windows)]
    #[test]
    fn used_letters_are_rejected_for_mapping() {
        // C: exists on every Windows host.
        assert!(validate_free_letter('c').is_err());
    }
}
