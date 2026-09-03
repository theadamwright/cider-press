//! Build the image, press a cluster, take it apart again.

use crate::config::{
    CM_HTTP_CONTAINER_PORT, CM_RO_CONTAINER_PORT, CM_RW_CONTAINER_PORT, Config,
    MONITOR_CONTAINER_PORT, PG_CONTAINER_PORT,
};
use crate::{bootstrap, container, monitor, state, term};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

/// How many times to start a node before giving up. Two, because the failure
/// this guards against is a transient DNS-registration race that a restart
/// clears; more attempts would just delay a real error.
const NODE_ATTEMPTS: u32 = 2;

/// How long to wait for Connection Manager after the last node joins. Short:
/// this is a courtesy, not a health check, and `up` succeeds regardless.
const CM_READY_TIMEOUT_SECS: u64 = 60;

// --- build -----------------------------------------------------------------

/// Build the node image.
///
/// The heavy lifting is in `image/Dockerfile`; this just assembles the flags.
/// The one part worth understanding is the token: it is passed as a BuildKit
/// *secret* read from our own environment, never as a `--build-arg`, so it
/// cannot end up in the image config or the layer history.
pub fn build(cfg: &Config, no_cache: bool) -> Result<()> {
    if !container::installed() {
        bail!("container is not installed — run: cider doctor");
    }
    let token = cfg.token.as_deref().context(
        "EDB_SUBSCRIPTION_TOKEN is not set.\n\n  \
         export EDB_SUBSCRIPTION_TOKEN=\"your-token\"\n\n  \
         or put it in .env (gitignored). Get a token at\n  \
         https://www.enterprisedb.com/repos-downloads",
    )?;
    let _ = token; // consumed by the build as a secret, read from our environment

    term::banner();
    term::info(&format!("building {}", cfg.image));
    println!("  flavor   {}  postgres {}", cfg.pg_flavor, cfg.pg_major);
    println!("  base     debian:{}-slim (arm64)", cfg.debian_version);
    println!("  token    passed as a BuildKit secret, never stored in the image");
    println!();

    let dockerfile = cfg.root.join("image/Dockerfile");
    let context_dir = cfg.root.join("image");
    if !dockerfile.exists() {
        bail!("{} not found", dockerfile.display());
    }

    // --secret ...,env=VAR reads straight from this process's environment, so
    // the token never touches disk and never lands in a layer or the history.
    let mut args: Vec<String> = vec![
        "build".into(),
        "--file".into(),
        dockerfile.display().to_string(),
        "--tag".into(),
        cfg.image.clone(),
        "--secret".into(),
        "id=edb_token,env=EDB_SUBSCRIPTION_TOKEN".into(),
        "--build-arg".into(),
        format!("PG_FLAVOR={}", cfg.pg_flavor),
        "--build-arg".into(),
        format!("PG_MAJOR={}", cfg.pg_major),
        "--build-arg".into(),
        format!("DEBIAN_VERSION={}", cfg.debian_version),
        "--cpus".into(),
        cfg.build_cpus.clone(),
        "--memory".into(),
        cfg.build_memory.clone(),
        "--progress".into(),
        "plain".into(),
    ];
    if no_cache {
        args.push("--no-cache".into());
    }
    args.push(context_dir.display().to_string());

    if !container::run_streaming(&args)? {
        bail!("build failed");
    }
    println!();
    term::ok(&format!("built {}", cfg.image));
    println!("Next: cider pgd up");
    Ok(())
}

/// Index of the first node whose container is running.
///
/// Anything that only needs *a* way into the cluster should enter through this
/// rather than assuming node 1. Every node runs its own Connection Manager and
/// every one of them routes to the current write leader, so entering through a
/// surviving node is equivalent — and keeps working when node 1 is the one that
/// went away, which is exactly when you reach for these commands.
pub fn first_running(cfg: &Config) -> Result<u16> {
    (1..=cfg.nodes)
        .find(|&i| container::state(&cfg.host_name(i)) == container::State::Running)
        .with_context(|| {
            format!(
                "no node of '{}' is running — run: cider pgd up",
                cfg.cluster_name
            )
        })
}

// --- up --------------------------------------------------------------------

