//! cider — press an EDB Postgres Distributed cluster out of Apple's
//! `container` runtime.
//!
//! # Layout
//!
//! - [`config`]    every tunable, resolved once from env/`.env`; naming and port maths
//! - [`container`] the only module that shells out to `container` or parses its output
//! - [`doctor`]    preflight checks, in the order they matter
//! - [`bootstrap`] one-time host setup: the container DNS domain and macOS resolver
//! - [`cluster`]   build, up, status, teardown — the verbs themselves
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
//! leaves room for a second product (EFM was evaluated and is viable) without a
//! breaking rename later. [`Verb`] is therefore written to be product-agnostic:
//! a second group would reuse it rather than invent its own grammar.

mod bootstrap;
mod cluster;
mod config;
mod container;
mod doctor;
mod monitor;
mod state;
mod term;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{CM_RW_CONTAINER_PORT, Config};

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
    Pgd(Verb),
}

/// Everything you can do to a cluster.
///
/// Each arm maps to one function in [`cluster`] (or a small helper below for
/// the ones that just shell into a container), so adding a verb means adding
/// an arm here and a function there — nothing else.
#[derive(Subcommand)]
enum Verb {
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
    /// Containers and volumes belonging to this cluster
    Containers,
    /// Every host port this cluster publishes
    Endpoints,
    /// Open the PGD Monitor web UI
    #[command(alias = "web", alias = "monitor")]
    Ui {
        /// Node, as "2" or "host-2"
        node: Option<String>,
    },
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
    /// Run the product's own CLI, e.g. cider pgd cli cluster show
    Cli {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    }
}

/// Dispatch a cluster verb. One arm per [`Verb`], and nothing else lives here.
fn run_pgd(cfg: &Config, verb: Verb) -> Result<()> {
    match verb {
        Verb::Build { no_cache } => cluster::build(cfg, no_cache),
        Verb::Up => cluster::up(cfg),
        Verb::Status => cluster::status(cfg),
        Verb::Containers => {
            cluster::containers_table(cfg);
            Ok(())
        }
        Verb::Endpoints => {
            cluster::endpoints(cfg);
            Ok(())
        }
        Verb::Ui { node } => cluster::ui(cfg, node.as_deref()),
        Verb::Stop => cluster::stop(cfg),
        Verb::Start => cluster::start(cfg),
        Verb::Down => cluster::down(cfg),
        Verb::Pomace { yes, dns } => cluster::pomace(cfg, yes, dns),
        Verb::Pour => pour(cfg),
        Verb::Psql { node, args } => psql(cfg, node.as_deref(), &args),
        Verb::Cli { args } => pgd_cli(cfg, &args),
        Verb::Shell { node } => shell(cfg, node.as_deref()),
        Verb::Logs { node, args } => {
            let i = cfg.node_index(node.as_deref());
            container::logs(&cfg.host_name(i), &args)
        }
    }
}

/// Assert a *specific* node is running. Used by the commands where the node is
/// the point — `psql 2`, `shell 3` — never by the ones that just need a way in.
fn require_running(cfg: &Config, i: u16) -> Result<String> {
    let name = cfg.host_name(i);
    if container::state(&name) != container::State::Running {
        anyhow::bail!("{name} is not running — run: cider pgd up");
    }
    Ok(name)
}

/// psql to the write leader, entering through whichever node is up.
fn pour(cfg: &Config) -> Result<()> {
    let i = cluster::first_running(cfg)?;
    let name = cfg.host_name(i);
    term::info(&format!(
        "pouring into the write leader via Connection Manager on {}:{CM_RW_CONTAINER_PORT}",
        cfg.host_fqdn(i)
    ));
    let cmd: Vec<String> = vec![
        "psql".into(),
        "-h".into(),
        cfg.host_fqdn(i),
        "-p".into(),
        CM_RW_CONTAINER_PORT.to_string(),
        "-U".into(),
        cfg.user.clone(),
        "-d".into(),
        cfg.db.clone(),
    ];
    container::exec_interactive(&name, &[("PGPASSWORD", &cfg.password)], &cmd)
}

/// psql to one specific node, bypassing Connection Manager's routing.
///
/// Use this when the node is the point — checking replication has arrived on
/// node 3, say. For ordinary work you want [`pour`], which finds the leader.
fn psql(cfg: &Config, node: Option<&str>, extra: &[String]) -> Result<()> {
    let i = cfg.node_index(node);
    let name = require_running(cfg, i)?;
    let mut cmd: Vec<String> = vec![
        "psql".into(),
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        "5432".into(),
        "-U".into(),
        cfg.user.clone(),
        "-d".into(),
        cfg.db.clone(),
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
    let i = cluster::first_running(cfg)?;
    let name = cfg.host_name(i);
    let dsn = format!(
        "host={} port={CM_RW_CONTAINER_PORT} dbname={} user={}",
        cfg.host_fqdn(i),
        cfg.db,
        cfg.user
    );

    let mut cmd: Vec<String> = vec!["pgd".into()];
    cmd.extend(extra.iter().cloned());
    container::exec_interactive(
        &name,
        &[("PGPASSWORD", &cfg.password), ("PGD_CLI_DSN", &dsn)],
        &cmd,
    )
}

/// An interactive bash shell inside a node container.
fn shell(cfg: &Config, node: Option<&str>) -> Result<()> {
    let i = cfg.node_index(node);
    let name = require_running(cfg, i)?;
    container::exec_interactive(
        &name,
        &[("PGPASSWORD", &cfg.password)],
        &["bash".to_string()],
    )
}
