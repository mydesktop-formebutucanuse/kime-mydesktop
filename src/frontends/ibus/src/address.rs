//! The IBus address file.
//!
//! IBus clients (the GTK/Qt `ibus` input method modules) do not look for the
//! daemon on the session bus. They read `IBUS_ADDRESS` and `IBUS_DAEMON_PID`
//! from `$XDG_CONFIG_HOME/ibus/bus/<machine-id>-<host>-<display>` and connect
//! to that address, ignoring the file unless the PID is alive. The path and
//! file format below mirror `ibus_get_socket_path()`, `ibus_get_address()`
//! and `ibus_write_address()` in ibus' `src/ibusshare.c`.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const MACHINE_ID_FILES: [&str; 2] = ["/var/lib/dbus/machine-id", "/etc/machine-id"];

/// `ibus_get_local_machine_id()`
pub fn machine_id() -> String {
    MACHINE_ID_FILES
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .map(|id| id.trim().to_owned())
        .unwrap_or_else(|| "machine-id".to_owned())
}

/// The file name part of `ibus_get_socket_path()`.
///
/// `WAYLAND_DISPLAY` wins over `DISPLAY` exactly like in ibus, so a client
/// and kime started from the same environment always agree on the file.
pub fn socket_file_name(
    machine_id: &str,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> String {
    let (hostname, display_number) = match (wayland_display, display) {
        (Some(wayland), _) => ("unix", wayland),
        (None, Some(display)) => {
            let (host, rest) = display.split_once(':').unwrap_or((display, "0"));
            // Drop the screen number: `:0.1` and `:0` share one daemon.
            let number = rest.split('.').next().unwrap_or(rest);
            (if host.is_empty() { "unix" } else { host }, number)
        }
        (None, None) => ("unix", "0"),
    };

    format!("{machine_id}-{hostname}-{display_number}")
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `g_get_user_config_dir()`
fn user_config_dir() -> PathBuf {
    non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty_env("HOME").map(|home| Path::new(&home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

/// `ibus_get_socket_path()`
pub fn address_file_path() -> PathBuf {
    if let Some(path) = non_empty_env("IBUS_ADDRESS_FILE") {
        return PathBuf::from(path);
    }

    let name = socket_file_name(
        &machine_id(),
        non_empty_env("WAYLAND_DISPLAY").as_deref(),
        non_empty_env("DISPLAY").as_deref(),
    );

    user_config_dir().join("ibus").join("bus").join(name)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AddressFile {
    pub address: Option<String>,
    pub pid: Option<i32>,
}

/// `ibus_get_address()`, minus the liveness check.
pub fn parse(contents: &str) -> AddressFile {
    let mut file = AddressFile::default();

    for line in contents.lines() {
        if line.starts_with('#') {
            continue;
        }

        if let Some(address) = line.strip_prefix("IBUS_ADDRESS=") {
            file.address = Some(address.to_owned());
        } else if let Some(pid) = line.strip_prefix("IBUS_DAEMON_PID=") {
            file.pid = pid.trim().parse().ok();
        }
    }

    file
}

pub fn render(address: &str, pid: u32) -> String {
    format!(
        "# This file is created by kime-ibus, please do not modify it.\n\
         # This file allows processes on the machine to find the\n\
         # ibus session bus with the below address.\n\
         # If the IBUS_ADDRESS environment variable is set, it will\n\
         # be used rather than this file.\n\
         IBUS_ADDRESS={address}\n\
         IBUS_DAEMON_PID={pid}\n"
    )
}

fn pid_alive(pid: i32) -> bool {
    // Same test as ibus: signal 0 only checks that the process exists.
    // EPERM still means it exists, but then it is not ours to talk to.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The PID of a live daemon other than us that currently owns `path`.
pub fn live_foreign_daemon(path: &Path) -> Option<i32> {
    let pid = parse(&fs::read_to_string(path).ok()?).pid?;
    (pid != std::process::id() as i32 && pid_alive(pid)).then_some(pid)
}

pub fn write(path: &Path, address: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }

    // Write to a sibling and rename it in place: clients watch this file and
    // reconnect as soon as it changes, so they must never see it half-written.
    let tmp = path.with_extension(format!("kime-{}", std::process::id()));
    fs::write(&tmp, render(address, std::process::id()))?;
    fs::rename(&tmp, path)
}

/// Remove `path` if it still points at this process.
pub fn remove_if_ours(path: &Path) {
    let ours = fs::read_to_string(path)
        .map(|contents| parse(&contents).pid == Some(std::process::id() as i32))
        .unwrap_or(false);

    if ours {
        if let Err(err) = fs::remove_file(path) {
            log::warn!("Can't remove {}: {}", path.display(), err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_display_names() {
        let name = |display| socket_file_name("id", None, Some(display));

        assert_eq!(name(":0"), "id-unix-0");
        assert_eq!(name(":1.0"), "id-unix-1");
        assert_eq!(name("localhost:10.2"), "id-localhost-10");
        assert_eq!(name("unix:3"), "id-unix-3");
    }

    #[test]
    fn display_fallbacks() {
        assert_eq!(socket_file_name("id", None, None), "id-unix-0");
        assert_eq!(
            socket_file_name("id", Some("wayland-0"), Some(":0")),
            "id-unix-wayland-0"
        );
    }

    #[test]
    fn rendered_file_round_trips() {
        let contents = render("unix:path=/run/user/1000/bus,guid=abc", 4242);

        assert_eq!(
            parse(&contents),
            AddressFile {
                address: Some("unix:path=/run/user/1000/bus,guid=abc".into()),
                pid: Some(4242),
            }
        );
    }

    #[test]
    fn parses_ibus_daemon_file() {
        let contents = "# This file is created by ibus-daemon, please do not modify it.\n\
                        IBUS_ADDRESS=unix:abstract=/home/u/.cache/ibus/dbus-x,guid=y\n\
                        IBUS_DAEMON_PID=17\n";

        assert_eq!(parse(contents).pid, Some(17));
    }

    #[test]
    fn write_and_remove() {
        let dir = std::env::temp_dir().join(format!("kime-ibus-test-{}", std::process::id()));
        let path = dir.join("ibus").join("bus").join("id-unix-0");

        write(&path, "unix:path=/nonexistent").unwrap();
        assert_eq!(
            parse(&fs::read_to_string(&path).unwrap()).pid,
            Some(std::process::id() as i32)
        );
        // Our own file never counts as a foreign daemon.
        assert_eq!(live_foreign_daemon(&path), None);

        remove_if_ours(&path);
        assert!(!path.exists());

        fs::remove_dir_all(dir).ok();
    }
}
