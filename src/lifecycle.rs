//! The lifecycle every product shares: build its image, start a node and wait
//! for it, stop it, start it again, take it all away.
//!
//! Nothing here knows what runs inside a node. A product describes its nodes
//! with a [`Deployment`], and supplies the two things that genuinely differ —
//! how to create a node's container, and how to tell it is ready — as
//! closures. PGD's are in `pgd.rs`.
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

/// How long a node may take before `up` checks whether the runtime's DNS is
/// the hold-up. Registration normally takes well under a second, so a note
/// before this would appear on perfectly ordinary starts.
const DNS_EXPLAIN_AFTER_SECS: u64 = 6;

/// Explain why a node whose name is not in the runtime's DNS yet is not a
/// fault. Shared by `up` and `status`, so both say the same thing.
///
/// The runtime sometimes takes a minute or more to register a container's name
/// after it starts, created or restarted. See the README's Troubleshooting.
///
/// "Three minutes" is the entrypoint's own limit (`PGD_SELF_RESOLVE_TIMEOUT`,
/// 180s, passed to `wait_for_self` in image/entrypoint.sh). Change them
/// together.
pub fn explain_dns_wait(fqdn: &str) {
    term::warn(&format!("{fqdn} is not in the runtime's DNS yet"));
    term::note("After a container starts, the runtime can take a minute or two to register");
    term::note("its name. The node starts, and its peers reach it, as soon as it appears.");
    term::note("If it has not appeared within three minutes, the node gives up and says why.");
}

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
    /// What the banner says is being pressed: "PGD", "logical replication".
    pub title: &'static str,
    /// Typed back to confirm `pomace`.
    pub cluster_name: &'a str,
    pub nodes: u16,
    pub host_prefix: &'a str,
    pub volume_prefix: &'a str,
    pub image: &'a str,
    /// The shared container DNS domain, borrowed from `Config`. Here so that
    /// [`Deployment::host_fqdn`] needs nothing else.
    pub domain: &'a str,
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

    /// Fully-qualified name node `i` is reachable at, e.g. "host-1.cider" —
    /// what nodes dial each other on, and what macOS can resolve too once
    /// `cider bootstrap` has run.
    pub fn host_fqdn(&self, i: u16) -> String {
        config::host_fqdn(self.host_prefix, i, self.domain)
    }

    /// Named volume holding node `i`'s data directory.
    pub fn volume_name(&self, i: u16) -> String {
        config::volume_name(self.volume_prefix, self.host_prefix, i)
    }

    /// Resolve a node argument — "2" or "host-2" — to an index. Anything
    /// missing or unrecognised means node 1, which always exists.
    pub fn node_index(&self, arg: Option<&str>) -> u16 {
        match arg {
            None => 1,
            Some(s) => {
                let t = s.trim();
                if let Ok(n) = t.parse::<u16>()
                    && n >= 1
                {
                    return n;
                }
                t.strip_prefix(self.host_prefix)
                    .and_then(|r| r.parse::<u16>().ok())
                    .unwrap_or(1)
            }
        }
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
/// `--dns-search` is here because every product's nodes address each other
/// as `<host>.<domain>`. The fixed MAC and `CIDER_PEERS` are here because
/// every product's nodes need those names to resolve the moment they start;
/// see [`node_mac`] and [`peers_env`].
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
        "--network".into(),
        format!("default,mac={}", node_mac(&d.host_name(i))),
        "--dns-search".into(),
        cfg.domain.clone(),
        "--env".into(),
        format!("CIDER_PEERS={}", peers_env(d)),
        "--volume".into(),
        format!("{}:{NODE_STATE_DIR}", d.volume_name(i)),
    ]
}

// --- stable addresses ------------------------------------------------------
//
// The runtime's DNS often takes a minute or more to register a container's
// name after it starts (ARCHITECTURE.md, bite #5). Nodes therefore do not
// rely on it to find each other. Instead:
//
//   1. Every container gets a fixed MAC, derived from its name.
//   2. Its IPv6 address is the network's /64 prefix plus an interface ID
//      derived from that MAC (EUI-64), so it is the same on every start.
//   3. Every node is told every node's name and interface ID (CIDER_PEERS),
//      and its entrypoint writes `<prefix>:<id> <name>` into /etc/hosts at
//      each start, using its own current prefix.
//
// Name lookup checks /etc/hosts before DNS, so peers resolve at once. The
// prefix is read inside the container rather than passed in, so if the
// runtime ever hands the network a new prefix, the next start picks it up.
// IPv4 is not used for this: the runtime allocates it in sequence and it
// changes on every start whatever the MAC.

