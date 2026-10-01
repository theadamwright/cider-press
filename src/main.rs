//! cider — press an EDB Postgres Distributed cluster out of Apple's
//! `container` runtime.
//!
//! # Layout
//!
//! - [`config`]    every tunable, resolved once from env/`.env`; naming and port maths
//! - [`container`] the only module that shells out to `container` or parses its output
//! - [`doctor`]    preflight checks, in the order they matter
//! - [`bootstrap`] one-time host setup: the container DNS domain and macOS resolver
//! - [`efm`]       `cider efm`: EDB Failover Manager, a primary, two standbys and HAProxy
//! - [`lifecycle`] what every product shares: build, start-and-wait, stop, teardown
//! - [`logical`]   `cider logical`: a community PostgreSQL pair for logical replication
//! - [`pgd`]       PGD's half of the verbs: its containers, readiness, endpoints, web UI
//! - [`state`]     live cluster state, read through the `pgd` CLI's JSON output
//! - [`monitor`]   PGD Monitor probes (the 6.5 web UI)
//! - [`term`]      colour, glyphs, banner
//!
//! # Command grammar
//!
//! `cider <group> <verb>`, matching EDB's own CLIs (`pgd node setup`). `doctor`
//! and `bootstrap` sit at the top level because they configure the Mac rather
//! than a cluster; everything that touches a cluster lives under `pgd`.
//!
//! The `pgd` group is deliberate even though PGD is the only product here. It
//! keeps host-level setup and cluster-level work visibly separate, and it
//! leaves room for a second product without a breaking rename later.
//!
//! Each product's verbs are its own enum ([`PgdVerb`]) with the verbs every
//! product shares ([`SharedVerb`]) flattened in, so `cider <group> --help`
//! lists exactly what that product can do and nothing it can't.

mod bootstrap;
mod config;
mod container;
mod doctor;
mod efm;
mod lifecycle;
mod logical;
mod monitor;
mod pgd;
mod state;
mod term;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{CM_RW_CONTAINER_PORT, Config, PG_CONTAINER_PORT};
use lifecycle::Deployment;

#[derive(Parser)]
#[command(
    name = "cider",
    version,
    about = "EDB clusters, pressed on Apple container",
    long_about = None,
    disable_help_subcommand = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Check macOS, container, DNS, token and images
    Doctor,
    /// One-time host setup: container DNS domain + macOS resolver
    Bootstrap {
        /// Do not prompt for confirmation
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// EDB Postgres Distributed — an active-active cluster
    #[command(subcommand)]
    Pgd(PgdVerb),
    /// Two community PostgreSQL nodes, for logical replication you wire up
    #[command(subcommand)]
    Logical(LogicalVerb),
    /// EDB Failover Manager — a primary, two standbys and a load balancer
    #[command(subcommand)]
    Efm(EfmVerb),
}

