//! One-time host setup: the container DNS domain, and the macOS resolver entry.

use crate::config::Config;
use crate::term;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use toml_edit::{DocumentMut, Item, Table, value};

/// Whether the container config is actually usable by this user.
///
/// A root-owned `~/.config` (a stray `sudo` can create one) makes the file
/// unreadable *and* unwritable, which otherwise shows up as "no [dns] domain
/// configured" followed by a bare EACCES. Worth naming precisely, because
/// `container` cannot read its own config in that state either.
pub fn config_access_problem(path: &Path) -> Option<String> {
    use std::io::ErrorKind;

    // The *outermost* directory we cannot enter is the one to fix. Chowning an
    // inner directory would not help, because the traversal fails higher up.
    let mut ancestors: Vec<&Path> = path.ancestors().skip(1).collect();
    ancestors.reverse();
    let blocker = ancestors.into_iter().find(
        |dir| matches!(std::fs::read_dir(dir), Err(e) if e.kind() == ErrorKind::PermissionDenied),
    );

    let target = match blocker {
        Some(dir) => dir,
        None => match std::fs::read(path) {
            Ok(_) => return None,
            Err(e) if e.kind() == ErrorKind::NotFound => return None,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => path,
            Err(e) => return Some(format!("{} cannot be read: {e}", path.display())),
        },
    };

    let owner = std::fs::symlink_metadata(target)
        .ok()
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            format!(" (owned by uid {}, mode {:o})", m.uid(), m.mode() & 0o777)
        })
        .unwrap_or_default();

    Some(format!(
        "{}{owner} is not accessible to your user.\nFix ownership, then re-run:\n  sudo chown -R \"$(id -un):staff\" {}\n  chmod 700 {}",
        target.display(),
        target.display(),
        target.display()
    ))
}

/// Read `[dns].domain` from the container runtime config.
pub fn configured_domain(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = text.parse::<DocumentMut>().ok()?;
    doc.get("dns")?
        .as_table_like()?
        .get("domain")?
        .as_str()
        .map(|s| s.to_string())
}

