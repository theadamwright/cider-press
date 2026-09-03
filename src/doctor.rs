//! Preflight checks, in the order they matter.

use crate::bootstrap;
use crate::config::Config;
use crate::container;
use crate::term;
use anyhow::{Result, bail};
use std::process::Command;

/// Run every preflight check and summarise.
///
/// Ordered so the first ✘ is the root cause: silicon, then macOS, then the
/// runtime, then DNS, then credentials, then the image. A check that cannot run
/// because an earlier one failed is shown as skipped rather than omitted, so it
/// is clear it is covered rather than missing.
///
/// Blocking problems fail the command; warnings do not, because they only
/// affect some commands (a missing token matters for `build` alone).
pub fn run(cfg: &Config) -> Result<()> {
    term::banner();
    let mut blocking = 0;
    let mut warnings = 0;

    term::info("host");
    let arch = uname_m();
    if arch == "arm64" {
        term::ok(&format!("Apple silicon ({arch})"));
    } else {
        term::bad(&format!("{arch} — Apple container requires Apple silicon"));
        blocking += 1;
    }

    let osver = sw_vers();
    let major: u32 = osver
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if major >= 26 {
        term::ok(&format!("macOS {osver}"));
    } else {
        term::bad(&format!(
            "macOS {osver} — container-to-container networking needs macOS 26+"
        ));
        blocking += 1;
    }

    term::info("container runtime");
    if container::installed() {
        term::ok(&container::version().unwrap_or_else(|| "container installed".into()));
        if container::system_running() {
            term::ok("container system is running");
        } else {
            term::bad("container system is not running — run: container system start");
            blocking += 1;
        }
    } else {
        term::bad("container not installed");
        term::note("Install the signed package from:");
        term::note("  https://github.com/apple/container/releases/latest");
        // Still name the service check, so it is clear it is covered rather
        // than missing — it just cannot run without the binary.
        term::skip("container system running — can't check until container is installed");
        blocking += 1;
    }

    term::info("DNS (how PGD nodes find each other)");
    let path = Config::container_config_path();

    // An unreadable config is not the same as an unconfigured one, and saying
    // "not configured" would send you to a fix that cannot work.
    match path.as_deref().and_then(bootstrap::config_access_problem) {
        Some(problem) => {
            term::bad("cannot read the container config");
            for line in problem.lines() {
                term::note(line.trim_end());
            }
            term::note("`container` reads its config from here too, so it is affected as well.");
            term::skip("[dns] domain — can't check until that is fixed");
            blocking += 1;
        }
        None => match path
            .as_deref()
            .and_then(bootstrap::configured_domain)
            .as_deref()
        {
            Some(d) if d == cfg.domain => {
                term::ok(&format!(
                    "[dns] domain = \"{}\" in ~/.config/container/config.toml",
                    cfg.domain
                ));
            }
            Some(d) => {
                term::bad(&format!(
                    "[dns] domain is \"{d}\", expected \"{}\"",
                    cfg.domain
                ));
                term::note("Either run: cider bootstrap");
                term::note(&format!(
                    "or set CIDER_DOMAIN={d} to use the domain you already have."
                ));
                blocking += 1;
            }
            None => {
                term::bad("no [dns] domain configured — run: cider bootstrap");
                blocking += 1;
            }
        },
    }

    if cfg.resolver_installed() {
        term::ok(&format!(
            "macOS resolves *.{} through container's DNS",
            cfg.domain
        ));
    } else {
        term::warn(&format!("macOS does not resolve *.{} yet", cfg.domain));
        term::note("Needed only to reach nodes by name from macOS itself.");
        term::note("Published 127.0.0.1 ports work regardless. Fix: cider bootstrap");
        warnings += 1;
    }

    term::info("EDB subscription");
    match &cfg.token {
        Some(t) => term::ok(&format!(
            "EDB_SUBSCRIPTION_TOKEN is set ({} chars)",
            t.len()
        )),
        None => {
            term::warn("EDB_SUBSCRIPTION_TOKEN is not set — required for 'cider pgd build' only");
            term::note("export EDB_SUBSCRIPTION_TOKEN=\"...\"   (or put it in ./.env)");
            term::note("Get one at https://www.enterprisedb.com/repos-downloads");
            warnings += 1;
        }
    }

    term::info("image");
    if container::installed() && container::image_exists(&cfg.image) {
        term::ok(&format!("{} is built", cfg.image));
    } else {
        term::warn(&format!(
            "{} not built yet — run: cider pgd build",
            cfg.image
        ));
        warnings += 1;
    }

    println!();
    if blocking > 0 {
        bail!("doctor found blocking problems (see ✘ above)");
    }
    if warnings > 0 {
        println!("{}", term::yellow("Ready, with warnings above."));
    } else {
        println!(
            "{}",
            term::green("All good. Next: cider pgd build && cider pgd up")
        );
    }
    Ok(())
}

fn uname_m() -> String {
    Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn sw_vers() -> String {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