/// Everything you can do to the EFM cluster.
///
/// `pour` goes through the load balancer, which follows the primary, as `pgd
/// pour` goes through Connection Manager. `ui` is HAProxy's stats page. `cli`
/// is Failover Manager's own `efm` command.
#[derive(Subcommand)]
enum EfmVerb {
    /// Build the image (needs EDB_SUBSCRIPTION_TOKEN)
    Build {
        /// Rebuild without using cached layers
        #[arg(long)]
        no_cache: bool,
    },
    /// Create the primary, the standbys and the load balancer
    #[command(alias = "press")]
    Up,
    /// Each node's role, and where the load balancer is routing
    #[command(alias = "ps")]
    Status,
    /// Every published port, and how to try a failover
    Endpoints,
    /// Open HAProxy's stats page: which node is primary
    #[command(alias = "web")]
    Ui,
    /// psql to the primary through the load balancer
    Pour,
    /// psql directly to a node (default 1)
    Psql {
        /// Node, as "2" or "maeve-2"
        node: Option<String>,
        /// Extra arguments passed to psql
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Failover Manager's efm command, e.g. cider efm cli cluster-status
    Cli {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(flatten)]
    Shared(SharedVerb),
}

/// Verbs every product has *and implements the same way* — one function in
/// [`lifecycle`] or a helper below serves them all.
///
/// Flattened into each product's own verb enum, so they appear in its `--help`
/// as if declared there. The test for belonging here is the implementation,
/// not the name: `up` and `status` exist for every product, but each does them
/// differently, so each product declares its own.
#[derive(Subcommand)]
enum SharedVerb {
    /// Containers and volumes belonging to this cluster
    Containers,
    /// Stop the node containers
    Stop,
    /// Restart stopped node containers
    Start,
    /// Remove containers, keep volumes (data survives)
    Down,
    /// Destroy containers, volumes and image. Irreversible
    #[command(alias = "destroy")]
    Pomace {
        /// Do not prompt for confirmation
        #[arg(short = 'y', long = "yes")]
        yes: bool,
        /// Also remove the host DNS setup that bootstrap created
        #[arg(long)]
        dns: bool,
    },
    /// bash inside a node container
    #[command(alias = "sh")]
    Shell {
        /// Node, as "2" or "host-2"
        node: Option<String>,
    },
    /// Container logs
    Logs {
        /// Node, as "2" or "host-2"
        node: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Everything you can do to a PGD cluster.
///
/// PGD's own verbs, then the shared ones. Each arm maps to one function, in
/// [`pgd`] or a helper below. Adding a verb means adding an arm here and a
/// function there — or, if every product would do it the same way, an arm on
/// [`SharedVerb`] instead.
#[derive(Subcommand)]
enum PgdVerb {
    /// Build the node image (needs EDB_SUBSCRIPTION_TOKEN)
    Build {
        /// Rebuild without using cached layers
        #[arg(long)]
        no_cache: bool,
    },
    /// Create volumes and press the cluster
    #[command(alias = "press")]
    Up,
    /// Live cluster state
    #[command(alias = "ps")]
    Status,
    /// Every host port this cluster publishes
    Endpoints,
    /// Open the PGD Monitor web UI
    #[command(alias = "web", alias = "monitor")]
    Ui {
        /// Node, as "2" or "host-2"
        node: Option<String>,
    },
    /// psql to the write leader via Connection Manager
    Pour,
    /// psql directly to a node (default 1)
    Psql {
        /// Node, as "2" or "host-2"
        node: Option<String>,
        /// Extra arguments passed to psql
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the PGD CLI against the write leader, e.g. cider pgd cli cluster show
    Cli {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(flatten)]
    Shared(SharedVerb),
}

/// Everything you can do to the logical-replication pair.
///
/// No `pour`, `cli` or `ui`: there is no leader to route to, no product CLI and
/// no web UI. The replication itself is yours to create; `endpoints` prints
/// the SQL.
#[derive(Subcommand)]
enum LogicalVerb {
    /// Build the node image (no token needed)
    Build {
        /// Rebuild without using cached layers
        #[arg(long)]
        no_cache: bool,
    },
    /// Create volumes and press the pair
    #[command(alias = "press")]
    Up,
    /// Publications, subscriptions and apply errors on each node
    #[command(alias = "ps")]
    Status,
    /// How to connect, and the SQL to wire up replication
    Endpoints,
    /// psql to a node (default 1)
    Psql {
        /// Node, as "2" or "dolores-2"
        node: Option<String>,
        /// Extra arguments passed to psql
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(flatten)]
    Shared(SharedVerb),
}

fn main() {
    restore_default_sigpipe();
    if let Err(e) = run() {
        eprintln!("{} {e}", term::red("error:"));
        std::process::exit(1);
    }
}

/// Parse arguments, load config, dispatch. Errors print and exit non-zero.
/// Die quietly when our output is closed early.
///
/// Rust sets `SIGPIPE` to `SIG_IGN` at startup, so a closed pipe surfaces as a
/// write error and `println!` panics — meaning `cider pgd endpoints | head`
/// ends in a Rust backtrace instead of just stopping. Restoring the default
/// handler makes this behave like every other Unix tool.
fn restore_default_sigpipe() {
    // SAFETY: setting a signal disposition to SIG_DFL before any threads are
    // spawned. This is the documented remedy for Rust's SIGPIPE default.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();

    match cli.command {
        Top::Doctor => doctor::run(&cfg),
        Top::Bootstrap { yes } => bootstrap::run(&cfg, yes),
        Top::Pgd(verb) => run_pgd(&cfg, verb),
        Top::Logical(verb) => run_logical(&cfg, verb),
        Top::Efm(verb) => run_efm(&cfg, verb),
    }
}

/// Dispatch an EFM verb. One arm per [`EfmVerb`].
fn run_efm(cfg: &Config, verb: EfmVerb) -> Result<()> {
    let d = efm::deployment(cfg);
    match verb {
        EfmVerb::Build { no_cache } => efm::build(cfg, no_cache),
        EfmVerb::Up => efm::up(cfg),
        EfmVerb::Status => efm::status(cfg),
        EfmVerb::Endpoints => {
            efm::endpoints(cfg);
            Ok(())
        }
        EfmVerb::Ui => efm::ui(cfg),
        EfmVerb::Pour => efm::pour(cfg),
        EfmVerb::Psql { node, args } => {
            psql(cfg, &d, &cfg.efm.user, &cfg.efm.db, node.as_deref(), &args)
        }
        EfmVerb::Cli { args } => efm::cli(cfg, &args),
        EfmVerb::Shared(verb) => run_shared(cfg, &d, verb),
    }
}

/// Dispatch a logical verb. One arm per [`LogicalVerb`].
fn run_logical(cfg: &Config, verb: LogicalVerb) -> Result<()> {
    let d = logical::deployment(cfg);
    match verb {
        LogicalVerb::Build { no_cache } => logical::build(cfg, no_cache),
        LogicalVerb::Up => logical::up(cfg),
        LogicalVerb::Status => logical::status(cfg),
        LogicalVerb::Endpoints => {
            logical::endpoints(cfg);
            Ok(())
        }
        LogicalVerb::Psql { node, args } => psql(
            cfg,
            &d,
            &cfg.logical.user,
            &cfg.logical.db,
            node.as_deref(),
            &args,
        ),
        LogicalVerb::Shared(verb) => run_shared(cfg, &d, verb),
    }
}

/// Dispatch a PGD verb. One arm per [`PgdVerb`], and nothing else lives here.
fn run_pgd(cfg: &Config, verb: PgdVerb) -> Result<()> {
    let d = pgd::deployment(cfg);
    match verb {
        PgdVerb::Build { no_cache } => pgd::build(cfg, no_cache),
        PgdVerb::Up => pgd::up(cfg),
        PgdVerb::Status => pgd::status(cfg),
        PgdVerb::Endpoints => {
            pgd::endpoints(cfg);
            Ok(())
        }
        PgdVerb::Ui { node } => pgd::ui(cfg, node.as_deref()),
        PgdVerb::Pour => pour(cfg),
        PgdVerb::Psql { node, args } => {
            psql(cfg, &d, &cfg.pgd.user, &cfg.pgd.db, node.as_deref(), &args)
        }
        PgdVerb::Cli { args } => pgd_cli(cfg, &args),
        PgdVerb::Shared(verb) => run_shared(cfg, &d, verb),
    }
}

/// Dispatch a shared verb for any product. One arm per [`SharedVerb`].
fn run_shared(cfg: &Config, d: &Deployment, verb: SharedVerb) -> Result<()> {
    match verb {
        SharedVerb::Containers => {
            lifecycle::containers_table(d);
            Ok(())
        }
        SharedVerb::Stop => lifecycle::stop(d),
        SharedVerb::Start => lifecycle::start(d),
        SharedVerb::Down => lifecycle::down(d),
        SharedVerb::Pomace { yes, dns } => lifecycle::pomace(cfg, d, yes, dns),
        SharedVerb::Shell { node } => shell(cfg, d, node.as_deref()),
        SharedVerb::Logs { node, args } => {
            container::logs(&d.container_for(node.as_deref()), &args)
        }
    }
}

/// Assert a *specific* node is running. Used by the commands where the node is
/// the point — `psql 2`, `shell 3` — never by the ones that just need a way in.
fn require_running(d: &Deployment, i: u16) -> Result<String> {
    let name = d.host_name(i);
    if container::state(&name) != container::State::Running {
        anyhow::bail!("{name} is not running — run: {}", d.hint("up"));
    }
    Ok(name)
}

/// psql to the write leader, entering through whichever node is up.
fn pour(cfg: &Config) -> Result<()> {
    let d = pgd::deployment(cfg);
    let i = pgd::first_running(cfg)?;
    let name = d.host_name(i);
    term::info(&format!(
        "pouring into the write leader via Connection Manager on {}:{CM_RW_CONTAINER_PORT}",
        d.host_fqdn(i)
    ));
    let cmd: Vec<String> = vec![
        "psql".into(),
        "-h".into(),
        d.host_fqdn(i),
        "-p".into(),
        CM_RW_CONTAINER_PORT.to_string(),
        "-U".into(),
        cfg.pgd.user.clone(),
        "-d".into(),
        cfg.pgd.db.clone(),
    ];
    container::exec_interactive(&name, &[("PGPASSWORD", &cfg.password)], &cmd)
}

/// psql to one specific node, straight to its own Postgres.
///
/// Use this when the node is the point — checking replication has arrived on
/// node 3, say. For PGD, ordinary work wants [`pour`] instead, which goes
/// through Connection Manager to the leader.
///
/// Written for any product: every node image runs Postgres on
/// [`PG_CONTAINER_PORT`] inside its container, and the caller supplies the
/// product's own user and database.
fn psql(
    cfg: &Config,
    d: &Deployment,
    user: &str,
    db: &str,
    node: Option<&str>,
    extra: &[String],
) -> Result<()> {
    let i = d.node_index(node);
    let name = require_running(d, i)?;
    let mut cmd: Vec<String> = vec![
        "psql".into(),
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        PG_CONTAINER_PORT.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        db.into(),
    ];
    cmd.extend(extra.iter().cloned());
    container::exec_interactive(&name, &[("PGPASSWORD", &cfg.password)], &cmd)
}

/// Run the PGD CLI against the write leader.
///
/// The binary already lives in the node image, so this needs nothing installed
/// on your Mac and no shell inside a container — `container exec` runs it in
/// place and streams the output back.
///
/// The DSN points at Connection Manager's read-write port rather than a node's
/// own 5432, which is what makes this the `pour` of the CLI world: CM routes it
/// to whichever node currently holds write leadership, so the command follows
/// the leader across a failover without you tracking it.
///
/// It is passed as `PGD_CLI_DSN` rather than `--dsn` deliberately. The env var
/// is the documented equivalent, and it acts as a *default* — a `--dsn` of your
/// own on the command line still wins, instead of colliding with an injected
/// flag.
fn pgd_cli(cfg: &Config, extra: &[String]) -> Result<()> {
    let d = pgd::deployment(cfg);
    let i = pgd::first_running(cfg)?;
    let name = d.host_name(i);
    let dsn = format!(
        "host={} port={CM_RW_CONTAINER_PORT} dbname={} user={}",
        d.host_fqdn(i),
        cfg.pgd.db,
        cfg.pgd.user
    );

    let mut cmd: Vec<String> = vec!["pgd".into()];
    cmd.extend(extra.iter().cloned());
    container::exec_interactive(
        &name,
        &[("PGPASSWORD", &cfg.password), ("PGD_CLI_DSN", &dsn)],
        &cmd,
    )
}

/// An interactive bash shell inside a node container, for any product.
fn shell(cfg: &Config, d: &Deployment, node: Option<&str>) -> Result<()> {
    let name = d.container_for(node);
    if container::state(&name) != container::State::Running {
        anyhow::bail!("{name} is not running — run: {}", d.hint("up"));
    }
    container::exec_interactive(
        &name,
        &[("PGPASSWORD", &cfg.password)],
        &["bash".to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Parse a command line the way the binary would, or panic with clap's
    /// own error so a failure says what was wrong.
    fn parse(line: &str) -> Top {
        Cli::try_parse_from(line.split_whitespace())
            .unwrap_or_else(|e| panic!("`{line}` did not parse: {e}"))
            .command
    }

    // clap's own consistency check. Flattening one enum into another is where
    // a duplicate verb name or a clashing alias would slip in, and clap only
    // reports those when the command is built — this makes it a test failure
    // instead of a runtime one.
    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    // Every verb and alias, as people type them. These are in shell history
    // and in the README, so a refactor that loses one is a breaking change.
    #[test]
    fn every_pgd_verb_and_alias_still_parses() {
        for line in [
            "cider pgd build --no-cache",
            "cider pgd up",
            "cider pgd press",
            "cider pgd status",
            "cider pgd ps",
            "cider pgd endpoints",
            "cider pgd ui 2",
            "cider pgd web",
            "cider pgd monitor",
            "cider pgd pour",
            "cider pgd psql host-2 -c select",
            "cider pgd cli nodes list -o json",
            "cider pgd containers",
            "cider pgd stop",
            "cider pgd start",
            "cider pgd down",
            "cider pgd pomace -y --dns",
            "cider pgd destroy",
            "cider pgd shell 3",
            "cider pgd sh",
            "cider pgd logs 2 --follow",
        ] {
            assert!(matches!(parse(line), Top::Pgd(_)), "{line}");
        }
    }

    #[test]
    fn shared_verbs_arrive_through_the_flattened_arm() {
        assert!(matches!(
            parse("cider pgd pomace -y"),
            Top::Pgd(PgdVerb::Shared(SharedVerb::Pomace {
                yes: true,
                dns: false
            }))
        ));
        assert!(matches!(
            parse("cider pgd sh 2"),
            Top::Pgd(PgdVerb::Shared(SharedVerb::Shell { node: Some(_) }))
        ));
    }

    // Arguments after the node belong to psql, including ones that look like
    // flags of our own.
    #[test]
    fn psql_passes_trailing_arguments_through() {
        match parse("cider pgd psql 2 -tAc select") {
            Top::Pgd(PgdVerb::Psql { node, args }) => {
                assert_eq!(node.as_deref(), Some("2"));
                assert_eq!(args, ["-tAc", "select"]);
            }
            _ => panic!("psql did not parse as psql"),
        }
    }

    #[test]
    fn every_logical_verb_and_alias_parses() {
        for line in [
            "cider logical build --no-cache",
            "cider logical up",
            "cider logical press",
            "cider logical status",
            "cider logical ps",
            "cider logical endpoints",
            "cider logical psql 2 -c select",
            "cider logical containers",
            "cider logical stop",
            "cider logical start",
            "cider logical down",
            "cider logical pomace -y",
            "cider logical shell 1",
            "cider logical logs 2",
        ] {
            assert!(matches!(parse(line), Top::Logical(_)), "{line}");
        }
    }

    #[test]
    fn every_efm_verb_and_alias_parses() {
        for line in [
            "cider efm build --no-cache",
            "cider efm up",
            "cider efm press",
            "cider efm status",
            "cider efm ps",
            "cider efm endpoints",
            "cider efm ui",
            "cider efm web",
            "cider efm pour",
            "cider efm psql 2 -c select",
            "cider efm cli cluster-status",
            "cider efm containers",
            "cider efm stop",
            "cider efm start",
            "cider efm down",
            "cider efm pomace -y",
            "cider efm shell lb",
            "cider efm logs lb",
        ] {
            assert!(matches!(parse(line), Top::Efm(_)), "{line}");
        }
    }

    // The point of per-product verb enums: `cider logical --help` offers only
    // what the pair can do, and PGD's own verbs are rejected, not accepted and
    // then failing.
    #[test]
    fn logical_rejects_pgds_own_verbs() {
        for line in [
            "cider logical ui",
            "cider logical pour",
            "cider logical cli nodes list",
        ] {
            assert!(
                Cli::try_parse_from(line.split_whitespace()).is_err(),
                "{line} should not parse"
            );
        }
    }
}