/// Is *this* node a live member of the cluster under its own name?
///
/// Checking only that `bdr.local_node_summary` is queryable is not enough: a
/// physical join starts a temporary server on a copy of the seed node's data
/// directory, so during the join window the view exists and answers — as the
/// *seed*. A node whose join later failed would still have looked "ready".
/// Matching the expected node name closes that window.
fn node_joined(cfg: &Config, container_name: &str, node_name: &str) -> bool {
    let sql = format!("select 1 from bdr.local_node_summary where node_name = '{node_name}'");
    container::exec_capture(
        container_name,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
            "-U",
            &cfg.user,
            "-d",
            &cfg.db,
            "-tAqc",
            &sql,
        ],
    )
    .is_some_and(|out| out.trim() == "1")
}

/// Block until a node has genuinely joined, or fail with its logs.
///
/// Three outcomes matter and all three are handled: the container vanished,
/// the container exited (its entrypoint gave up), or it is still running but
/// not yet a cluster member. Only the last one is worth waiting on.
fn wait_for_node(cfg: &Config, name: &str, node_name: &str) -> Result<()> {
    print!("  waiting for {name} ");
    std::io::stdout().flush().ok();
    let mut waited = 0u64;
    while waited < cfg.ready_timeout {
        // A node whose entrypoint gave up leaves a *stopped* container, not an
        // absent one, so both count as failure.
        match container::state(name) {
            container::State::Absent => {
                println!();
                bail!(
                    "{name} disappeared. Last logs:\n{}",
                    container::logs_tail(name, 30)
                );
            }
            container::State::Stopped => {
                println!();
                bail!("{name} exited before joining the cluster");
            }
            container::State::Running => {}
        }
        if node_joined(cfg, name, node_name) {
            println!(" {}", term::green("ready"));
            return Ok(());
        }
        print!(".");
        std::io::stdout().flush().ok();
        sleep(Duration::from_secs(3));
        waited += 3;
    }
    println!();
    bail!("{name} did not become ready within {}s", cfg.ready_timeout)
}

