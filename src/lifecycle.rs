//! The lifecycle every product shares: build its image, start a node and wait
//! for it, stop it, start it again, take it all away.
//!
//! Nothing here knows what runs inside a node. A product describes its nodes
//! with a [`Deployment`], and supplies the two things that genuinely differ —
//! how to create a node's container, and how to tell it is ready — as
//! closures. PGD's are in `cluster.rs`.
//!
//! Everything in this file exists because of a specific failure: the stop
//! signal, the retry, the stopped-container check, the typed confirmation on
//! `pomace`. It is shared rather than copied so that those fixes cannot drift
//! apart between products.

use crate::config::{self, Config};
use crate::{bootstrap, container, term};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

/// How many times to start a node before giving up. Two, because the failure
/// this guards against is a transient DNS-registration race that a restart
/// clears; more attempts would just delay a real error.
const NODE_ATTEMPTS: u32 = 2;

/// The signal that gives Postgres a *fast* shutdown.
///
/// Postgres reads SIGTERM, which is what `container stop` sends by default, as
/// a smart shutdown that waits for every client to disconnect — and PGD nodes
/// keep connections open to each other, so it waits forever and gets killed.
/// Every product here runs Postgres, and a fast shutdown is what stopping a
/// lab container should mean for all of them. See `container::stop_with_signal`
/// for the whole story.
const PG_STOP_SIGNAL: &str = "SIGINT";

/// Grace period for a node to shut down before the runtime kills it. This is a
/// ceiling: a clean shutdown of an idle node takes about a second.
const PG_STOP_TIMEOUT_SECS: u64 = 60;

/// Where every image mounts its node's volume. A convention the images share,
/// not a setting: each image's `PGDATA` and log live under it.
const NODE_STATE_DIR: &str = "/var/lib/cider-press";

/// One product's nodes, as the shared lifecycle sees them.
///
/// The container prefix, the volume prefix and the image tag live here because
/// they are exactly what two products must *not* share if both are to run at
/// once. Everything else a product needs stays in its own module.
pub struct Deployment<'a> {
    /// The `<group>` in `cider <group> <verb>`, used in every "run: ..." hint.
    pub group: &'static str,
    /// Typed back to confirm `pomace`.
    pub cluster_name: &'a str,
    pub nodes: u16,
    pub host_prefix: &'a str,
    pub volume_prefix: &'a str,
    pub image: &'a str,
    /// Relative to the project root. Its directory is the build context.
    pub dockerfile: &'static str,
    /// How long `up` waits for one node to become ready, in seconds.
    pub ready_timeout: u64,
}

impl Deployment<'_> {
    /// Container name for node `i`, e.g. "host-1".
    pub fn host_name(&self, i: u16) -> String {
        config::container_name(self.host_prefix, i)
    }

    /// Named volume holding node `i`'s data directory.
    pub fn volume_name(&self, i: u16) -> String {
        config::volume_name(self.volume_prefix, self.host_prefix, i)
    }

    /// A command to suggest to the user, e.g. `hint("up")` is "cider pgd up".
    pub fn hint(&self, verb: &str) -> String {
        format!("cider {} {verb}", self.group)
    }
}

/// Stop one node cleanly, so the next start does not pay crash recovery.
///
/// Every path that stops a node goes through here. Getting this wrong is not
/// visible at the time — the container stops either way — and shows up later
/// as a slow `up`, which is a long way from the cause.
pub fn stop_node(name: &str) -> bool {
    container::stop_with_signal(name, PG_STOP_SIGNAL, PG_STOP_TIMEOUT_SECS)
}

// --- build -----------------------------------------------------------------

/// Build a product's image from its Dockerfile.
///
/// `extra` carries whatever the product adds — build args, and for PGD the
/// token as a BuildKit secret — and is placed straight after `--tag`. The
/// caller prints its own header first, because what is worth saying about a
/// build differs by product.
pub fn build_image(cfg: &Config, d: &Deployment, extra: &[String], no_cache: bool) -> Result<()> {
    let dockerfile = cfg.root.join(d.dockerfile);
    let context_dir = dockerfile
        .parent()
        .context("the Dockerfile path has no parent directory")?
        .to_path_buf();
    if !dockerfile.exists() {
        bail!("{} not found", dockerfile.display());
    }

    let mut args: Vec<String> = vec![
        "build".into(),
        "--file".into(),
        dockerfile.display().to_string(),
        "--tag".into(),
        d.image.to_string(),
    ];
    args.extend(extra.iter().cloned());
    args.extend([
        "--cpus".into(),
        cfg.build_cpus.clone(),
        "--memory".into(),
        cfg.build_memory.clone(),
        "--progress".into(),
        "plain".into(),
    ]);
    if no_cache {
        args.push("--no-cache".into());
    }
    args.push(context_dir.display().to_string());

    if !container::run_streaming(&args)? {
        bail!("build failed");
    }
    println!();
    term::ok(&format!("built {}", d.image));
    println!("Next: {}", d.hint("up"));
    Ok(())
}