/// Set `[dns].domain`, preserving every other section, comment and blank line.
/// A real TOML editor rather than a hand-rolled rewriter, because this is
/// someone's live `container` configuration.
pub fn set_domain(path: &Path, domain: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    if doc.get("dns").and_then(|i| i.as_table_like()).is_none() {
        doc["dns"] = Item::Table(Table::new());
    }
    doc["dns"]["domain"] = value(domain);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Remove `[dns].domain`, and the now-empty `[dns]` table with it. Every other
/// section is preserved. Returns false if the key was not there to begin with.
pub fn remove_domain(path: &Path) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    let Some(dns) = doc.get_mut("dns").and_then(|i| i.as_table_like_mut()) else {
        return Ok(false);
    };
    if dns.remove("domain").is_none() {
        return Ok(false);
    }
    if dns.is_empty() {
        doc.remove("dns");
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(true)
}

/// Undo what `bootstrap` did to the host: the container DNS domain and the
/// macOS resolver entry.
///
/// The DNS domain is a global `container` setting rather than something this
/// tool owns, so a domain that is not ours is reported and left alone.
pub fn remove_dns_setup(cfg: &Config, assume_yes: bool) -> Result<()> {
    let path = Config::container_config_path().context("HOME is not set")?;
    let current = configured_domain(&path);
    let resolver = format!("the macOS resolver for *.{}", cfg.domain);
    let resolver_present = cfg.resolver_installed();

    if current.as_deref() != Some(cfg.domain.as_str()) && !resolver_present {
        term::ok("no DNS setup of ours to remove");
        return Ok(());
    }

    println!();
    println!("{}", term::yellow("DNS teardown will also:"));
    match current.as_deref() {
        Some(d) if d == cfg.domain => {
            println!(
                "  remove  [dns] domain = \"{}\"  from {}",
                cfg.domain,
                path.display()
            );
            println!("  restart the container system");
        }
        Some(d) => {
            println!(
                "  {}",
                term::dim(&format!(
                    "leave [dns] domain = \"{d}\" alone — it is not ours to remove"
                ))
            );
        }
        None => {}
    }
    if resolver_present {
        println!("  remove  {resolver}  (needs sudo)");
    }
    println!();
    println!(
        "  {}",
        term::dim("Anything else on your Mac using *.{domain} names will stop resolving.")
            .replace("{domain}", &cfg.domain)
    );
    println!(
        "  {}",
        term::dim("Re-running 'cider bootstrap' sets it all up again.")
    );
    println!();

    if !assume_yes && !confirm("Remove the DNS setup too? [y/N] ")? {
        println!("Left the DNS setup in place.");
        return Ok(());
    }

    // Deliberately not collapsed into one `&&`: remove_domain() edits the
    // user's config file. A mutation does not belong inside a condition, where
    // it reads like a predicate.
    #[allow(clippy::collapsible_if)]
    if current.as_deref() == Some(cfg.domain.as_str()) {
        if remove_domain(&path)? {
            term::ok(&format!("removed [dns] domain = \"{}\"", cfg.domain));
            term::info("restarting container system");
            let _ = crate::container::quiet_ok(&["system", "stop"]);
            if crate::container::quiet_ok(&["system", "start"]) {
                term::ok("container system restarted");
            } else {
                term::warn("container system did not restart — run: container system start");
            }
        }
    }

    if resolver_present {
        term::info("removing the macOS resolver entry (sudo)");
        let bin = crate::container::binary_path()
            .context("could not locate the `container` binary on PATH")?;
        let status = Command::new("sudo")
            .arg(&bin)
            .args(["system", "dns", "delete", &cfg.domain])
            .status()
            .context("could not run sudo")?;
        if status.success() {
            term::ok(&format!("removed {resolver}"));
        } else {
            term::warn(&format!(
                "'sudo {} system dns delete {}' failed — remove {resolver} by hand",
                bin.display(),
                cfg.domain
            ));
        }
    }

    // Point at the pre-bootstrap backup rather than restoring it silently:
    // the file may have changed for unrelated reasons since.
    if let Some(backup) = newest_backup(&path) {
        println!();
        println!(
            "  {}",
            term::dim(&format!(
                "your pre-bootstrap config is still at {}",
                backup.display()
            ))
        );
    }
    Ok(())
}

/// The most recent `config.toml.cider-press.<epoch>.bak` beside the config.
fn newest_backup(config: &Path) -> Option<std::path::PathBuf> {
    let dir = config.parent()?;
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".cider-press.") && n.ends_with(".bak"))
        })
        .collect();
    found.sort();
    found.pop()
}

