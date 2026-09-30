//! Configuration, resolved once from the environment (and an optional `.env`).
//!
//! Shell environment wins over `.env`, so `CIDER_NODES=5 cider pgd up` is a
//! one-shot override without editing the file.
//!
//! Settings are split by who owns them. [`Config`] holds what every product
//! shares — the DNS domain, node sizing, the lab password, the pinned OS.
//! [`PgdConfig`], at `cfg.pgd`, holds what only PGD uses. A second product
//! gets its own struct alongside it rather than more fields on `Config`.

use std::env;
use std::path::PathBuf;

// Ports *inside* a PGD node container. These are fixed by PGD and Postgres,
// not by us; the host-side published ports are computed per node further down.

/// Postgres itself.
pub const PG_CONTAINER_PORT: u16 = 5432;
/// Connection Manager, read-write — routed to the current write leader.
pub const CM_RW_CONTAINER_PORT: u16 = 6432;
/// Connection Manager, read-only — routed across read nodes.
pub const CM_RO_CONTAINER_PORT: u16 = 6433;
/// Connection Manager health/JSON API. Not a browsable UI.
pub const CM_HTTP_CONTAINER_PORT: u16 = 6434;
/// PGD Monitor's web UI, REST API and `/metrics`.
///
/// Postgres port + 1005, per the PGD Monitor documentation:
/// <https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/pgd-monitor/>
pub const MONITOR_CONTAINER_PORT: u16 = 6437;

/// Settings every product shares.
pub struct Config {
    /// The project directory, where `image/` and `.env` live.
    pub root: PathBuf,

    /// The container DNS domain every node is registered under, so `host-1`
    /// is reachable as `host-1.cider`. One per Mac, shared by every product.
    pub domain: String,
    /// The lab password for every database superuser. It is `secret`, it is
    /// in the README, and that is deliberate: see "What this is for".
    pub password: String,
    /// Debian major for every image. Pinned; see the Dockerfile for why.
    pub debian_version: String,

    /// Resources for each node container.
    pub cpus: String,
    pub memory: String,
    /// Resources for the build container, which needs more than a node.
    pub build_cpus: String,
    pub build_memory: String,
    /// How long `up` waits for one node to become ready, in seconds.
    pub ready_timeout: u64,

    pub pgd: PgdConfig,
}

/// Settings only PGD uses.
pub struct PgdConfig {
    pub nodes: u16,
    /// Container names are `<host_prefix><i>`: "host-1".
    pub host_prefix: String,
    /// PGD's catalog names are `<node_prefix><i>`: "node-1".
    pub node_prefix: String,
    pub cluster_name: String,
    pub group_name: String,
    pub image: String,
    pub volume_prefix: String,

    pub db: String,
    pub user: String,

    /// pge | epas | pg — which Postgres PGD runs on. Only `pge` has been built.
    pub pg_flavor: String,
    pub pg_major: String,

    pub pg_port_base: u16,
    pub cm_port_base: u16,
    pub monitor: bool,
    /// Connection Manager `server_pool_mode`: none | session | transaction.
    /// Empty means "leave whatever the cluster already has".
    pub pool_mode: String,
    /// Preload pg_stat_statements and create the extension.
    pub stat_statements: bool,

    /// Needed by `cider pgd build` only. Never stored anywhere by this tool.
    pub token: Option<String>,
}