// --- up --------------------------------------------------------------------

/// Refuse to start nodes on a Mac that cannot run them.
///
/// Each of these, if wrong, produces a failure minutes later inside a
/// container, so they are worth failing fast on.
pub fn preflight(d: &Deployment) -> Result<()> {
    if !container::installed() {
        bail!("container is not installed — run: cider doctor");
    }
    if !container::system_running() {
        bail!("container system is not running — run: container system start");
    }
    if !container::image_exists(d.image) {
        bail!("{} is not built — run: {}", d.image, d.hint("build"));
    }
    Ok(())
}

/// Refuse to start nodes if the runtime's DNS domain is not the one they will
/// address each other on.
///
/// Kept apart from [`preflight`] because it needs the host configuration, not
/// just the deployment.
pub fn require_dns_domain(cfg: &Config) -> Result<()> {
    let path = Config::container_config_path();
    let current = path.as_deref().and_then(bootstrap::configured_domain);
    if current.as_deref() != Some(cfg.domain.as_str()) {
        bail!(
            "container DNS domain is \"{}\", not \"{}\".\n  \
             Nodes address each other as <host>.{}, so this must match.\n  \
             Run: cider bootstrap",
            current.unwrap_or_else(|| "unset".into()),
            cfg.domain,
            cfg.domain
        );
    }
    Ok(())
}

/// Index of the first node whose container is running.
///
/// Anything that only needs *a* way into the cluster should enter through this
/// rather than assuming node 1. Keeps working when node 1 is the one that went
/// away, which is exactly when you reach for these commands.
pub fn first_running(d: &Deployment) -> Result<u16> {
    (1..=d.nodes)
        .find(|&i| container::state(&d.host_name(i)) == container::State::Running)
        .with_context(|| {
            format!(
                "no node of '{}' is running — run: {}",
                d.cluster_name,
                d.hint("up")
            )
        })
}

/// Get node `i` as far as the product's own `container run`.
///
/// Returns `true` when a new container must be created — its volume now
/// exists — and `false` when there is nothing more to do because the container
/// was already running or has just been started again.
///
/// An existing *stopped* container is started, never recreated, so a node
/// keeps its identity and data across `down`/`up`.
pub fn prepare_node(d: &Deployment, i: u16) -> Result<bool> {
    let name = d.host_name(i);
    let vol = d.volume_name(i);

    match container::state(&name) {
        container::State::Running => {
            term::ok(&format!("{name} already running"));
            return Ok(false);
        }
        container::State::Stopped => {
            term::info(&format!("starting existing container {name}"));
            if !container::quiet_ok(&["start", &name]) {
                bail!("could not start {name}");
            }
            return Ok(false);
        }
        container::State::Absent => {}
    }

    if !container::volume_exists(&vol) {
        if !container::quiet_ok(&["volume", "create", &vol]) {
            bail!("could not create volume {vol}");
        }
        term::ok(&format!("created volume {vol}"));
    }
    Ok(true)
}

/// The `container run` flags every node gets, whatever runs inside it.
///
/// The product appends its published ports, its environment and the image.
/// `--dns-search` is here because every product's nodes find each other the
/// same way: as `<host>.<domain>` through the runtime's DNS.
pub fn base_run_args(cfg: &Config, d: &Deployment, i: u16) -> Vec<String> {
    vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        d.host_name(i),
        "--cpus".into(),
        cfg.cpus.clone(),
        "--memory".into(),
        cfg.memory.clone(),
        "--dns-search".into(),
        cfg.domain.clone(),
        "--volume".into(),
        format!("{}:{NODE_STATE_DIR}", d.volume_name(i)),
    ]
}