/// Start a node and wait for it to join, retrying once if it fails.
///
/// The runtime registers a container in its DNS asynchronously, and that
/// registration occasionally does not land before the entrypoint gives up. It
/// is transient: restarting the same container almost always succeeds, which is
/// exactly what a human would do next. Doing it automatically is the difference
/// between a tool that works every time and one that works most times.
///
/// Retrying is safe because a node that fails this way never reached
/// provisioning, and one that fails *during* a join discards its partial
/// PGDATA before exiting — so a restart always begins from a clean state.
fn start_and_wait(cfg: &Config, i: u16) -> Result<()> {
    let name = cfg.host_name(i);
    let node = cfg.node_name(i);

    for attempt in 1..=NODE_ATTEMPTS {
        start_node(cfg, i)?;
        match wait_for_node(cfg, &name, &node) {
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

/// Start node `i`, creating its volume first if this is its first run.
///
/// An existing container is simply started again; an existing *stopped* one is
/// not recreated, so a node keeps its identity and data across `down`/`up`.
/// Everything the entrypoint needs is passed as environment variables — the
/// image itself holds no per-node configuration.
fn start_node(cfg: &Config, i: u16) -> Result<()> {
    let name = cfg.host_name(i);
    let fqdn = cfg.host_fqdn(i);
    let node = cfg.node_name(i);
    let vol = cfg.volume_name(i);

    match container::state(&name) {
        container::State::Running => {
            term::ok(&format!("{name} already running"));
            return Ok(());
        }
        container::State::Stopped => {
            term::info(&format!("starting existing container {name}"));
            if !container::quiet_ok(&["start", &name]) {
                bail!("could not start {name}");
            }
            return Ok(());
        }
        container::State::Absent => {}
    }

    if !container::volume_exists(&vol) {
        if !container::quiet_ok(&["volume", "create", &vol]) {
            bail!("could not create volume {vol}");
        }
        term::ok(&format!("created volume {vol}"));
    }

    term::info(&format!("running {name} ({node}) at {fqdn}"));

    let monitor_flag = if cfg.monitor { "on" } else { "off" };
    let args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        name.clone(),
        "--cpus".into(),
        cfg.cpus.clone(),
        "--memory".into(),
        cfg.memory.clone(),
        "--dns-search".into(),
        cfg.domain.clone(),
        "--volume".into(),
        format!("{vol}:/var/lib/cider-press"),
        "--publish".into(),
        format!("127.0.0.1:{}:{PG_CONTAINER_PORT}", cfg.pg_port(i)),
        "--publish".into(),
        format!("127.0.0.1:{}:{CM_RW_CONTAINER_PORT}", cfg.cm_rw(i)),
        "--publish".into(),
        format!("127.0.0.1:{}:{CM_RO_CONTAINER_PORT}", cfg.cm_ro(i)),
        "--publish".into(),
        format!("127.0.0.1:{}:{CM_HTTP_CONTAINER_PORT}", cfg.cm_http(i)),
        "--publish".into(),
        format!("127.0.0.1:{}:{MONITOR_CONTAINER_PORT}", cfg.ui_port(i)),
        "--env".into(),
        format!("PGD_NODE_NAME={node}"),
        "--env".into(),
        format!("PGD_HOST_FQDN={fqdn}"),
        "--env".into(),
        format!("PGD_IS_FIRST={}", i == 1),
        "--env".into(),
        format!("PGD_GROUP_NAME={}", cfg.group_name),
        "--env".into(),
        format!("PGD_CLUSTER_NAME={}", cfg.cluster_name),
        "--env".into(),
        format!("PGD_INITIAL_NODE_COUNT={}", cfg.nodes),
        "--env".into(),
        format!("PGD_JOIN_DSN={}", cfg.join_dsn()),
        "--env".into(),
        format!("PGD_ALL_HOSTS={}", cfg.all_hosts_csv()),
        "--env".into(),
        format!("PGD_MONITOR_ENABLED={monitor_flag}"),
        "--env".into(),
        format!(
            "PGD_STAT_STATEMENTS={}",
            if cfg.stat_statements { "on" } else { "off" }
        ),
        "--env".into(),
        format!("POSTGRES_DB={}", cfg.db),
        "--env".into(),
        format!("POSTGRES_USER={}", cfg.user),
        "--env".into(),
        format!("PGPASSWORD={}", cfg.password),
        cfg.image.clone(),
    ];

    if !container::run_streaming(&args)? {
        bail!("could not run {name}");
    }
    Ok(())
}

/// Create the cluster: volumes, then nodes, then cluster-wide settings.
///
/// Nodes start **one at a time**, and that is deliberate rather than lazy:
/// node 1 seeds the cluster and nodes 2..n join it, and concurrent joins
/// against a fresh PGD cluster are not safe. Each node must be a confirmed
/// member before the next one starts.
///
/// Everything before the loop is a precondition check. Getting those wrong
/// produces failures minutes later inside a container, so they are worth
/// failing fast on.
pub fn up(cfg: &Config) -> Result<()> {
    if !container::installed() {
        bail!("container is not installed — run: cider doctor");
    }
    if !container::system_running() {
        bail!("container system is not running — run: container system start");
    }
    if !container::image_exists(&cfg.image) {
        bail!("{} is not built — run: cider pgd build", cfg.image);
    }

    let path = Config::container_config_path();
    let current = path.as_deref().and_then(bootstrap::configured_domain);
    if current.as_deref() != Some(cfg.domain.as_str()) {
        bail!(
            "container DNS domain is \"{}\", not \"{}\".\n  \
             PGD nodes address each other as <host>.{}, so this must match.\n  \
             Run: cider bootstrap",
            current.unwrap_or_else(|| "unset".into()),
            cfg.domain,
            cfg.domain
        );
    }

    term::banner();
    term::info(&format!(
        "pressing a {}-node PGD cluster '{}'",
        cfg.nodes, cfg.cluster_name
    ));
    println!();

    // Node 1 creates the cluster; the rest join it. Serialised on purpose —
    // PGD joins are not safe to run concurrently against a fresh cluster.
    for i in 1..=cfg.nodes {
        start_and_wait(cfg, i)?;
    }

    apply_pool_mode(cfg);
    create_stat_statements(cfg);
    wait_for_connection_manager(cfg);

    println!();
    status(cfg)?;
    println!();
    endpoints(cfg);
    Ok(())
}

/// Create the pg_stat_statements extension, but only if the entrypoint
/// actually got the library preloaded — creating it without the library gives
/// a view that errors on every read, which is worse than not having it.
fn create_stat_statements(cfg: &Config) {
    if !cfg.stat_statements {
        return;
    }
    let host = cfg.host_name(1);
    let loaded = container::exec_capture(
        &host,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
            "-U",
            &cfg.user,
            "-d",
            &cfg.db,
            "-tAqc",
            "show shared_preload_libraries",
        ],
    )
    .is_some_and(|v| v.contains("pg_stat_statements"));

    if !loaded {
        term::warn("pg_stat_statements is not preloaded — Query Diagnostics will be empty");
        return;
    }

    // PGD replicates DDL, so creating it on one node is enough.
    let created = container::exec_ok(
        &host,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
            "-U",
            &cfg.user,
            "-d",
            &cfg.db,
            "-qc",
            "CREATE EXTENSION IF NOT EXISTS pg_stat_statements",
        ],
    );
    if created {
        term::ok("pg_stat_statements ready");
    } else {
        term::warn("could not create the pg_stat_statements extension");
    }
}

