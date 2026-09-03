//! Thin wrapper over the `container` CLI.
//!
//! Everything the tool does to the runtime goes through here, so the places
//! that parse `container` output are all in one file.

use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// Lifecycle state of a container, as `container` reports it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Running,
    Stopped,
    Absent,
}

/// Absolute path to the `container` binary.
///
/// Needed for anything run under `sudo`: sudo resets `PATH` to a secure default
/// that excludes Homebrew's `/opt/homebrew/bin`, so `sudo container ...` fails
/// with "command not found" even though it works fine unprivileged.
pub fn binary_path() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("container"))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Is the `container` CLI on PATH and runnable?
pub fn installed() -> bool {
    Command::new("container")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Short version string for `doctor`, e.g. "container 1.3.0".
pub fn version() -> Option<String> {
    let out = Command::new("container").arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(format!("container {}", short_version(s.lines().next()?)))
}

/// `container --version` prints a whole sentence
/// ("container CLI version 1.3.0 (build: release, commit: ...)"). Pull the
/// version number out of it, falling back to the raw line.
fn short_version(line: &str) -> String {
    line.split_whitespace()
        .find(|t| {
            let core = t.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            core.split('.').count() >= 2 && core.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(|t| {
            t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .to_string()
        })
        .unwrap_or_else(|| line.to_string())
}

/// Are the container services up? `system status` exits 0 when healthy.
pub fn system_running() -> bool {
    quiet_ok(&["system", "status"])
}

/// Run and stream output, but do not fail the process on non-zero.
pub fn run_streaming(args: &[String]) -> Result<bool> {
    let status = Command::new("container")
        .args(args)
        .status()
        .context("failed to execute `container`")?;
    Ok(status.success())
}

/// Capture stdout, returning None on failure.
pub fn capture(args: &[&str]) -> Option<String> {
    let out = Command::new("container").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run silently, reporting only success.
pub fn quiet_ok(args: &[&str]) -> bool {
    Command::new("container")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Does this container exist at all, running or stopped?
pub fn exists(name: &str) -> bool {
    quiet_ok(&["inspect", name])
}

/// Running, stopped, or absent.
///
/// A node whose entrypoint gave up leaves a *stopped* container, not an
/// absent one, so callers must treat those differently.
pub fn state(name: &str) -> State {
    if !exists(name) {
        return State::Absent;
    }
    // `container ls` lists running containers only; first column is the name.
    let running = capture(&["ls"])
        .map(|out| first_column_contains(&out, name))
        .unwrap_or(false);
    if running {
        State::Running
    } else {
        State::Stopped
    }
}

/// Is `domain` registered with the host resolver?
///
/// Ask `container` rather than looking for a file: it names the resolver file
/// `/etc/resolver/containerization.<domain>`, not `/etc/resolver/<domain>`, and
/// that is an implementation detail worth not depending on.
pub fn dns_domain_registered(domain: &str) -> bool {
    match capture(&["system", "dns", "list"]) {
        Some(out) => first_column_contains(&out, domain),
        // Fall back to the file when the CLI is unavailable.
        None => {
            std::path::Path::new(&format!("/etc/resolver/containerization.{domain}")).exists()
                || std::path::Path::new(&format!("/etc/resolver/{domain}")).exists()
        }
    }
}

/// Does this named volume exist?
pub fn volume_exists(name: &str) -> bool {
    capture(&["volume", "list"])
        .map(|out| first_column_contains(&out, name))
        .unwrap_or(false)
}

pub fn image_exists(reference: &str) -> bool {
    // Ask the runtime rather than parsing `image list` columns, whose layout
    // is not contractual.
    quiet_ok(&["image", "inspect", reference])
}

/// Exec a command inside a container, capturing stdout. `env` pairs are passed
/// with -e.
pub fn exec_capture(name: &str, env: &[(&str, &str)], cmd: &[&str]) -> Option<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(name.to_string());
    args.extend(cmd.iter().map(|s| s.to_string()));

    let out = Command::new("container").args(&args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a command in a container, caring only whether it succeeded.
pub fn exec_ok(name: &str, env: &[(&str, &str)], cmd: &[&str]) -> bool {
    exec_capture(name, env, cmd).is_some()
}

/// Exec interactively (psql, bash), replacing our stdio.
///
/// `--tty` is only passed when we actually have one: asking for a TTY from a
/// pipe or a script makes `container exec` fail outright, which would stop
/// `cider pgd cli nodes list` working anywhere but a terminal.
pub fn exec_interactive(name: &str, env: &[(&str, &str)], cmd: &[String]) -> Result<()> {
    use std::io::IsTerminal;
    let mut args: Vec<String> = vec!["exec".into(), "--interactive".into()];
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        args.push("--tty".into());
    }
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(name.to_string());
    args.extend(cmd.iter().cloned());

    let status = Command::new("container")
        .args(&args)
        .status()
        .context("failed to execute `container exec`")?;
    // psql exits non-zero for ordinary things (\q after an error); don't treat
    // that as a tool failure.
    let _ = status;
    Ok(())
}

/// Stream a container's logs to our stdout, passing `extra` through.
pub fn logs(name: &str, extra: &[String]) -> Result<()> {
    let mut args: Vec<String> = vec!["logs".into()];
    args.extend(extra.iter().cloned());
    args.push(name.to_string());
    run_streaming(&args)?;
    Ok(())
}

/// The last `lines` of a container's logs, for error reporting.
pub fn logs_tail(name: &str, lines: usize) -> String {
    capture(&["logs", name])
        .map(|out| {
            let all: Vec<&str> = out.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        })
        .unwrap_or_else(|| "(no logs available)".to_string())
}

/// True if any line's first whitespace-delimited column equals `needle`.
fn first_column_contains(output: &str, needle: &str) -> bool {
    output
        .lines()
        .skip(1) // header
        .filter_map(|l| l.split_whitespace().next())
        .any(|c| c == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_first_column() {
        let out = "ID             IMAGE            STATE\n\
                   host-1         cider-press        running\n\
                   host-10        cider-press        running\n";
        assert!(first_column_contains(out, "host-1"));
        assert!(first_column_contains(out, "host-10"));
        assert!(!first_column_contains(out, "host-2"));
        // The header must never match.
        assert!(!first_column_contains(out, "ID"));
        // A substring of a real entry must not match.
        assert!(!first_column_contains(out, "host"));
    }

    #[test]
    fn extracts_the_version_number() {
        // The real 1.3.0 output.
        assert_eq!(
            short_version("container CLI version 1.3.0 (build: release, commit: unspeci)"),
            "1.3.0"
        );
        assert_eq!(short_version("container 2.0"), "2.0");
        // Unrecognised shapes fall back to the whole line rather than lying.
        assert_eq!(short_version("no version here"), "no version here");
    }

    #[test]
    fn empty_output_matches_nothing() {
        assert!(!first_column_contains("", "host-1"));
        assert!(!first_column_contains("HEADER\n", "host-1"));
    }
}
