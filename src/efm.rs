//! `cider efm`: EDB Postgres Extended under EDB Failover Manager — a primary,
//! two standbys, and an HAProxy load balancer that always reaches the primary.
//!
//! The shared lifecycle — start, wait, retry, stop, tear down — is in
//! `lifecycle`, as for every product. The load balancer is one of the
//! deployment's `extras`: no volume, started after the nodes. This module
//! supplies the rest: how the nodes and the load balancer are created, what
//! ready means for each, a status view, and the ways in.
//!
//! How clients find the primary is the point of the design. Failover Manager
//! 5.4 added an HTTP health endpoint to every agent — 200 on the primary, 404
//! everywhere else — for load balancers to poll. HAProxy polls it every second,
//! so its one published port always reaches the primary, and follows it after a
//! failover, much as PGD's Connection Manager follows the write leader.

use crate::config::{Config, PG_CONTAINER_PORT};
use crate::lifecycle::{self, Deployment};
use crate::{container, term};
use anyhow::{Context, Result, bail};
use std::io::Write;

/// Where each agent's health endpoint listens inside its container.
const HEALTH_CONTAINER_PORT: u16 = 8080;
/// Where HAProxy's stats page listens inside the load balancer's container.
const LB_STATS_CONTAINER_PORT: u16 = 8404;
/// The load balancer's entrypoint, in the same image as the nodes.
const LB_ENTRYPOINT: &str = "/usr/local/bin/cider-press-lb-entrypoint";

/// The EFM cluster, described for the shared lifecycle.
pub fn deployment(cfg: &Config) -> Deployment<'_> {
    Deployment {
        group: "efm",
        title: "EDB Failover Manager",
        cluster_name: &cfg.efm.cluster_name,
        nodes: cfg.efm.nodes,
        host_prefix: &cfg.efm.host_prefix,
        volume_prefix: &cfg.efm.volume_prefix,
        image: &cfg.efm.image,
        domain: &cfg.domain,
        dockerfile: "image/efm.Dockerfile",
        ready_timeout: cfg.ready_timeout,
        extras: vec![cfg.efm.lb_name()],
    }
}

/// Every node's fully-qualified name, comma-separated: what the entrypoints
/// turn into Failover Manager's .nodes file and HAProxy's server list.
fn nodes_csv(d: &Deployment) -> String {
    (1..=d.nodes)
        .map(|i| d.host_fqdn(i))
        .collect::<Vec<_>>()
        .join(",")
}

// --- build -----------------------------------------------------------------

/// Build the image the nodes and the load balancer share. Needs the EDB
/// token, passed as a BuildKit secret exactly as for PGD.
pub fn build(cfg: &Config, no_cache: bool) -> Result<()> {
    if !container::installed() {
        bail!("container is not installed — run: cider doctor");
    }
    cfg.pgd.token.as_deref().context(
        "EDB_SUBSCRIPTION_TOKEN is not set.\n\n  \
         export EDB_SUBSCRIPTION_TOKEN=\"your-token\"\n\n  \
         or put it in .env (gitignored). Get a token at\n  \
         https://www.enterprisedb.com/repos-downloads",
    )?;

    let d = deployment(cfg);
    term::banner_for(d.title);
    term::info(&format!("building {}", d.image));
    println!(
        "  efm      {}  postgres {} (EDB Postgres Extended)",
        cfg.efm.efm_version, cfg.efm.pg_major
    );
    println!("  base     debian:{}-slim (arm64)", cfg.efm.debian_version);
    println!("  token    passed as a BuildKit secret, never stored in the image");
    println!();

    let extra: Vec<String> = vec![
        "--secret".into(),
        "id=edb_token,env=EDB_SUBSCRIPTION_TOKEN".into(),
        "--build-arg".into(),
        format!("PG_MAJOR={}", cfg.efm.pg_major),
        "--build-arg".into(),
        format!("EFM_VERSION={}", cfg.efm.efm_version),
        "--build-arg".into(),
        format!("DEBIAN_VERSION={}", cfg.efm.debian_version),
    ];
    lifecycle::build_image(cfg, &d, &extra, no_cache)
}

// --- probes ----------------------------------------------------------------