/// A fixed MAC address for a container, derived from its name.
///
/// Without one, the runtime gives a container a new random MAC on every
/// start, and with it a new IPv6 address.
///
/// Derived from the *name*, not the node number, so two clusters running at
/// once (say a scratch cluster beside yours) never share a MAC. The leading
/// `02` marks it locally administered, so it cannot match real hardware.
pub fn node_mac(container_name: &str) -> String {
    let b = mac_bytes(container_name);
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    )
}

fn mac_bytes(container_name: &str) -> [u8; 6] {
    // FNV-1a, 64-bit. Hand-rolled because std's hasher is not promised to
    // give the same answer across Rust releases, and a MAC should not change
    // because the compiler did.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in container_name.bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let b = h.to_be_bytes();
    [0x02, b[3], b[4], b[5], b[6], b[7]]
}

/// The low 64 bits of a container's IPv6 address, from its MAC, as four
/// groups: "0074:56ff:fe18:cc15".
///
/// This is modified EUI-64 (RFC 4291, appendix A), which is how the
/// runtime's network forms addresses: flip the universal/local bit of the
/// first octet, and put ff:fe between the two halves of the MAC.
pub fn interface_id(container_name: &str) -> String {
    eui64(mac_bytes(container_name))
}

fn eui64(m: [u8; 6]) -> String {
    format!(
        "{:02x}{:02x}:{:02x}ff:fe{:02x}:{:02x}{:02x}",
        m[0] ^ 0x02,
        m[1],
        m[2],
        m[3],
        m[4],
        m[5]
    )
}

/// `CIDER_PEERS` for this deployment: every node, itself included, as
/// `<fqdn>=<interface id>`, comma-separated. The same list for every node.
pub fn peers_env(d: &Deployment) -> String {
    (1..=d.nodes)
        .map(|i| format!("{}={}", d.host_fqdn(i), interface_id(&d.host_name(i))))
        .collect::<Vec<_>>()
        .join(",")
}

/// Block until `ready` says node `i` is ready, or fail with its logs.
///
/// Three outcomes matter and all three are handled: the container vanished,
/// the container exited (its entrypoint gave up), or it is still running but
/// not yet ready. Only the last one is worth waiting on.
fn wait_for_node(d: &Deployment, i: u16, ready: &impl Fn(u16) -> bool) -> Result<()> {
    let name = d.host_name(i);
    let fqdn = d.host_fqdn(i);
    print!("  waiting for {name} ");
    std::io::stdout().flush().ok();
    let mut waited = 0u64;
    let mut dns_explained = false;
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
        // A long wait is usually the runtime's DNS, not the node. Say so once,
        // rather than leave a line of dots that looks like a hang.
        if !dns_explained
            && waited >= DNS_EXPLAIN_AFTER_SECS
            && container::resolves_inside(&name, &fqdn) == Some(false)
        {
            println!();
            explain_dns_wait(&fqdn);
            print!("  still waiting for {name} ");
            dns_explained = true;
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
    let containers: Vec<String> = (1..=d.nodes).map(|i| d.host_name(i)).collect();
    let volumes: Vec<String> = (1..=d.nodes).map(|i| d.volume_name(i)).collect();

    term::info("containers");
    if let Some(out) = container::capture(&["ls", "--all"]) {
        for line in own_rows(&out, &containers) {
            println!("{line}");
        }
    }
    println!();
    term::info("volumes");
    if let Some(out) = container::capture(&["volume", "list"]) {
        for line in own_rows(&out, &volumes) {
            println!("{line}");
        }
    }
}

/// The header of a `container` listing plus the rows whose first column is
/// one of `names`.
///
/// Matched exactly rather than by prefix: every product's volumes start
/// `cider-press-`, so a prefix match would show one product's volumes under
/// another's name.
fn own_rows<'a>(listing: &'a str, names: &[String]) -> Vec<&'a str> {
    listing
        .lines()
        .enumerate()
        .filter(|(n, line)| {
            *n == 0
                || line
                    .split_whitespace()
                    .next()
                    .is_some_and(|first| names.iter().any(|name| name == first))
        })
        .map(|(_, line)| line)
        .collect()
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
///
/// Returns as soon as the containers are running, not when the nodes are
/// ready — so it says what to expect in between, rather than leave `status`
/// to report `Unreachable` with no explanation.
pub fn start(d: &Deployment) -> Result<()> {
    let mut started = 0;
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        match container::state(&name) {
            container::State::Stopped => {
                if container::quiet_ok(&["start", &name]) {
                    term::ok(&format!("started {name}"));
                    started += 1;
                }
            }
            container::State::Running => term::ok(&format!("{name} already running")),
            container::State::Absent => {
                term::warn(&format!("{name} does not exist — run: {}", d.hint("up")))
            }
        }
    }
    if started > 0 {
        println!();
        term::note("Nodes can show Unreachable for a minute or two while the runtime");
        term::note(&format!(
            "registers their names in DNS. Check with: {}",
            d.hint("status")
        ));
    }
    Ok(())
}