/// Set the host up: container DNS domain, then the macOS resolver entry.
///
/// This is the only command that needs sudo, and the only one that edits a file
/// belonging to another tool — so it checks access *before* prompting, backs the
/// file up, and verifies the change took.
pub fn run(cfg: &Config, assume_yes: bool) -> Result<()> {
    term::banner();

    if !crate::container::installed() {
        bail!(
            "container is not installed.\n  \
             Download the signed installer from \
             https://github.com/apple/container/releases/latest\n  \
             then re-run: cider bootstrap"
        );
    }

    let path = Config::container_config_path().context("HOME is not set")?;

    // Check this before touching anything: otherwise the user answers the
    // prompt and only then discovers they never had permission.
    if let Some(problem) = config_access_problem(&path) {
        bail!(
            "cannot use the container config.\n\n      {problem}\n\n      \
             `container` reads its own configuration from this path, so it is \
             affected too."
        );
    }

    let current = configured_domain(&path);
    let domain_ok = current.as_deref() == Some(cfg.domain.as_str());
    let resolver_ok = cfg.resolver_installed();

    if domain_ok && resolver_ok {
        term::ok(&format!(
            "already bootstrapped for domain \"{}\"",
            cfg.domain
        ));
        return Ok(());
    }

    println!("This will:");
    if !domain_ok {
        println!(
            "  1. set  [dns] domain = \"{}\"  in {}",
            cfg.domain,
            path.display()
        );
        println!("  2. restart the container system so it picks that up");
    }
    if !resolver_ok {
        println!(
            "  3. run 'sudo container system dns create {}', which writes",
            cfg.domain
        );
        println!(
            "     the resolver entry so macOS resolves *.{} too.",
            cfg.domain
        );
        println!(
            "     {}",
            term::dim("This is the only step that needs your password.")
        );
    }
    println!();

    if !assume_yes && !confirm("Proceed? [y/N] ")? {
        println!("Aborted.");
        return Ok(());
    }

    if !domain_ok {
        if path.exists() {
            let backup = path.with_extension(format!(
                "toml.cider-press.{}.bak",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            std::fs::copy(&path, &backup).with_context(|| {
                format!(
                    "could not back up {} to {}",
                    path.display(),
                    backup.display()
                )
            })?;
            term::info(&format!(
                "backed up existing config to {}",
                backup.display()
            ));
        }

        set_domain(&path, &cfg.domain)?;
        if configured_domain(&path).as_deref() != Some(cfg.domain.as_str()) {
            bail!("failed to set [dns] domain in {}", path.display());
        }
        term::ok(&format!("set [dns] domain = \"{}\"", cfg.domain));

        term::info("restarting container system");
        let _ = crate::container::quiet_ok(&["system", "stop"]);
        if !crate::container::quiet_ok(&["system", "start"]) {
            bail!("container system start failed");
        }
        term::ok("container system restarted");
    }

    if !resolver_ok {
        term::info("creating the macOS resolver entry (sudo)");
        // Absolute path: sudo's PATH does not include Homebrew.
        let bin = crate::container::binary_path()
            .context("could not locate the `container` binary on PATH")?;
        let status = Command::new("sudo")
            .arg(&bin)
            .args(["system", "dns", "create", &cfg.domain])
            .status()
            .context("could not run sudo")?;
        if !status.success() {
            bail!(
                "'sudo {} system dns create {}' failed",
                bin.display(),
                cfg.domain
            );
        }
        term::ok(&format!("macOS now resolves *.{}", cfg.domain));
    }

    println!();
    println!(
        "{} Next: cider pgd build",
        term::green("Bootstrap complete.")
    );
    Ok(())
}

/// Ask a yes/no question. Anything but "y"/"yes" is a no.
pub fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Read one trimmed line of input.
pub fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests run in parallel threads within one process, so each call needs its
    // own file or they race and delete each other's.
    fn roundtrip(input: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!("cider-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("config-{}.toml", N.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, input).unwrap();
        set_domain(&p, "cider").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        std::fs::remove_file(&p).ok();
        out
    }

    #[test]
    fn sets_domain_in_empty_file() {
        let out = roundtrip("");
        assert_eq!(configured_domain_str(&out).as_deref(), Some("cider"));
    }

    #[test]
    fn replaces_an_existing_domain() {
        let out = roundtrip("[dns]\ndomain = \"test\"\n");
        assert_eq!(configured_domain_str(&out).as_deref(), Some("cider"));
    }

    #[test]
    fn preserves_other_sections_and_comments() {
        let input = "# my settings\n[registry]\ndomain = \"docker.io\"\n\n[kernel]\nbinaryPath = \"opt/x\"\n";
        let out = roundtrip(input);
        assert_eq!(configured_domain_str(&out).as_deref(), Some("cider"));
        assert!(out.contains("# my settings"), "comment lost:\n{out}");
        assert!(out.contains("docker.io"), "[registry] lost:\n{out}");
        assert!(out.contains("binaryPath"), "[kernel] lost:\n{out}");
    }

    #[test]
    fn does_not_confuse_registry_domain_for_dns_domain() {
        let doc = "[registry]\ndomain = \"docker.io\"\n";
        assert_eq!(configured_domain_str(doc), None);
    }

    #[test]
    fn adds_domain_to_an_empty_dns_table() {
        let out = roundtrip("[dns]\n\n[registry]\ndomain = \"docker.io\"\n");
        assert_eq!(configured_domain_str(&out).as_deref(), Some("cider"));
        assert!(out.contains("docker.io"));
    }

    #[test]
    fn is_idempotent() {
        let mut out = roundtrip("[dns]\ndomain = \"test\"\n\n[registry]\ndomain = \"docker.io\"\n");
        for _ in 0..3 {
            out = roundtrip(&out);
        }
        assert_eq!(configured_domain_str(&out).as_deref(), Some("cider"));
        assert_eq!(out.matches("[dns]").count(), 1, "duplicated [dns]:\n{out}");
    }

    fn write_temp(input: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(1000);
        let dir = std::env::temp_dir().join(format!("cider-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("rm-{}.toml", N.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, input).unwrap();
        p
    }

    #[test]
    fn access_problem_is_none_for_ordinary_paths() {
        // Existing and readable.
        let p = write_temp("[dns]\ndomain = \"cider\"\n");
        assert_eq!(config_access_problem(&p), None);

        // Missing file in a readable directory is fine — it gets created.
        let missing = p.parent().unwrap().join("not-created-yet.toml");
        assert_eq!(config_access_problem(&missing), None);
    }

    // Reproduces the real failure: an outer directory the user cannot enter,
    // with the config nested below it. The fix must name the *outer* directory,
    // because chowning the inner one leaves the traversal still blocked.
    #[test]
    fn access_problem_reports_the_outermost_blocked_directory() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("cider-acc-{}", std::process::id()));
        let outer = base.join("outer");
        let inner = outer.join("container");
        std::fs::create_dir_all(&inner).unwrap();
        let cfg = inner.join("config.toml");
        std::fs::write(&cfg, "[dns]\ndomain = \"cider\"\n").unwrap();

        // Readable to begin with.
        assert_eq!(config_access_problem(&cfg), None);

        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o000)).unwrap();
        let msg = config_access_problem(&cfg);

        // Restore before asserting, so a failure still leaves a clean tmpdir.
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&base).ok();

        let msg = msg.expect("an unreadable ancestor should be reported");
        assert!(
            msg.contains(outer.to_str().unwrap()),
            "should name the outer directory: {msg}"
        );
        assert!(
            !msg.contains("container/config.toml"),
            "named the inner path instead of the outermost blocker: {msg}"
        );
    }

    #[test]
    fn remove_domain_drops_the_key_and_the_empty_table() {
        let p = write_temp("[dns]\ndomain = \"cider\"\n");
        assert!(remove_domain(&p).unwrap());
        let out = std::fs::read_to_string(&p).unwrap();
        assert_eq!(configured_domain_str(&out), None);
        assert!(!out.contains("[dns]"), "empty [dns] left behind:\n{out}");
    }

    #[test]
    fn remove_domain_preserves_every_other_section() {
        let p = write_temp(
            "# keep me\n[registry]\ndomain = \"docker.io\"\n\n[dns]\ndomain = \"cider\"\n\n[kernel]\nbinaryPath = \"opt/x\"\n",
        );
        assert!(remove_domain(&p).unwrap());
        let out = std::fs::read_to_string(&p).unwrap();
        assert_eq!(configured_domain_str(&out), None);
        assert!(out.contains("# keep me"), "comment lost:\n{out}");
        assert!(out.contains("docker.io"), "[registry] lost:\n{out}");
        assert!(out.contains("binaryPath"), "[kernel] lost:\n{out}");
    }

    #[test]
    fn remove_domain_keeps_a_dns_table_that_has_other_keys() {
        let p = write_temp("[dns]\ndomain = \"cider\"\nsomethingElse = true\n");
        assert!(remove_domain(&p).unwrap());
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("somethingElse"), "sibling key lost:\n{out}");
        assert!(
            out.contains("[dns]"),
            "[dns] removed while still in use:\n{out}"
        );
    }

    #[test]
    fn remove_domain_is_a_no_op_when_there_is_nothing_to_remove() {
        let p = write_temp("[registry]\ndomain = \"docker.io\"\n");
        assert!(!remove_domain(&p).unwrap());
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("docker.io"),
            "registry domain wrongly removed:\n{out}"
        );

        let missing = std::env::temp_dir().join("cider-does-not-exist-at-all.toml");
        assert!(!remove_domain(&missing).unwrap());
    }

    fn configured_domain_str(text: &str) -> Option<String> {
        text.parse::<DocumentMut>()
            .ok()?
            .get("dns")?
            .as_table_like()?
            .get("domain")?
            .as_str()
            .map(|s| s.to_string())
    }
}