/// Wait for Connection Manager to start routing before advertising its ports.
///
/// A node counts as joined as soon as PGD is up, but Connection Manager binds
/// its ports a few seconds later. Printing the endpoints in that window invites
/// a genuinely misleading error: psql reports `No route to host`, which reads
/// like a network fault when in fact the address is fine and nothing is
/// listening on the port yet.
///
/// Never fatal. The cluster is up either way; this only decides whether the
/// endpoints we print work the instant someone pastes them.
fn wait_for_connection_manager(cfg: &Config) {
    let Ok(i) = first_running(cfg) else { return };
    let port = cfg.cm_http(i);

    if monitor::cm_ready_rw(port) {
        return;
    }

    print!("  waiting for Connection Manager ");
    std::io::stdout().flush().ok();
    let mut waited = 0u64;
    while waited < CM_READY_TIMEOUT_SECS {
        if monitor::cm_ready_rw(port) {
            println!(" {}", term::green("routing"));
            return;
        }
        print!(".");
        std::io::stdout().flush().ok();
        sleep(Duration::from_secs(2));
        waited += 2;
    }
    println!();
    term::warn("Connection Manager is not routing yet — the ports below may need a moment");
}

/// Set Connection Manager's pool mode for the node group.
///
/// A group option, not a per-node setting, so it is applied once after the
/// cluster is formed and inherited by every node. Never fatal: a cluster that
/// is up but unpooled is still a usable cluster.
fn apply_pool_mode(cfg: &Config) {
    if cfg.pool_mode.is_empty() {
        return;
    }
    if !matches!(cfg.pool_mode.as_str(), "none" | "session" | "transaction") {
        term::warn(&format!(
            "CIDER_POOL_MODE=\"{}\" is not one of none|session|transaction — leaving pooling alone",
            cfg.pool_mode
        ));
        return;
    }

    let host = cfg.host_name(1);
    if state::pool_mode(cfg, &host).as_deref() == Some(cfg.pool_mode.as_str()) {
        term::ok(&format!(
            "connection pooling: {} (already set)",
            cfg.pool_mode
        ));
        return;
    }

    let applied = container::exec_ok(
        &host,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "pgd",
            "group",
            &cfg.group_name,
            "set-option",
            "server_pool_mode",
            &cfg.pool_mode,
        ],
    );

    // Trust the catalog rather than the exit code.
    match state::pool_mode(cfg, &host) {
        Some(now) if now == cfg.pool_mode => {
            term::ok(&format!("connection pooling: {now}"));
        }
        Some(now) => term::warn(&format!(
            "wanted pool mode \"{}\" but the group reports \"{now}\"",
            cfg.pool_mode
        )),
        None if applied => term::ok(&format!("connection pooling: {}", cfg.pool_mode)),
        None => term::warn("could not set connection pooling mode"),
    }
}

// --- status ----------------------------------------------------------------