/// Remove the containers but keep the volumes.
///
/// `up` afterwards brings back the *same* cluster — same node identities, same
/// data — because the volumes still hold each node's data directory.
pub fn down(d: &Deployment) -> Result<()> {
    term::banner_for(d.title);
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
    term::banner_for(d.title);
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
            title: "PGD",
            cluster_name: "cider",
            nodes: 3,
            host_prefix: "host-",
            volume_prefix: "cider-press-",
            image: "cider-press:latest",
            domain: "cider",
            dockerfile: "image/Dockerfile",
            ready_timeout: 420,
        }
    }

    #[test]
    fn node_arguments_resolve_by_number_or_container_name() {
        let d = pgd_like();
        assert_eq!(d.node_index(None), 1);
        assert_eq!(d.node_index(Some("2")), 2);
        assert_eq!(d.node_index(Some("host-3")), 3);
        assert_eq!(d.node_index(Some(" 2 ")), 2);
        // Unrecognised input falls back to node 1 rather than failing.
        assert_eq!(d.node_index(Some("0")), 1);
        assert_eq!(d.node_index(Some("dolores-2")), 1);
    }

    // Both products' volumes start "cider-press-", and one product's
    // `containers` must not list the other's.
    #[test]
    fn container_table_shows_only_this_deployments_rows() {
        let listing = "NAME                   TYPE   DRIVER\n\
                       cider-press-host-1     named  local\n\
                       cider-press-host-10    named  local\n\
                       cider-press-dolores-1  named  local\n\
                       cider-press-host-2     named  local\n";
        let names = vec![
            "cider-press-host-1".to_string(),
            "cider-press-host-2".to_string(),
        ];
        let rows = own_rows(listing, &names);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[0].starts_with("NAME"));
        assert!(
            rows.iter()
                .all(|r| !r.contains("dolores") && !r.contains("host-10"))
        );
    }

    // Both values observed on real containers on this runtime, so these test
    // the derivation against the network's actual behaviour, not against
    // itself.
    #[test]
    fn interface_ids_match_what_the_runtime_assigns() {
        // A hand-picked MAC: the container came up as fd68:…:c1:deff:fe00:1.
        assert_eq!(
            eui64([0x02, 0xc1, 0xde, 0x00, 0x00, 0x01]),
            "00c1:deff:fe00:0001"
        );
        // host-1's derived MAC: it came up as fd68:…:74:56ff:fe18:cc15.
        assert_eq!(node_mac("host-1"), "02:74:56:18:cc:15");
        assert_eq!(interface_id("host-1"), "0074:56ff:fe18:cc15");
    }

    #[test]
    fn node_macs_are_distinct_locally_administered_unicast() {
        let names = [
            "host-1",
            "host-2",
            "host-3",
            "scratch-1",
            "dolores-1",
            "dolores-2",
        ];
        let macs: std::collections::HashSet<_> = names.iter().map(|n| node_mac(n)).collect();
        assert_eq!(macs.len(), names.len(), "two names share a MAC");
        for name in names {
            let first = mac_bytes(name)[0];
            assert_eq!(first & 0b01, 0, "{name}: must be unicast");
            assert_eq!(first & 0b10, 0b10, "{name}: must be locally administered");
        }
    }

    #[test]
    fn peers_list_every_node_including_itself() {
        assert_eq!(
            peers_env(&pgd_like()),
            format!(
                "host-1.cider={},host-2.cider={},host-3.cider={}",
                interface_id("host-1"),
                interface_id("host-2"),
                interface_id("host-3")
            )
        );
    }

    #[test]
    fn every_node_gets_its_fixed_mac_and_the_peer_list() {
        let args = base_run_args(&Config::load(), &pgd_like(), 2);
        let pos = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(args[pos + 1], format!("default,mac={}", node_mac("host-2")));
        assert!(args.contains(&format!("CIDER_PEERS={}", peers_env(&pgd_like()))));
    }

    #[test]
    fn fqdn_is_container_name_plus_domain() {
        assert_eq!(pgd_like().host_fqdn(2), "host-2.cider");
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
