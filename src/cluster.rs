//! PGD: what is specific to pressing an EDB Postgres Distributed cluster.
//!
//! The shared lifecycle — start, wait, retry, stop, tear down — is in
//! `lifecycle`. This module supplies PGD's half: how a node's container is
//! created, how to tell it has joined, and everything that happens once the
//! cluster exists (pooling, `pg_stat_statements`, Connection Manager, the web
//! UI, the endpoints).

use crate::config::{
    CM_HTTP_CONTAINER_PORT, CM_RO_CONTAINER_PORT, CM_RW_CONTAINER_PORT, Config,
    MONITOR_CONTAINER_PORT, PG_CONTAINER_PORT,
};
use crate::lifecycle::{self, Deployment};
use crate::{container, monitor, state, term};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

/// How long to wait for Connection Manager after the last node joins. Short:
/// this is a courtesy, not a health check, and `up` succeeds regardless.
const CM_READY_TIMEOUT_SECS: u64 = 60;

/// PGD's nodes, described for the shared lifecycle.
///
/// Built from `Config` on every call rather than stored: it only borrows, so
/// it costs nothing, and there is then no second copy of a setting to go
/// stale.
pub fn deployment(cfg: &Config) -> Deployment<'_> {
    Deployment {
        group: "pgd",
        cluster_name: &cfg.cluster_name,
        nodes: cfg.nodes,
        host_prefix: &cfg.host_prefix,
        volume_prefix: &cfg.volume_prefix,
        image: &cfg.image,
        dockerfile: "image/Dockerfile",
        ready_timeout: cfg.ready_timeout,
    }
}

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

    // --secret ...,env=VAR reads straight from this process's environment, so
    // the token never touches disk and never lands in a layer or the history.
    let extra: Vec<String> = vec![
        "--secret".into(),
        "id=edb_token,env=EDB_SUBSCRIPTION_TOKEN".into(),
        "--build-arg".into(),
        format!("PG_FLAVOR={}", cfg.pg_flavor),
        "--build-arg".into(),
        format!("PG_MAJOR={}", cfg.pg_major),
        "--build-arg".into(),
        format!("DEBIAN_VERSION={}", cfg.debian_version),
    ];
    lifecycle::build_image(cfg, &deployment(cfg), &extra, no_cache)
}

/// Index of the first running node — the way in for anything that just needs
/// *a* node.
///
/// For PGD, entering through any node is equivalent: every node runs its own
/// Connection Manager and every one of them routes to the current write
/// leader.
pub fn first_running(cfg: &Config) -> Result<u16> {
    lifecycle::first_running(&deployment(cfg))
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

/// Start node `i`: through the shared lifecycle if it already exists,
/// otherwise by creating its container with PGD's ports and environment.
///
/// Everything the entrypoint needs is passed as environment variables — the
/// image itself holds no per-node configuration.
fn start_node(cfg: &Config, i: u16) -> Result<()> {
    let d = deployment(cfg);
    if !lifecycle::prepare_node(&d, i)? {
        return Ok(());
    }

    let name = cfg.host_name(i);
    let fqdn = cfg.host_fqdn(i);
    let node = cfg.node_name(i);
    term::info(&format!("running {name} ({node}) at {fqdn}"));

    let monitor_flag = if cfg.monitor { "on" } else { "off" };
    let pgd_args: Vec<String> = vec![
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
    let mut args = lifecycle::base_run_args(cfg, &d, i);
    args.extend(pgd_args);

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
/// The precondition checks are shared (`lifecycle::preflight`); getting any of
/// them wrong produces failures minutes later inside a container.
pub fn up(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);
    lifecycle::preflight(&d)?;
    lifecycle::require_dns_domain(cfg)?;

    term::banner();
    term::info(&format!(
        "pressing a {}-node PGD cluster '{}'",
        cfg.nodes, cfg.cluster_name
    ));
    println!();

    // Node 1 creates the cluster; the rest join it. Serialised on purpose —
    // PGD joins are not safe to run concurrently against a fresh cluster.
    for i in 1..=cfg.nodes {
        lifecycle::start_and_wait(
            &d,
            i,
            |i| start_node(cfg, i),
            |i| node_joined(cfg, &cfg.host_name(i), &cfg.node_name(i)),
        )?;
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
            lifecycle::containers_table(&deployment(cfg));
        }
    }
    Ok(())
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