/// An environment variable, or `default` if unset or empty.
fn var(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// An environment variable parsed into `T`, falling back to `default` if it is
/// unset *or* unparseable. A typo in a port number gives you the default rather
/// than a crash — this is a lab tool, not a server.
fn var_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A boolean setting that is on unless explicitly switched off.
fn var_on(key: &str) -> bool {
    !matches!(var(key, "on").as_str(), "off" | "false" | "0")
}

impl Config {
    /// Read every setting once, at startup.
    ///
    /// Precedence is shell environment first, then `.env`, then the defaults
    /// here. `dotenvy` does not overwrite variables that already exist, which
    /// is what makes `CIDER_NODES=5 cider pgd up` a one-shot override.
    pub fn load() -> Self {
        let root = exe_root();

        // Shell env takes precedence: dotenvy does not overwrite existing vars.
        // Must run before anything below reads a variable.
        let _ = dotenvy::from_path(root.join(".env"));

        Config {
            domain: var("CIDER_DOMAIN", "cider"),
            password: var("CIDER_PASSWORD", "secret"),
            debian_version: var("DEBIAN_VERSION", "12"),

            cpus: var("CIDER_CPUS", "2"),
            memory: var("CIDER_MEMORY", "2G"),
            build_cpus: var("CIDER_BUILD_CPUS", "4"),
            build_memory: var("CIDER_BUILD_MEMORY", "4G"),
            ready_timeout: var_parse("CIDER_READY_TIMEOUT", 420u64),

            pgd: PgdConfig::from_env(),
            root,
        }
    }

    /// Whether macOS resolves `*.<domain>` through container's DNS service.
    pub fn resolver_installed(&self) -> bool {
        crate::container::dns_domain_registered(&self.domain)
    }

    /// Where the `container` runtime keeps its own configuration.
    ///
    /// `cider bootstrap` edits the `[dns]` table in this file. It belongs to the
    /// runtime, not to us, which is why every edit backs it up first.
    pub fn container_config_path() -> Option<PathBuf> {
        env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/container/config.toml"))
    }
}

impl PgdConfig {
    /// PGD's settings. Only called from [`Config::load`], after `.env` has
    /// been read.
    ///
    /// The variable names have no `PGD` in them (`CIDER_NODES`, not
    /// `CIDER_PGD_NODES`) because they predate a second product, and renaming
    /// them would break every existing `.env`.
    fn from_env() -> Self {
        // EDB Postgres Advanced Server's superuser is `enterprisedb`, not
        // `postgres`. Derive the default from the flavor so choosing EPAS does
        // not silently fail later with an authentication error; an explicit
        // CIDER_USER still wins.
        let pg_flavor = var("PG_FLAVOR", "pge");
        let default_user = if pg_flavor == "epas" {
            "enterprisedb"
        } else {
            "postgres"
        };

        PgdConfig {
            nodes: var_parse("CIDER_NODES", 3u16).max(1),
            host_prefix: var("CIDER_HOST_PREFIX", "host-"),
            node_prefix: var("CIDER_NODE_PREFIX", "node-"),
            cluster_name: var("CIDER_CLUSTER_NAME", "cider"),
            group_name: var("CIDER_GROUP_NAME", "group-1"),
            image: var("CIDER_IMAGE", "cider-press:latest"),
            volume_prefix: var("CIDER_VOLUME_PREFIX", "cider-press-"),

            db: var("CIDER_DB", "pgddb"),
            user: var("CIDER_USER", default_user),

            pg_flavor,
            pg_major: var("PG_MAJOR", "18"),

            pg_port_base: var_parse("CIDER_PG_PORT_BASE", 5432u16),
            cm_port_base: var_parse("CIDER_CM_PORT_BASE", 6432u16),
            monitor: var_on("CIDER_MONITOR"),
            // PGD's own default is "none" (no pooling); cider defaults to
            // session pooling, the mode with no application-visible caveats.
            pool_mode: match var("CIDER_POOL_MODE", "session").as_str() {
                "" | "leave" | "keep" => String::new(),
                other => other.to_ascii_lowercase(),
            },
            stat_statements: var_on("CIDER_STAT_STATEMENTS"),

            token: env::var("EDB_SUBSCRIPTION_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        }
    }

    /// PGD's own name for node `i`, e.g. "node-1". Not the container name —
    /// see "naming" at the bottom of this file.
    pub fn node_name(&self, i: u16) -> String {
        format!("{}{i}", self.node_prefix)
    }

    // --- published ports -------------------------------------------------------
    //
    // Inside a container the ports are always the same (5432, 6432, ...). On
    // the Mac they cannot be, because three nodes would collide on loopback, so
    // each node gets its own block of host ports:
    //
    //            postgres   cm-rw   cm-ro   cm-health   web-ui
    //   host-1       5432    6432    6433        6434     6437
    //   host-2       5433    6442    6443        6444     6447
    //   host-3       5434    6452    6453        6454     6457
    //
    // Postgres advances by one per node; the Connection Manager family advances
    // by ten, leaving room inside each block. The offsets within a block (+1,
    // +2, +5) mirror the fixed container-side ports above.
    /// Host port forwarding to node `i`'s Postgres.
    pub fn pg_port(&self, i: u16) -> u16 {
        self.pg_port_base + i - 1
    }
    /// Host port forwarding to node `i`'s Connection Manager, read-write.
    pub fn cm_rw(&self, i: u16) -> u16 {
        self.cm_port_base + (i - 1) * 10
    }
    /// Host port forwarding to node `i`'s Connection Manager, read-only.
    pub fn cm_ro(&self, i: u16) -> u16 {
        self.cm_port_base + (i - 1) * 10 + 1
    }
    /// Host port for node `i`'s Connection Manager health/JSON API.
    pub fn cm_http(&self, i: u16) -> u16 {
        self.cm_port_base + (i - 1) * 10 + 2
    }
    /// Host port for node `i`'s PGD Monitor web UI.
    pub fn ui_port(&self, i: u16) -> u16 {
        self.cm_port_base + (i - 1) * 10 + 5
    }

    /// Browsable URL for node `i`'s PGD Monitor web UI.
    pub fn ui_url(&self, i: u16) -> String {
        format!("http://127.0.0.1:{}/", self.ui_port(i))
    }

    /// A multi-host libpq URI over every node's Connection Manager read-only
    /// port, with `load_balance_hosts=random` so libpq shuffles the list rather
    /// than always trying the first host.
    pub fn read_only_uri(&self) -> String {
        let ports: Vec<u16> = (1..=self.nodes).map(|i| self.cm_ro(i)).collect();
        build_read_only_uri(&self.user, &self.db, &ports)
    }
}

/// The directory holding the project, so `image/` and `.env` are found whether
/// the binary is run from `target/release` or via the shim.
fn exe_root() -> PathBuf {
    if let Ok(dir) = env::var("CIDER_ROOT") {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = env::current_exe() {
        // target/{debug,release}/cider -> project root
        if let Some(p) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            && p.join("image/Dockerfile").exists()
        {
            return p.to_path_buf();
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// --- naming ------------------------------------------------------------------
//
// Three different names refer to "node i", and mixing them up is the easiest
// mistake to make in this codebase:
//
//   container_name   "host-1"          the *container* name
//   host_fqdn        "host-1.cider"    how nodes address each other
//   node_name        "node-1"          what PGD calls it in its own catalog
//
// The container name matters because container's DNS registers containers as
// <name>.<domain> -- so the container name and the domain together *produce*
// the FQDN. See ARCHITECTURE.md for why fully-qualified names are used
// everywhere nodes address each other.
//
// The first two apply to every product and are reached through
// `lifecycle::Deployment` (`d.host_name(i)`, `d.host_fqdn(i)`); only PGD has
// the third, on `PgdConfig`. Each is *formed* here and nowhere else, so the
// code that creates a container and the code that later stops it cannot
// disagree about what it is called.

/// Container name for node `i`, e.g. "host-1".
pub fn container_name(host_prefix: &str, i: u16) -> String {
    format!("{host_prefix}{i}")
}

/// Fully-qualified name node `i` is reachable at, e.g. "host-1.cider".
pub fn host_fqdn(host_prefix: &str, i: u16, domain: &str) -> String {
    format!("{}.{domain}", container_name(host_prefix, i))
}

/// Named volume for node `i`, e.g. "cider-press-host-1".
pub fn volume_name(volume_prefix: &str, host_prefix: &str, i: u16) -> String {
    format!("{volume_prefix}{}", container_name(host_prefix, i))
}

/// Multi-host libpq URI over the given read-only ports.
///
/// `load_balance_hosts=random` (libpq 16+) makes libpq shuffle the host list
/// per connection; without it every session would try the first host first and
/// the other read nodes would sit idle.
fn build_read_only_uri(user: &str, db: &str, ports: &[u16]) -> String {
    let hosts = ports
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("postgresql://{user}@{hosts}/{db}?load_balance_hosts=random")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PGD's settings at their defaults, built directly rather than read from
    /// the environment so a developer's own `.env` cannot change the result.
    fn pgd_defaults() -> PgdConfig {
        PgdConfig {
            nodes: 3,
            host_prefix: "host-".into(),
            node_prefix: "node-".into(),
            cluster_name: "cider".into(),
            group_name: "group-1".into(),
            image: "cider-press:latest".into(),
            volume_prefix: "cider-press-".into(),
            db: "pgddb".into(),
            user: "postgres".into(),
            pg_flavor: "pge".into(),
            pg_major: "18".into(),
            pg_port_base: 5432,
            cm_port_base: 6432,
            monitor: true,
            pool_mode: "session".into(),
            stat_statements: true,
            token: None,
        }
    }

    // The port table in the comment above, and in the README, as a test. These
    // ports are in people's bookmarks and shell history.
    #[test]
    fn ports_match_the_documented_table() {
        let p = pgd_defaults();
        let row = |i| {
            (
                p.pg_port(i),
                p.cm_rw(i),
                p.cm_ro(i),
                p.cm_http(i),
                p.ui_port(i),
            )
        };
        assert_eq!(row(1), (5432, 6432, 6433, 6434, 6437));
        assert_eq!(row(2), (5433, 6442, 6443, 6444, 6447));
        assert_eq!(row(3), (5434, 6452, 6453, 6454, 6457));
    }

    #[test]
    fn the_three_names_for_a_node() {
        assert_eq!(container_name("host-", 1), "host-1");
        assert_eq!(host_fqdn("host-", 1, "cider"), "host-1.cider");
        assert_eq!(pgd_defaults().node_name(1), "node-1");
    }

    #[test]
    fn read_only_uri_lists_every_port_and_load_balances() {
        let uri = build_read_only_uri("postgres", "pgddb", &[6433, 6443, 6453]);
        assert_eq!(
            uri,
            "postgresql://postgres@127.0.0.1:6433,127.0.0.1:6443,127.0.0.1:6453/pgddb\
             ?load_balance_hosts=random"
        );
        assert_eq!(uri.matches("127.0.0.1:").count(), 3);
        assert_eq!(pgd_defaults().read_only_uri(), uri);
    }

    #[test]
    fn read_only_uri_handles_a_single_node() {
        let uri = build_read_only_uri("edb", "mydb", &[6433]);
        assert_eq!(
            uri,
            "postgresql://edb@127.0.0.1:6433/mydb?load_balance_hosts=random"
        );
        // No stray separator when there is nothing to separate.
        assert!(!uri.contains(",,") && !uri.contains("@,"));
    }
}