/// Block until `ready` says node `i` is ready, or fail with its logs.
///
/// Three outcomes matter and all three are handled: the container vanished,
/// the container exited (its entrypoint gave up), or it is still running but
/// not yet ready. Only the last one is worth waiting on.
fn wait_for_node(d: &Deployment, i: u16, ready: &impl Fn(u16) -> bool) -> Result<()> {
    let name = d.host_name(i);
    print!("  waiting for {name} ");
    std::io::stdout().flush().ok();
    let mut waited = 0u64;
    while waited < d.ready_timeout {
        // A node whose entrypoint gave up leaves a *stopped* container, not an
        // absent one, so both count as failure.
        match container::state(&name) {
            container::State::Absent => {
                println!();
                bail!(
                    "{name} disappeared. Last logs:\n{}",
                    container::logs_tail(&name, 30)
                );
            }
            container::State::Stopped => {
                println!();
                bail!("{name} exited before it was ready");
            }
            container::State::Running => {}
        }
        if ready(i) {
            println!(" {}", term::green("ready"));
            return Ok(());
        }
        print!(".");
        std::io::stdout().flush().ok();
        sleep(Duration::from_secs(3));
        waited += 3;
    }
    println!();
    bail!("{name} did not become ready within {}s", d.ready_timeout)
}

/// Start node `i` with `start` and wait for `ready`, retrying once on failure.
///
/// The runtime registers a container in its DNS asynchronously, and that
/// registration occasionally does not land before the entrypoint gives up. It
/// is transient: restarting the same container almost always succeeds, which is
/// exactly what a human would do next. Doing it automatically is the difference
/// between a tool that works every time and one that works most times.
///
/// Retrying is only safe if a failed start leaves nothing half-done behind.
/// That is the product's job: PGD's entrypoint discards a partial PGDATA before
/// exiting, so a restart always begins from a clean state. A new product must
/// make the same guarantee before relying on this.
pub fn start_and_wait(
    d: &Deployment,
    i: u16,
    start: impl Fn(u16) -> Result<()>,
    ready: impl Fn(u16) -> bool,
) -> Result<()> {
    let name = d.host_name(i);

    for attempt in 1..=NODE_ATTEMPTS {
        start(i)?;
        match wait_for_node(d, i, &ready) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < NODE_ATTEMPTS => {
                term::warn(&format!("{e}"));
                term::info(&format!(
                    "retrying {name} (attempt {}/{NODE_ATTEMPTS})",
                    attempt + 1
                ));
            }
            Err(e) => {
                println!("{}", term::red(&format!("Last 40 log lines from {name}:")));
                println!("{}", container::logs_tail(&name, 40));
                return Err(e);
            }
        }
    }
    unreachable!("loop returns on the final attempt")
}

// --- inspection ------------------------------------------------------------

/// Raw `container ls` and `volume list`, filtered to this deployment's own
/// containers and volumes.
pub fn containers_table(d: &Deployment) {
    term::info("containers");
    if let Some(out) = container::capture(&["ls", "--all"]) {
        for (n, line) in out.lines().enumerate() {
            if n == 0 || line.starts_with(d.host_prefix) {
                println!("{line}");
            }
        }
    }
    println!();
    term::info("volumes");
    if let Some(out) = container::capture(&["volume", "list"]) {
        for (n, line) in out.lines().enumerate() {
            if n == 0 || line.starts_with(d.volume_prefix) {
                println!("{line}");
            }
        }
    }
}

// --- lifecycle -------------------------------------------------------------

/// Stop the node containers, leaving them and their data intact.
pub fn stop(d: &Deployment) -> Result<()> {
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        if container::state(&name) == container::State::Running {
            if stop_node(&name) {
                term::ok(&format!("stopped {name}"));
            } else {
                term::warn(&format!("could not stop {name}"));
            }
        }
    }
    Ok(())
}

/// Start previously stopped node containers.
pub fn start(d: &Deployment) -> Result<()> {
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        match container::state(&name) {
            container::State::Stopped => {
                if container::quiet_ok(&["start", &name]) {
                    term::ok(&format!("started {name}"));
                }
            }
            container::State::Running => term::ok(&format!("{name} already running")),
            container::State::Absent => {
                term::warn(&format!("{name} does not exist — run: {}", d.hint("up")))
            }
        }
    }
    Ok(())
}

/// Remove the containers but keep the volumes.
///
/// `up` afterwards brings back the *same* cluster — same node identities, same
/// data — because the volumes still hold each node's data directory.
pub fn down(d: &Deployment) -> Result<()> {
    term::banner();
    term::info("removing containers (volumes and image are kept)");
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        match container::state(&name) {
            container::State::Absent => println!("  {}", term::dim(&format!("{name} not present"))),
            _ => {
                // Clean shutdown first: `up` afterwards brings this same node
                // back, and a node killed here recovers on the way up.
                let _ = stop_node(&name);
                if container::quiet_ok(&["delete", &name]) {
                    term::ok(&format!("removed {name}"));
                }
            }
        }
    }
    println!();
    println!(
        "Data is still in the volumes. {} brings the same cluster back.",
        d.hint("up")
    );
    println!("To discard everything: {}", d.hint("pomace"));
    Ok(())
}