/// A node's health endpoint, asked from inside its own container: `Some(200)`
/// on the primary, `Some(404)` on a standby, `None` if the agent is not
/// answering at all.
fn health(name: &str) -> Option<u16> {
    let url = format!("http://127.0.0.1:{HEALTH_CONTAINER_PORT}/");
    container::exec_capture(
        name,
        &[],
        &[
            "curl",
            "-s",
            "-o",
            "/dev/null",
            "-m",
            "2",
            "-w",
            "%{http_code}",
            &url,
        ],
    )
    .and_then(|code| code.trim().parse().ok())
    .filter(|&code| code != 0)
}

/// Whether node `name` is in recovery: `Some(false)` for a primary,
/// `Some(true)` for a standby, `None` if Postgres is not answering.
fn in_recovery(cfg: &Config, name: &str) -> Option<bool> {
    container::exec_capture(
        name,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-X",
            "-h",
            "127.0.0.1",
            "-U",
            &cfg.efm.user,
            "-d",
            &cfg.efm.db,
            "-tAqc",
            "select pg_is_in_recovery()",
        ],
    )
    .and_then(|v| match v.trim() {
        "t" => Some(true),
        "f" => Some(false),
        _ => None,
    })
}

/// The load balancer's backends and their HAProxy state, from its stats page
/// in CSV: `[("maeve-1", "UP"), ("maeve-2", "DOWN"), ...]`.
fn lb_backends(cfg: &Config) -> Option<Vec<(String, String)>> {
    let url = format!("http://127.0.0.1:{LB_STATS_CONTAINER_PORT}/;csv");
    let csv = container::exec_capture(&cfg.efm.lb_name(), &[], &["curl", "-s", "-m", "2", &url])?;
    Some(parse_lb_csv(&csv))
}

/// The server rows of the `efm_primary` backend from HAProxy's CSV stats.
/// Column 0 is the proxy, 1 the server name, 17 the status.
fn parse_lb_csv(csv: &str) -> Vec<(String, String)> {
    csv.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let cols: Vec<&str> = l.split(',').collect();
            let (proxy, server, status) = (cols.first()?, cols.get(1)?, cols.get(17)?);
            (*proxy == "efm_primary" && *server != "BACKEND" && *server != "FRONTEND")
                .then(|| (server.to_string(), status.to_string()))
        })
        .collect()
}

/// The node the load balancer is sending connections to, if any.
fn lb_target(cfg: &Config) -> Option<String> {
    lb_backends(cfg)?
        .into_iter()
        .find(|(_, status)| status == "UP")
        .map(|(server, _)| server)
}

// --- up --------------------------------------------------------------------

/// Is node `i` ready? Postgres answering, and its Failover Manager agent
/// answering on the health endpoint — with either 200 or 404, since both mean
/// the agent is up and knows its role.
fn node_ready(cfg: &Config, i: u16) -> bool {
    let name = deployment(cfg).host_name(i);
    in_recovery(cfg, &name).is_some() && matches!(health(&name), Some(200 | 404))
}