/// Show live cluster state, falling back to the container view.
///
/// If PGD cannot be reached, showing which containers exist is still useful —
/// more useful than an error — so this degrades rather than failing.
pub fn status(cfg: &Config) -> Result<()> {
    match state::fetch(cfg) {
        Some(c) => {
            let health = if cfg.monitor {
                monitor::health(cfg.ui_port(1))
            } else {
                None
            };
            state::render(cfg, &c, health);
        }
        None => {
            term::warn("could not read cluster state — falling back to container view");
            println!();
            containers_table(cfg);
        }
    }
    Ok(())
}

/// Raw `container ls` and `volume list`, filtered to this cluster's own
/// containers and volumes.
pub fn containers_table(cfg: &Config) {
    term::info("containers");
    if let Some(out) = container::capture(&["ls", "--all"]) {
        for (n, line) in out.lines().enumerate() {
            if n == 0 || line.starts_with(&cfg.host_prefix) {
                println!("{line}");
            }
        }
    }
    println!();
    term::info("volumes");
    if let Some(out) = container::capture(&["volume", "list"]) {
        for (n, line) in out.lines().enumerate() {
            if n == 0 || line.starts_with(&cfg.volume_prefix) {
                println!("{line}");
            }
        }
    }
}

/// Print every way into the cluster from macOS.
///
/// Ordered by what you most likely want: the write leader first, then
/// load-balanced reads, then the web UI, then the raw per-node grid.
pub fn endpoints(cfg: &Config) {
    term::info("connect");
    // Listed first because it is the most robust: psql runs *inside* a node, so
    // it never traverses macOS networking and cannot be intercepted by a VPN or
    // endpoint-security proxy. It also follows the write leader on its own.
    println!(
        "  {}",
        term::dim("# simplest — runs psql inside a node, so host networking can't interfere")
    );
    println!("  ./cider pgd pour");
    println!();
    println!(
        "  {}",
        term::dim("# write leader, from your own tools on loopback")
    );
    println!(
        "  PGPASSWORD={} psql -h 127.0.0.1 -p {} -U {} {}",
        cfg.password,
        cfg.cm_rw(1),
        cfg.user,
        cfg.db
    );
    // Node 1 is not special: if it is the node that went away, its published
    // port goes with it, and any surviving node's Connection Manager routes to
    // the leader just as well.
    if cfg.nodes > 1 {
        println!(
            "  {}",
            term::dim(&format!(
                "#   any node routes to the leader — :{} and :{} work too",
                cfg.cm_rw(2),
                cfg.cm_rw(cfg.nodes)
            ))
        );
    }
    // Read-only, spread across every node's Connection Manager read-only port.
    // libpq shuffles a multi-host list when load_balance_hosts=random, so
    // sessions land on different read nodes instead of all piling onto the
    // first host in the list.
    println!();
    println!(
        "  {}",
        term::dim("# read-only, load balanced across all nodes (libpq 16+)")
    );
    println!(
        "  PGPASSWORD={} psql \"{}\"",
        cfg.password,
        cfg.read_only_uri()
    );

    if cfg.monitor {
        println!();
        println!(
            "  {}",
            term::dim(&format!(
                "# PGD Monitor web UI (login: {} / {})",
                cfg.user, cfg.password
            ))
        );
        println!("  {}", cfg.ui_url(1));
    }
    println!();
    println!("  {}", term::dim("# per-node"));
    println!(
        "  {:<16}{:<10}{:<10}{:<10}{:<11}web-ui",
        "", "postgres", "cm-rw", "cm-ro", "cm-health"
    );
    for i in 1..=cfg.nodes {
        let ui = if cfg.monitor {
            cfg.ui_url(i)
        } else {
            "disabled".to_string()
        };
        println!(
            "  {:<16}:{:<9}:{:<9}:{:<9}:{:<10}{}",
            cfg.host_name(i),
            cfg.pg_port(i),
            cfg.cm_rw(i),
            cfg.cm_ro(i),
            cfg.cm_http(i),
            ui
        );
    }
    if cfg.resolver_installed() {
        println!();
        // Do not advertise the by-name path without checking it: this is the one
        // route that leaves the host, so a VPN or endpoint-security proxy can
        // block it. Printing a command that fails with "No route to host" sends
        // people hunting for a broken cluster when nothing is wrong.
        let host = cfg.host_fqdn(1);
        if monitor::tcp_reachable(&host, CM_RW_CONTAINER_PORT) {
            println!(
                "  {}",
                term::dim(&format!(
                    "# also by name, since macOS resolves *.{}",
                    cfg.domain
                ))
            );
            println!(
                "  PGPASSWORD={} psql -h {host} -p {CM_RW_CONTAINER_PORT} -U {} {}",
                cfg.password, cfg.user, cfg.db
            );
            if cfg.monitor {
                println!("  http://{host}:{MONITOR_CONTAINER_PORT}/");
            }
            println!(
                "  {}",
                term::dim("#   if these fail with \"No route to host\", a VPN or security")
            );
            println!(
                "  {}",
                term::dim("#   proxy is intercepting them; the 127.0.0.1 ports are unaffected")
            );
        } else {
            println!(
                "  {}",
                term::dim(&format!("# note: {host} is not reachable from this Mac"))
            );
            println!(
                "  {}",
                term::dim("#   usually a VPN or endpoint-security proxy intercepting it.")
            );
            println!(
                "  {}",
                term::dim("#   Your cluster is fine — every 127.0.0.1 port above is unaffected.")
            );
        }
    }
}

