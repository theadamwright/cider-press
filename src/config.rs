//! Configuration, resolved once from the environment (and an optional `.env`).
//!
//! Shell environment wins over `.env`, so `CIDER_NODES=5 cider pgd up` is a
//! one-shot override without editing the file.

use std::env;
use std::path::PathBuf;

// Ports *inside* a node container. These are fixed by PGD and Postgres, not by
// us; the host-side published ports are computed per node further down.

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

pub struct Config {
    pub root: PathBuf,

    pub domain: String,
    pub nodes: u16,
    pub host_prefix: String,
    pub node_prefix: String,
    pub cluster_name: String,
    pub group_name: String,
    pub image: String,
    pub volume_prefix: String,

    pub db: String,
    pub user: String,
    pub password: String,

    pub pg_flavor: String,
    pub pg_major: String,
    pub debian_version: String,

    pub cpus: String,
    pub memory: String,
    pub build_cpus: String,
    pub build_memory: String,

    pub pg_port_base: u16,
    pub cm_port_base: u16,
    pub ready_timeout: u64,
    pub monitor: bool,
    /// Connection Manager `server_pool_mode`: none | session | transaction.
    /// Empty means "leave whatever the cluster already has".
    pub pool_mode: String,
    /// Preload pg_stat_statements and create the extension.
    pub stat_statements: bool,

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

impl Config {
    /// Read every setting once, at startup.
    ///
    /// Precedence is shell environment first, then `.env`, then the defaults
    /// here. `dotenvy` does not overwrite variables that already exist, which
    /// is what makes `CIDER_NODES=5 cider pgd up` a one-shot override.
    pub fn load() -> Self {
        let root = exe_root();

        // Shell env takes precedence: dotenvy does not overwrite existing vars.
        let _ = dotenvy::from_path(root.join(".env"));

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

        Config {
            domain: var("CIDER_DOMAIN", "cider"),
            nodes: var_parse("CIDER_NODES", 3u16).max(1),
            host_prefix: var("CIDER_HOST_PREFIX", "host-"),
            node_prefix: var("CIDER_NODE_PREFIX", "node-"),
            cluster_name: var("CIDER_CLUSTER_NAME", "cider"),
            group_name: var("CIDER_GROUP_NAME", "group-1"),
            image: var("CIDER_IMAGE", "cider-press:latest"),
            volume_prefix: var("CIDER_VOLUME_PREFIX", "cider-press-"),

            db: var("CIDER_DB", "pgddb"),
            user: var("CIDER_USER", default_user),
            password: var("CIDER_PASSWORD", "secret"),

            pg_flavor,
            pg_major: var("PG_MAJOR", "18"),
            debian_version: var("DEBIAN_VERSION", "12"),

            cpus: var("CIDER_CPUS", "2"),
            memory: var("CIDER_MEMORY", "2G"),
            build_cpus: var("CIDER_BUILD_CPUS", "4"),
            build_memory: var("CIDER_BUILD_MEMORY", "4G"),

            pg_port_base: var_parse("CIDER_PG_PORT_BASE", 5432u16),
            cm_port_base: var_parse("CIDER_CM_PORT_BASE", 6432u16),
            ready_timeout: var_parse("CIDER_READY_TIMEOUT", 420u64),
            monitor: !matches!(var("CIDER_MONITOR", "on").as_str(), "off" | "false" | "0"),
            // PGD's own default is "none" (no pooling); cider defaults to
            // session pooling, the mode with no application-visible caveats.
            pool_mode: match var("CIDER_POOL_MODE", "session").as_str() {
                "" | "leave" | "keep" => String::new(),
                other => other.to_ascii_lowercase(),
            },

            stat_statements: !matches!(
                var("CIDER_STAT_STATEMENTS", "on").as_str(),
                "off" | "false" | "0"
            ),

            token: env::var("EDB_SUBSCRIPTION_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            root,
        }
    }

    // --- naming --------------------------------------------------------------
    //
    // Three different names refer to "node i", and mixing them up is the
    // easiest mistake to make in this codebase:
    //
    //   host_name(1)   "host-1"          the *container* name
    //   host_fqdn(1)   "host-1.cider"    how PGD addresses it over the network
    //   node_name(1)   "node-1"          what PGD calls it in its own catalog
    //
    // The container name matters because container's DNS registers containers
    // as <name>.<domain> -- so host_name and the domain together *produce*
    // host_fqdn. See ARCHITECTURE.md for why fully-qualified names are used
    // everywhere PGD is concerned.
    /// Container name for node `i`, e.g. "host-1".
    pub fn host_name(&self, i: u16) -> String {
        format!("{}{i}", self.host_prefix)
    }
    /// Fully-qualified name PGD nodes dial each other on, e.g. "host-1.cider".
    pub fn host_fqdn(&self, i: u16) -> String {
        format!("{}{i}.{}", self.host_prefix, self.domain)
    }
    /// PGD's own name for node `i`, e.g. "node-1".
    pub fn node_name(&self, i: u16) -> String {
        format!("{}{i}", self.node_prefix)
    }
    /// Named volume holding node `i`'s data directory.
    pub fn volume_name(&self, i: u16) -> String {
        format!("{}{}{i}", self.volume_prefix, self.host_prefix)
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

    /// Every node's FQDN, comma-separated. Handed to the entrypoint so each
    /// node can write a `pgd` CLI config listing the whole cluster.
    pub fn all_hosts_csv(&self) -> String {
        (1..=self.nodes)
            .map(|i| self.host_fqdn(i))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Connection string for the seed node (node 1), which nodes 2..n join to.
    pub fn join_dsn(&self) -> String {
        format!(
            "host={} port=5432 dbname={} user={}",
            self.host_fqdn(1),
            self.db,
            self.user
        )
    }

    /// Resolve "2" or "host-2" to an index. Defaults to node 1.
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
                t.strip_prefix(&self.host_prefix)
                    .and_then(|r| r.parse::<u16>().ok())
                    .unwrap_or(1)
            }
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

    #[test]
    fn read_only_uri_lists_every_port_and_load_balances() {
        let uri = build_read_only_uri("postgres", "pgddb", &[6433, 6443, 6453]);
        assert_eq!(
            uri,
            "postgresql://postgres@127.0.0.1:6433,127.0.0.1:6443,127.0.0.1:6453/pgddb\
             ?load_balance_hosts=random"
        );
        assert_eq!(uri.matches("127.0.0.1:").count(), 3);
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