/// Start node `i`: through the shared lifecycle if it already exists,
/// otherwise by creating its container.
fn start_node(cfg: &Config, i: u16) -> Result<()> {
    let d = deployment(cfg);
    if !lifecycle::prepare_node(&d, i)? {
        return Ok(());
    }
    let name = d.host_name(i);
    let fqdn = d.host_fqdn(i);
    let role = if i == 1 { "primary" } else { "standby" };
    term::info(&format!("running {name} ({role}) at {fqdn}"));

    let own_args: Vec<String> = vec![
        "--publish".into(),
        format!("127.0.0.1:{}:{PG_CONTAINER_PORT}", cfg.efm.pg_port(i)),
        "--publish".into(),
        format!(
            "127.0.0.1:{}:{HEALTH_CONTAINER_PORT}",
            cfg.efm.health_port(i)
        ),
        "--env".into(),
        format!("NODE_NAME={name}"),
        "--env".into(),
        format!("NODE_FQDN={fqdn}"),
        "--env".into(),
        format!("EFM_CLUSTER={}", cfg.efm.cluster_name),
        "--env".into(),
        format!("EFM_NODES={}", nodes_csv(&d)),
        // Node 1 is created as the primary and the others are cloned from it.
        // Only ever used when a node is first provisioned: after a failover,
        // which node is primary is Failover Manager's business, not ours.
        "--env".into(),
        format!("EFM_PRIMARY_FQDN={}", d.host_fqdn(1)),
        "--env".into(),
        format!("EFM_IS_FIRST={}", i == 1),
        // What each agent pings to check it is not cut off from the network.
        // Failover Manager wants something that should always be reachable
        // and is not a cluster node; the default, 8.8.8.8, does not answer
        // from here. The load balancer is on the same network, is where
        // clients come from, and starts first.
        "--env".into(),
        format!("EFM_PING_HOST={}.{}", cfg.efm.lb_name(), cfg.domain),
        "--env".into(),
        format!(
            "EFM_AUTO_REJOIN={}",
            if cfg.efm.auto_rejoin { "on" } else { "off" }
        ),
        "--env".into(),
        format!("POSTGRES_DB={}", cfg.efm.db),
        "--env".into(),
        format!("POSTGRES_USER={}", cfg.efm.user),
        "--env".into(),
        format!("PGPASSWORD={}", cfg.password),
        cfg.efm.image.clone(),
    ];
    let mut args = lifecycle::base_run_args(cfg, &d, i);
    args.extend(own_args);
    if !container::run_streaming(&args)? {
        bail!("could not run {name}");
    }
    Ok(())
}

/// Start the load balancer: again if it exists, otherwise create it.
///
/// Not a node, so it has no volume and is not in `CIDER_PEERS` itself; it is
/// given the nodes' list so it can find them in /etc/hosts, a fixed MAC like
/// every container, and the load balancer's entrypoint from the shared image.
fn start_lb(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);
    let name = cfg.efm.lb_name();
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
    term::info(&format!("running {name} (HAProxy, following the primary)"));
    let args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        name.clone(),
        "--cpus".into(),
        "1".into(),
        "--memory".into(),
        "512M".into(),
        "--network".into(),
        format!("default,mac={}", lifecycle::node_mac(&name)),
        "--dns-search".into(),
        cfg.domain.clone(),
        "--env".into(),
        format!("CIDER_PEERS={}", lifecycle::peers_env(&d)),
        "--env".into(),
        format!("EFM_NODES={}", nodes_csv(&d)),
        "--publish".into(),
        format!("127.0.0.1:{}:{PG_CONTAINER_PORT}", cfg.efm.lb_port()),
        "--publish".into(),
        format!("127.0.0.1:{}:{LB_STATS_CONTAINER_PORT}", cfg.efm.ui_port()),
        "--entrypoint".into(),
        LB_ENTRYPOINT.into(),
        cfg.efm.image.clone(),
    ];
    if !container::run_streaming(&args)? {
        bail!("could not run {name}");
    }
    Ok(())
}

/// Create the cluster: the load balancer, then the primary, then each standby
/// cloned from it. The load balancer goes first because every agent pings it
/// at startup; with no primary yet it simply routes nowhere. The nodes go one
/// at a time, because a standby cannot be cloned from a primary that is not
/// there yet.
pub fn up(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);
    lifecycle::preflight(&d)?;
    lifecycle::require_dns_domain(cfg)?;

    term::banner_for(d.title);
    term::info(&format!(
        "pressing a {}-node EFM cluster '{}': a primary, {} standbys, and a load balancer",
        d.nodes,
        d.cluster_name,
        d.nodes - 1
    ));
    println!();

    start_lb(cfg)?;
    let lb = cfg.efm.lb_name();
    lifecycle::wait_for(&d, &lb, &format!("{lb}.{}", cfg.domain), &|| {
        lb_backends(cfg).is_some()
    })?;
    for i in 1..=d.nodes {
        lifecycle::start_and_wait(&d, i, |i| start_node(cfg, i), |i| node_ready(cfg, i))?;
    }
    wait_for_lb_target(cfg)?;

    println!();
    status(cfg)?;
    println!();
    endpoints(cfg);
    Ok(())
}