/// Destroy everything: containers, volumes, image, optionally the host DNS.
///
/// Irreversible, so it lists exactly what will go and requires the cluster name
/// typed back. `remove_dns` additionally undoes `bootstrap`, and asks again
/// separately — that part touches settings shared with every other container
/// on the machine, including any other product's nodes.
pub fn pomace(cfg: &Config, d: &Deployment, assume_yes: bool, remove_dns: bool) -> Result<()> {
    term::banner();
    println!("{}", term::yellow("This permanently destroys:"));
    for i in 1..=d.nodes {
        println!(
            "  container {:<10} volume {:<20} {}",
            d.host_name(i),
            d.volume_name(i),
            term::dim("(all its data)")
        );
    }
    println!("  image     {}", d.image);
    println!();
    if remove_dns {
        println!("Then, with your confirmation, the host DNS setup too (--dns).");
    } else {
        println!(
            "Left alone: your container DNS config and the *.{} resolver.",
            cfg.domain
        );
        println!(
            "  {}",
            term::dim("Add --dns to remove those as well (full teardown).")
        );
    }
    println!();

    if !assume_yes {
        let reply = bootstrap::prompt_line(&format!(
            "Type the cluster name ({}) to confirm: ",
            d.cluster_name
        ))?;
        if reply != d.cluster_name {
            println!("Aborted.");
            return Ok(());
        }
    }

    for i in 1..=d.nodes {
        let name = d.host_name(i);
        if container::exists(&name) {
            // The volume is about to be deleted, so recovery cost is moot, but
            // a fast shutdown still returns sooner than waiting out the kill
            // timer.
            let _ = stop_node(&name);
            if container::quiet_ok(&["delete", &name]) {
                term::ok(&format!("removed container {name}"));
            }
        }
    }
    for i in 1..=d.nodes {
        let vol = d.volume_name(i);
        if container::volume_exists(&vol) {
            if container::quiet_ok(&["volume", "delete", &vol]) {
                term::ok(&format!("removed volume {vol}"));
            } else {
                term::warn(&format!("could not remove volume {vol}"));
            }
        }
    }
    if container::image_exists(d.image) {
        if container::quiet_ok(&["image", "delete", d.image]) {
            term::ok(&format!("removed image {}", d.image));
        } else {
            term::warn(&format!("could not remove image {}", d.image));
        }
    }
    if remove_dns {
        bootstrap::remove_dns_setup(cfg, assume_yes)?;
    }

    println!();
    println!("{}", term::green("All pressed out."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pgd_like() -> Deployment<'static> {
        Deployment {
            group: "pgd",
            cluster_name: "cider",
            nodes: 3,
            host_prefix: "host-",
            volume_prefix: "cider-press-",
            image: "cider-press:latest",
            dockerfile: "image/Dockerfile",
            ready_timeout: 420,
        }
    }

    // These names are load-bearing: an existing cluster's volumes were created
    // under them, and a change here would quietly orphan that data. `up` would
    // create fresh, empty volumes and the old ones would sit unused.
    #[test]
    fn names_match_what_existing_clusters_were_created_with() {
        let d = pgd_like();
        assert_eq!(d.host_name(1), "host-1");
        assert_eq!(d.host_name(3), "host-3");
        assert_eq!(d.volume_name(2), "cider-press-host-2");
    }

    #[test]
    fn hints_name_the_group() {
        let d = pgd_like();
        assert_eq!(d.hint("up"), "cider pgd up");
        let d = Deployment {
            group: "logical",
            ..d
        };
        assert_eq!(d.hint("pomace"), "cider logical pomace");
    }

    #[test]
    fn every_node_mounts_its_own_volume_at_the_shared_state_dir() {
        let d = pgd_like();
        // Config supplies only the host-level flags (cpus, memory, domain),
        // none of which are asserted, so whatever the environment holds is
        // fine. Both values checked come from the Deployment.
        let args = base_run_args(&Config::load(), &d, 2);
        let pos = args.iter().position(|a| a == "--volume").unwrap();
        assert_eq!(args[pos + 1], "cider-press-host-2:/var/lib/cider-press");
        let pos = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[pos + 1], "host-2");
    }
}