// --- web ui ----------------------------------------------------------------

/// Open the PGD Monitor web UI, having first checked it will actually load.
///
/// Two failure modes look identical from the browser and are worth telling
/// apart: the monitor is switched off, or it is running but the published port
/// is not reaching it. This probes for both rather than opening a dead tab.
pub fn ui(cfg: &Config, node: Option<&str>) -> Result<()> {
    let i = cfg.node_index(node);
    let name = cfg.host_name(i);
    let port = cfg.ui_port(i);
    let url = cfg.ui_url(i);

    if container::state(&name) != container::State::Running {
        bail!("{name} is not running — run: cider pgd up");
    }

    if monitor::guc_enabled(cfg, &name) == Some(false) {
        term::bad(&format!(
            "PGD Monitor is disabled on {name} (bdr.monitor_enabled = off)"
        ));
        println!();
        println!("  Enable it now — the setting is reloadable, so no restart:");
        println!(
            "    cider pgd psql {i} -c \"ALTER SYSTEM SET bdr.monitor_enabled='on'\" -c 'SELECT pg_reload_conf()'"
        );
        println!();
        println!("  To make it stick for new clusters, set CIDER_MONITOR=on in .env");
        bail!("monitor disabled");
    }

    print!("  checking {url} ");
    std::io::stdout().flush().ok();
    let mut waited = 0;
    let mut live = false;
    while waited < 20 {
        if monitor::live_from_host(port) {
            println!(" {}", term::green("answering"));
            live = true;
            break;
        }
        print!(".");
        std::io::stdout().flush().ok();
        sleep(Duration::from_secs(2));
        waited += 2;
    }

    if !live {
        println!();
        if monitor::live_in_container(&name) {
            term::bad(&format!(
                "the monitor is up inside {name} but port {port} is not reaching it"
            ));
            term::note("The published port may have been lost. Recreate the node:");
            term::note("  cider pgd down && cider pgd up");
        } else {
            term::bad(&format!("the monitor is not listening inside {name}"));
            term::note("Check what the worker said at startup:");
            term::note(&format!("  cider pgd logs {i} | grep -i 'HTTP.*server'"));
        }
        bail!("web UI not reachable");
    }

    println!();
    println!("  {} — {name}", term::bold("PGD Monitor"));
    println!("  {}", term::bold(&url));
    // Only offer the by-name URL if it actually works from here; see the same
    // check in `endpoints` for why.
    if cfg.resolver_installed() && monitor::tcp_reachable(&cfg.host_fqdn(i), MONITOR_CONTAINER_PORT)
    {
        println!(
            "  {}",
            term::dim(&format!(
                "also http://{}:{MONITOR_CONTAINER_PORT}/ (straight to the node, no port forward)",
                cfg.host_fqdn(i)
            ))
        );
    }
    println!();
    println!(
        "  sign in with   user {}   password {}",
        term::bold(&cfg.user),
        term::bold(&cfg.password)
    );
    println!(
        "  {}",
        term::dim("Any node serves a cluster-wide view, so node 1 is normally enough.")
    );
    println!();

    if std::process::Command::new("open")
        .arg(&url)
        .status()
        .is_ok()
    {
        term::ok("opened in your default browser");
    }
    Ok(())
}