/// Wait for the load balancer to find the primary: HAProxy checks each
/// agent's endpoint every second, so this is normally immediate.
fn wait_for_lb_target(cfg: &Config) -> Result<()> {
    print!("  load balancer finding the primary ");
    std::io::stdout().flush().ok();
    for _ in 0..30 {
        if let Some(target) = lb_target(cfg) {
            println!(" {}", term::green(&format!("→ {target}")));
            return Ok(());
        }
        print!(".");
        std::io::stdout().flush().ok();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    println!();
    bail!("no node answered 200 on its health endpoint within 30s — see: cider efm logs 1")
}

// --- status ----------------------------------------------------------------

/// Each node's role as Postgres and as Failover Manager see it, and which node
/// the load balancer is sending connections to.
///
/// The two views should agree — a node that is not in recovery answers 200 —
/// and during a failover watching them disagree for a moment is the
/// interesting part. `cider efm cli cluster-status maeve` shows Failover
/// Manager's own, fuller view.
pub fn status(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);
    println!(
        " {} · {} {} · PostgreSQL {} · {} nodes",
        term::bold_amber(d.cluster_name),
        d.title,
        cfg.efm.efm_version,
        cfg.efm.pg_major,
        d.nodes
    );
    println!();
    println!(
        "  {:<12}{:<12}{:<14}HEALTH",
        "NODE", "POSTGRES", "EFM AGENT"
    );
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        if container::state(&name) != container::State::Running {
            let why = match container::state(&name) {
                container::State::Stopped => "stopped",
                _ => "not created",
            };
            println!("  {name:<12}{}", term::dim(why));
            continue;
        }
        let pg = match in_recovery(cfg, &name) {
            Some(false) => term::green("primary"),
            Some(true) => "standby".to_string(),
            None => term::yellow("down"),
        };
        let (agent, code) = match health(&name) {
            Some(200) => (term::green("primary"), "200".to_string()),
            // 404 means only "not the primary": a standby, or an idle agent
            // whose database has failed. `cli cluster-status` says which.
            Some(404) => ("not primary".to_string(), "404".to_string()),
            Some(other) => (term::yellow("?"), other.to_string()),
            None => (term::yellow("not answering"), "—".to_string()),
        };
        // Pad before colouring: escape codes would otherwise count as width.
        println!("  {name:<12}{}{}{code}", pad(&pg, 12), pad(&agent, 14));
    }
    println!();
    match container::state(&cfg.efm.lb_name()) {
        container::State::Running => match lb_target(cfg) {
            Some(target) => println!(
                "  load balancer → {}   (127.0.0.1:{})",
                term::bold(&target),
                cfg.efm.lb_port()
            ),
            None => term::warn("load balancer: no node is answering 200 — no primary to route to"),
        },
        _ => term::warn(&format!(
            "load balancer {} is not running — run: {}",
            cfg.efm.lb_name(),
            d.hint("start")
        )),
    }
    Ok(())
}

/// Left-align `s` in `width` columns, ignoring terminal escape codes.
fn pad(s: &str, width: usize) -> String {
    let visible = strip_ansi(s).chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(visible)))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in s.chars() {
        match (in_escape, c) {
            (false, '\x1b') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

// --- endpoints and ways in -------------------------------------------------

/// Every way into the cluster from macOS, primary first.
pub fn endpoints(cfg: &Config) {
    let d = deployment(cfg);
    term::info("connect");
    println!(
        "  {}",
        term::dim("# simplest — psql to the primary from inside the load balancer")
    );
    println!("  ./cider efm pour");
    println!();
    println!(
        "  {}",
        term::dim("# the primary, through the load balancer — follows it after a failover")
    );
    println!(
        "  PGPASSWORD={} psql -h 127.0.0.1 -p {} -U {} {}",
        cfg.password,
        cfg.efm.lb_port(),
        cfg.efm.user,
        cfg.efm.db
    );
    println!();
    println!(
        "  {}",
        term::dim("# which node is primary right now: HAProxy's stats page")
    );
    println!("  http://127.0.0.1:{}/", cfg.efm.ui_port());
    println!();
    println!("  {}", term::dim("# per node"));
    println!("  {:<12}{:<11}health endpoint", "", "postgres");
    for i in 1..=d.nodes {
        println!(
            "  {:<12}:{:<10}http://127.0.0.1:{}/",
            d.host_name(i),
            cfg.efm.pg_port(i),
            cfg.efm.health_port(i)
        );
    }
    println!();
    term::info("try a failover");
    println!(
        "  {}",
        term::dim("# stop the primary's database; its agent notices and a standby is promoted")
    );
    println!("  ./cider efm shell 1   then:  su postgres -c 'pg_ctl -D $PGDATA -m immediate stop'");
    println!(
        "  ./cider efm status    {}",
        term::dim("# watch the 200 and the load balancer move, in about a minute")
    );
    if cfg.efm.auto_rejoin {
        println!(
            "  {}",
            term::dim("# then the old primary rewinds itself and rejoins as a standby")
        );
    }
    println!("  ./cider efm cli cluster-status {}", cfg.efm.cluster_name);
}

/// Open HAProxy's stats page, having checked it answers.
pub fn ui(cfg: &Config) -> Result<()> {
    let lb = cfg.efm.lb_name();
    if container::state(&lb) != container::State::Running {
        bail!("{lb} is not running — run: cider efm up");
    }
    let url = format!("http://127.0.0.1:{}/", cfg.efm.ui_port());
    if lb_backends(cfg).is_none() {
        bail!("the stats page is not answering inside {lb} — see: cider efm logs lb");
    }
    println!("  {} — which node is primary", term::bold("HAProxy stats"));
    println!("  {}", term::bold(&url));
    println!(
        "  {}",
        term::dim("The primary shows UP; the standbys show DOWN, meaning only \"not primary\".")
    );
    println!();
    std::io::stdout().flush().ok();
    if std::process::Command::new("open")
        .arg(&url)
        .status()
        .is_ok()
    {
        term::ok("opened in your default browser");
    }
    Ok(())
}

/// psql to the primary, through the load balancer, from inside its container
/// — so macOS networking cannot interfere, as with `pgd pour`.
pub fn pour(cfg: &Config) -> Result<()> {
    let lb = cfg.efm.lb_name();
    if container::state(&lb) != container::State::Running {
        bail!("{lb} is not running — run: cider efm up");
    }
    match lb_target(cfg) {
        Some(target) => term::info(&format!("pouring into the primary ({target}) via {lb}")),
        None => term::warn("no node is primary right now; psql will fail until one is"),
    }
    let cmd: Vec<String> = vec![
        "psql".into(),
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        PG_CONTAINER_PORT.to_string(),
        "-U".into(),
        cfg.efm.user.clone(),
        "-d".into(),
        cfg.efm.db.clone(),
    ];
    container::exec_interactive(&lb, &[("PGPASSWORD", &cfg.password)], &cmd)
}

/// Run Failover Manager's own CLI on the first running node, e.g.
/// `cider efm cli cluster-status maeve`.
pub fn cli(cfg: &Config, args: &[String]) -> Result<()> {
    let d = deployment(cfg);
    let i = lifecycle::first_running(&d)?;
    let mut cmd: Vec<String> = vec![format!("/usr/edb/efm-{}/bin/efm", cfg.efm.efm_version)];
    cmd.extend(args.iter().cloned());
    container::exec_interactive(&d.host_name(i), &[], &cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_primary_backend_from_haproxy_csv() {
        // Trimmed HAProxy stats CSV: proxy, server, 15 columns, then status
        // as the 18th field (index 17).
        let row = |proxy: &str, server: &str, status: &str| {
            format!("{proxy},{server},{}{status},", ",".repeat(15))
        };
        let csv = [
            "# pxname,svname,...".to_string(),
            row("primary", "FRONTEND", "OPEN"),
            row("efm_primary", "maeve-1", "DOWN"),
            row("efm_primary", "maeve-2", "UP"),
            row("efm_primary", "maeve-3", "DOWN"),
            row("efm_primary", "BACKEND", "UP"),
            row("stats", "FRONTEND", "OPEN"),
        ]
        .join("\n");
        let backends = parse_lb_csv(&csv);
        assert_eq!(backends.len(), 3);
        assert_eq!(
            backends
                .iter()
                .find(|(_, s)| s == "UP")
                .map(|(n, _)| n.as_str()),
            Some("maeve-2")
        );
    }

    #[test]
    fn padding_ignores_colour_codes() {
        let coloured = "\x1b[32mprimary\x1b[0m";
        assert_eq!(strip_ansi(&pad(coloured, 10)), "primary   ");
    }
}