// --- lifecycle -------------------------------------------------------------

/// Stop the node containers, leaving them and their data intact.
pub fn stop(cfg: &Config) -> Result<()> {
    for i in 1..=cfg.nodes {
        let name = cfg.host_name(i);
        if container::state(&name) == container::State::Running {
            if container::quiet_ok(&["stop", &name]) {
                term::ok(&format!("stopped {name}"));
            } else {
                term::warn(&format!("could not stop {name}"));
            }
        }
    }
    Ok(())
}

/// Start previously stopped node containers.
pub fn start(cfg: &Config) -> Result<()> {
    for i in 1..=cfg.nodes {
        let name = cfg.host_name(i);
        match container::state(&name) {
            container::State::Stopped => {
                if container::quiet_ok(&["start", &name]) {
                    term::ok(&format!("started {name}"));
                }
            }
            container::State::Running => term::ok(&format!("{name} already running")),
            container::State::Absent => {
                term::warn(&format!("{name} does not exist — run: cider pgd up"))
            }
        }
    }
    Ok(())
}

/// Remove the containers but keep the volumes.
///
/// `up` afterwards brings back the *same* cluster — same node identities, same
/// data — because the volumes still hold each node's data directory.
pub fn down(cfg: &Config) -> Result<()> {
    term::banner();
    term::info("removing containers (volumes and image are kept)");
    for i in 1..=cfg.nodes {
        let name = cfg.host_name(i);
        match container::state(&name) {
            container::State::Absent => println!("  {}", term::dim(&format!("{name} not present"))),
            _ => {
                let _ = container::quiet_ok(&["stop", &name]);
                if container::quiet_ok(&["delete", &name]) {
                    term::ok(&format!("removed {name}"));
                }
            }
        }
    }
    println!();
    println!("Data is still in the volumes. cider pgd up brings the same cluster back.");
    println!("To discard everything: cider pgd pomace");
    Ok(())
}

/// Destroy everything: containers, volumes, image, optionally the host DNS.
///
/// Irreversible, so it lists exactly what will go and requires the cluster name
/// typed back. `remove_dns` additionally undoes `bootstrap`, and asks again
/// separately — that part touches settings shared with every other container
/// on the machine.
pub fn pomace(cfg: &Config, assume_yes: bool, remove_dns: bool) -> Result<()> {
    term::banner();
    println!("{}", term::yellow("This permanently destroys:"));
    for i in 1..=cfg.nodes {
        println!(
            "  container {:<10} volume {:<20} {}",
            cfg.host_name(i),
            cfg.volume_name(i),
            term::dim("(all its data)")
        );
    }
    println!("  image     {}", cfg.image);
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
            cfg.cluster_name
        ))?;
        if reply != cfg.cluster_name {
            println!("Aborted.");
            return Ok(());
        }
    }

    for i in 1..=cfg.nodes {
        let name = cfg.host_name(i);
        if container::exists(&name) {
            let _ = container::quiet_ok(&["stop", &name]);
            if container::quiet_ok(&["delete", &name]) {
                term::ok(&format!("removed container {name}"));
            }
        }
    }
    for i in 1..=cfg.nodes {
        let vol = cfg.volume_name(i);
        if container::volume_exists(&vol) {
            if container::quiet_ok(&["volume", "delete", &vol]) {
                term::ok(&format!("removed volume {vol}"));
            } else {
                term::warn(&format!("could not remove volume {vol}"));
            }
        }
    }
    if container::image_exists(&cfg.image) {
        if container::quiet_ok(&["image", "delete", &cfg.image]) {
            term::ok(&format!("removed image {}", cfg.image));
        } else {
            term::warn(&format!("could not remove image {}", cfg.image));
        }
    }
    if remove_dns {
        bootstrap::remove_dns_setup(cfg, assume_yes)?;
    }

    println!();
    println!("{}", term::green("All pressed out."));
    Ok(())
}
