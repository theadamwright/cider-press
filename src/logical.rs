//! `cider logical`: two community PostgreSQL nodes, ready for you to wire up
//! logical replication between them.
//!
//! The shared lifecycle — start, wait, retry, stop, tear down — is in
//! `lifecycle`, exactly as for PGD. This module supplies the rest: how a node's
//! container is created, what "ready" means for it, a status view of its
//! publications and subscriptions, and the SQL to connect the pair.
//!
//! It deliberately creates no publication or subscription itself. Typing
//! those is the point of the exercise; `endpoints` prints them ready to paste.

use crate::config::{Config, PG_CONTAINER_PORT};
use crate::lifecycle::{self, Deployment};
use crate::{container, term};
use anyhow::Result;

/// The logical-replication pair, described for the shared lifecycle.
pub fn deployment(cfg: &Config) -> Deployment<'_> {
    Deployment {
        group: "logical",
        title: "logical replication",
        cluster_name: &cfg.logical.cluster_name,
        nodes: cfg.logical.nodes,
        host_prefix: &cfg.logical.host_prefix,
        volume_prefix: &cfg.logical.volume_prefix,
        image: &cfg.logical.image,
        domain: &cfg.domain,
        dockerfile: "image/logical.Dockerfile",
        ready_timeout: cfg.ready_timeout,
        extras: vec![],
    }
}

// --- build -----------------------------------------------------------------

/// Build the node image. No token: PGDG's repository is public.
pub fn build(cfg: &Config, no_cache: bool) -> Result<()> {
    if !container::installed() {
        anyhow::bail!("container is not installed — run: cider doctor");
    }
    let d = deployment(cfg);
    term::banner_for(d.title);
    term::info(&format!("building {}", d.image));
    println!(
        "  postgres {} (community, from the PGDG repository)",
        cfg.logical.pg_major
    );
    println!(
        "  base     debian:{}-slim (arm64)",
        cfg.logical.debian_version
    );
    println!("  token    none needed");
    println!();

    let extra: Vec<String> = vec![
        "--build-arg".into(),
        format!("PG_MAJOR={}", cfg.logical.pg_major),
        "--build-arg".into(),
        format!("DEBIAN_VERSION={}", cfg.logical.debian_version),
    ];
    lifecycle::build_image(cfg, &d, &extra, no_cache)
}

// --- up --------------------------------------------------------------------

/// Is node `i` ready for you to publish from and subscribe to?
///
/// Two things, both needed before the printed `CREATE SUBSCRIPTION` works:
/// Postgres answering with `wal_level = logical`, and the node's name in the
/// runtime's DNS, since that is how its peer's subscription will reach it.
/// The second is often the slow one; see `lifecycle::explain_dns_wait`.
fn node_ready(cfg: &Config, i: u16) -> bool {
    let d = deployment(cfg);
    let name = d.host_name(i);
    let wal_level = container::exec_capture(
        &name,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-h",
            "127.0.0.1",
            "-U",
            &cfg.logical.user,
            "-d",
            &cfg.logical.db,
            "-tAqc",
            "select current_setting('wal_level')",
        ],
    );
    wal_level.is_some_and(|w| w.trim() == "logical")
        && container::resolves_inside(&name, &d.host_fqdn(i)) == Some(true)
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
    term::info(&format!("running {name} at {fqdn}"));

    let own_args: Vec<String> = vec![
        "--publish".into(),
        format!("127.0.0.1:{}:{PG_CONTAINER_PORT}", cfg.logical.pg_port(i)),
        "--env".into(),
        format!("NODE_FQDN={fqdn}"),
        "--env".into(),
        format!("POSTGRES_DB={}", cfg.logical.db),
        "--env".into(),
        format!("POSTGRES_USER={}", cfg.logical.user),
        "--env".into(),
        format!("PGPASSWORD={}", cfg.password),
        cfg.logical.image.clone(),
    ];
    let mut args = lifecycle::base_run_args(cfg, &d, i);
    args.extend(own_args);

    if !container::run_streaming(&args)? {
        anyhow::bail!("could not run {name}");
    }
    Ok(())
}

/// Create the pair. Unlike PGD there is no seed and no join, so the order
/// does not matter; they still start one at a time, which keeps the output
/// readable and costs a few seconds.
pub fn up(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);
    lifecycle::preflight(&d)?;
    lifecycle::require_dns_domain(cfg)?;

    term::banner_for(d.title);
    term::info(&format!(
        "pressing a {}-node logical replication pair '{}'",
        d.nodes, d.cluster_name
    ));
    println!();

    for i in 1..=d.nodes {
        lifecycle::start_and_wait(&d, i, |i| start_node(cfg, i), |i| node_ready(cfg, i))?;
    }

    println!();
    status(cfg)?;
    println!();
    endpoints(cfg);
    Ok(())
}

// --- status ----------------------------------------------------------------

/// One node, as `status` shows it.
#[derive(Debug, PartialEq)]
struct NodeState {
    wal_level: String,
    publications: Vec<String>,
    subscriptions: Vec<Subscription>,
}

#[derive(Debug, PartialEq)]
struct Subscription {
    name: String,
    /// streaming | stopped | disabled
    state: String,
    apply_errors: u64,
}

/// One row, `|`-separated: wal_level | publications | subscriptions, where
/// each subscription is `name:state:apply_errors` and lists are
/// comma-separated. Public catalogs and statistics views only.
///
/// "streaming" means the subscription's leader apply worker is running. A
/// subscription whose apply worker keeps failing — a conflict it cannot
/// apply, say — flips between states, so the error count is the more
/// telling column. That count is PostgreSQL's own, and includes failures to
/// connect to the peer, which every restart produces for a while; it is not a
/// conflict count.
const STATE_SQL: &str = "\
select current_setting('wal_level'),
       coalesce((select string_agg(pubname, ',' order by pubname) from pg_publication), ''),
       coalesce((select string_agg(
                   s.subname || ':' ||
                   case when not s.subenabled then 'disabled'
                        when exists (select 1 from pg_stat_subscription st
                                     where st.subid = s.oid and st.pid is not null
                                       and st.relid is null and st.leader_pid is null)
                        then 'streaming'
                        else 'stopped' end || ':' ||
                   coalesce(ss.apply_error_count, 0),
                   ',' order by s.subname)
                 from pg_subscription s
                 left join pg_stat_subscription_stats ss on ss.subid = s.oid
                 where s.subdbid = (select oid from pg_database
                                    where datname = current_database())), '')";

/// Parse one row of [`STATE_SQL`]. `None` if it is not the shape expected.
fn parse_state(row: &str) -> Option<NodeState> {
    let mut cols = row.trim().splitn(3, '|');
    let wal_level = cols.next()?.to_string();
    let pubs = cols.next()?;
    let subs = cols.next()?;
    let list = |s: &str| -> Vec<String> {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    };
    let subscriptions = subs
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Split from the right, so a name containing ':' still parses.
            let mut parts = s.rsplitn(3, ':');
            let apply_errors = parts.next()?.parse().ok()?;
            let state = parts.next()?.to_string();
            let name = parts.next()?.to_string();
            Some(Subscription {
                name,
                state,
                apply_errors,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(NodeState {
        wal_level,
        publications: list(pubs),
        subscriptions,
    })
}

fn fetch_state(cfg: &Config, name: &str) -> Option<NodeState> {
    let out = container::exec_capture(
        name,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-h",
            "127.0.0.1",
            "-U",
            &cfg.logical.user,
            "-d",
            &cfg.logical.db,
            "-tAq",
            "-F",
            "|",
            "-c",
            STATE_SQL,
        ],
    )?;
    parse_state(&out)
}

/// Each node's replication state: WAL level, publications, subscriptions and
/// their apply errors.
///
/// Degrades per node rather than failing: a stopped node, or one that is not
/// answering yet, is shown as such alongside the others.
pub fn status(cfg: &Config) -> Result<()> {
    let d = deployment(cfg);

    // Gather first, print after: the publications column is sized to its
    // widest entry, so two publications cannot run into the next column.
    enum Row {
        Up {
            wal: String,
            pubs: String,
            subs: String,
        },
        Down(&'static str),
    }
    let mut rows = Vec::new();
    let mut any_subscription = false;
    let mut waiting_for_dns = Vec::new();
    for i in 1..=d.nodes {
        let name = d.host_name(i);
        let running = container::state(&name) == container::State::Running;
        let row = match running.then(|| fetch_state(cfg, &name)).flatten() {
            Some(s) => {
                any_subscription |= !s.subscriptions.is_empty();
                if container::resolves_inside(&name, &d.host_fqdn(i)) == Some(false) {
                    waiting_for_dns.push(d.host_fqdn(i));
                }
                Row::Up {
                    wal: s.wal_level,
                    pubs: if s.publications.is_empty() {
                        "—".into()
                    } else {
                        s.publications.join(",")
                    },
                    subs: if s.subscriptions.is_empty() {
                        "—".into()
                    } else {
                        s.subscriptions
                            .iter()
                            .map(describe_subscription)
                            .collect::<Vec<_>>()
                            .join("; ")
                    },
                }
            }
            None => Row::Down(match container::state(&name) {
                container::State::Running => "running, not answering yet",
                container::State::Stopped => "stopped",
                container::State::Absent => "not created",
            }),
        };
        rows.push((name, row));
    }
    let pubs_width = rows
        .iter()
        .filter_map(|(_, r)| match r {
            Row::Up { pubs, .. } => Some(pubs.chars().count()),
            Row::Down(_) => None,
        })
        .chain(["PUBLICATIONS".len()])
        .max()
        .unwrap_or(0)
        + 2;

    // The version as the server reports it -- "19beta4", "18.6" -- so it is
    // plain which pre-release a demo is running on. The configured major if
    // no node answers.
    let version = (1..=d.nodes)
        .find_map(|i| server_version(cfg, &d.host_name(i)))
        .unwrap_or_else(|| cfg.logical.pg_major.clone());
    println!(
        " {} · {} · PostgreSQL {} · {} nodes",
        term::bold_amber(d.cluster_name),
        d.title,
        version,
        d.nodes
    );
    println!();
    println!(
        "  {:<14}{:<10}{:<pubs_width$}SUBSCRIPTIONS",
        "NODE", "WAL", "PUBLICATIONS"
    );
    for (name, row) in rows {
        match row {
            Row::Up { wal, pubs, subs } => {
                println!("  {name:<14}{wal:<10}{pubs:<pubs_width$}{subs}")
            }
            Row::Down(why) => println!("  {name:<14}{}", term::dim(why)),
        }
    }

    for fqdn in waiting_for_dns {
        println!();
        lifecycle::explain_dns_wait(&fqdn);
    }
    if !any_subscription {
        println!();
        term::note(&format!(
            "No subscriptions yet. The SQL to wire the pair up: {}",
            d.hint("endpoints")
        ));
    }
    Ok(())
}

/// The server's own version string, e.g. "19beta4" or "18.6".
fn server_version(cfg: &Config, name: &str) -> Option<String> {
    if container::state(name) != container::State::Running {
        return None;
    }
    container::exec_capture(
        name,
        &[("PGPASSWORD", cfg.password.as_str())],
        &[
            "psql",
            "-X",
            "-h",
            "127.0.0.1",
            "-U",
            &cfg.logical.user,
            "-d",
            &cfg.logical.db,
            "-tAqc",
            "show server_version",
        ],
    )
    .map(|v| v.split_whitespace().next().unwrap_or("").to_string())
    .filter(|v| !v.is_empty())
}

/// "from_dolores_2 streaming", with the apply error count when there is one.
fn describe_subscription(s: &Subscription) -> String {
    match s.apply_errors {
        0 => format!("{} {}", s.name, s.state),
        1 => format!("{} {}, 1 apply error", s.name, s.state),
        n => format!("{} {}, {n} apply errors", s.name, s.state),
    }
}

// --- endpoints -------------------------------------------------------------

/// How to connect, and the SQL to wire the pair up in both directions.
pub fn endpoints(cfg: &Config) {
    let d = deployment(cfg);
    term::info("connect");
    println!(
        "  {}",
        term::dim("# simplest — runs psql inside a node, so host networking can't interfere")
    );
    for i in 1..=d.nodes {
        println!("  ./cider logical psql {i}");
    }
    println!();
    println!("  {}", term::dim("# from your own tools on loopback"));
    for i in 1..=d.nodes {
        println!(
            "  PGPASSWORD={} psql -h 127.0.0.1 -p {} -U {} {}",
            cfg.password,
            cfg.logical.pg_port(i),
            cfg.logical.user,
            cfg.logical.db
        );
    }
    println!();
    term::info("wire up replication in both directions");
    print_sql(&wiring_sql(&d, &cfg.logical.db));

    if major_at_least(&cfg.logical.pg_major, 19) {
        println!();
        term::info("PostgreSQL 19: sequences, one way, dolores-1 to dolores-2");
        print_sql(&sequence_sql(&d, &cfg.logical.db));
    }
}

/// Print SQL indented for the terminal. No indent on blank lines: trailing
/// spaces paste badly.
fn print_sql(sql: &str) {
    for line in sql.lines() {
        if line.is_empty() {
            println!();
        } else {
            println!("  {line}");
        }
    }
}

/// Whether a major version string ("18", "19") is at least `at_least`.
fn major_at_least(major: &str, at_least: u32) -> bool {
    major.parse::<u32>().is_ok_and(|m| m >= at_least)
}

/// PostgreSQL 19's sequence synchronisation, as a script to follow.
///
/// What it shows, measured on 19beta4: CREATE SUBSCRIPTION copies a sequence's
/// value once, along with the rows. After that, rows keep replicating but the
/// sequence does not -- the subscriber's stays where it was -- until ALTER
/// SUBSCRIPTION ... REFRESH SEQUENCES brings it level. Skip that before
/// writing on the subscriber, as in a switchover, and its first insert takes
/// an id that has already been replicated: a duplicate-key error.
///
/// One way, and the order matters: stop writing on the publisher before
/// refreshing. Writing on both afterwards gives both the same next id, and the
/// publisher's then conflicts on the subscriber.
///
/// REFRESH SEQUENCES returns before the value changes: a background worker
/// applies it, measured at under half a second. An INSERT sent in the same
/// breath still got the old value and collided, so the script checks
/// last_value in between.
fn sequence_sql(d: &Deployment, db: &str) -> String {
    let sub = format!("orders_{}", subscription_name(d, 1));
    format!(
        "-- on BOTH nodes\n\
         CREATE SEQUENCE order_id;\n\
         CREATE TABLE orders (id bigint PRIMARY KEY DEFAULT nextval('order_id'), item text);\n\
         \n\
         -- on {p} (./cider logical psql 1): publish the table, and every sequence\n\
         CREATE PUBLICATION pub_orders FOR TABLE orders;\n\
         CREATE PUBLICATION pub_sequences FOR ALL SEQUENCES;\n\
         INSERT INTO orders (item) SELECT 'order ' || g FROM generate_series(1, 5) g;\n\
         \n\
         -- on {s} (./cider logical psql 2): rows and the sequence's value both arrive\n\
         CREATE SUBSCRIPTION {sub}\n  \
         CONNECTION 'host={pf} dbname={db} tcp_user_timeout=5000'\n  \
         PUBLICATION pub_orders, pub_sequences;\n\
         \n\
         -- on {p}: more orders. The rows replicate; the sequence does not\n\
         INSERT INTO orders (item) SELECT 'order ' || g FROM generate_series(6, 20) g;\n\
         \n\
         -- on {s}: last_value is still 5, so a write here would take id 6: duplicate key\n\
         SELECT last_value FROM order_id;\n\
         \n\
         -- switch over: stop writing on {p}, then on {s}\n\
         ALTER SUBSCRIPTION {sub} REFRESH SEQUENCES;\n\
         SELECT last_value FROM order_id;  -- 20, once a background worker applies it: under a second\n\
         INSERT INTO orders (item) VALUES ('first order on {s}') RETURNING id;  -- 21\n",
        p = d.host_name(1),
        s = d.host_name(2),
        pf = d.host_fqdn(1),
    )
}

/// A subscription name for "from node `i`": `from_dolores_2`. Hyphens are not
/// allowed in an unquoted SQL identifier, so anything but a letter or digit
/// becomes `_`.
fn subscription_name(d: &Deployment, from: u16) -> String {
    let host: String = d
        .host_name(from)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("from_{host}")
}

/// The SQL that connects the pair: the same table and publication on both
/// nodes, then on each a subscription to the other.
///
/// `origin = none` (PostgreSQL 16+) is what makes two-way replication safe to
/// set up: without it each node would send the other's changes straight back.
/// `copy_data = false` because both tables start empty.
///
/// `tcp_user_timeout=5000` makes a connection attempt to the peer give up
/// after 5s and retry. Without it, restarting both nodes regularly left a
/// subscription's first attempt hanging for the kernel's full SYN-retry time,
/// about 127s, before it tried again: measured at 136s against 7-8s with it.
/// `connect_timeout` would not help; the apply worker connects with libpq's
/// asynchronous API, which leaves that timeout to the caller. PGD's own
/// connection strings set `tcp_user_timeout` for the same reason. What this does *not*
/// do is resolve conflicts, replicate DDL or keep sequences in step — those
/// are left to you, and a primary key inserted on both nodes is the quickest
/// way to see it.
fn wiring_sql(d: &Deployment, db: &str) -> String {
    let mut sql = String::from(
        "-- on BOTH nodes\n\
         CREATE TABLE pingpong (id int PRIMARY KEY, msg text);\n\
         CREATE PUBLICATION pp FOR TABLE pingpong;\n",
    );
    for i in 1..=d.nodes {
        // Each node subscribes to the other one. For a pair that is simply
        // "the node that isn't i".
        let peer = if i == 1 { 2 } else { 1 };
        sql.push_str(&format!(
            "\n-- on {} (./cider logical psql {i})\n\
             CREATE SUBSCRIPTION {}\n  \
             CONNECTION 'host={} dbname={db} tcp_user_timeout=5000'\n  \
             PUBLICATION pp\n  \
             WITH (origin = none, copy_data = false);\n",
            d.host_name(i),
            subscription_name(d, peer),
            d.host_fqdn(peer),
        ));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> Deployment<'static> {
        Deployment {
            group: "logical",
            title: "logical replication",
            cluster_name: "dolores",
            nodes: 2,
            host_prefix: "dolores-",
            volume_prefix: "cider-press-",
            image: "cider-press-logical:latest",
            domain: "cider",
            dockerfile: "image/logical.Dockerfile",
            ready_timeout: 420,
            extras: vec![],
        }
    }

    #[test]
    fn names_are_distinct_from_pgds() {
        let d = pair();
        assert_eq!(d.host_name(1), "dolores-1");
        assert_eq!(d.host_fqdn(2), "dolores-2.cider");
        assert_eq!(d.volume_name(1), "cider-press-dolores-1");
    }

    #[test]
    fn each_node_subscribes_to_the_other_by_fqdn() {
        let sql = wiring_sql(&pair(), "demo");
        // Fully qualified: the CONNECTION string is stored and resolved again
        // on every reconnect, so it needs the name that always resolves.
        assert!(sql.contains(
            "-- on dolores-1 (./cider logical psql 1)\n\
             CREATE SUBSCRIPTION from_dolores_2\n  \
             CONNECTION 'host=dolores-2.cider dbname=demo tcp_user_timeout=5000'"
        ));
        assert!(sql.contains(
            "-- on dolores-2 (./cider logical psql 2)\n\
             CREATE SUBSCRIPTION from_dolores_1\n  \
             CONNECTION 'host=dolores-1.cider dbname=demo tcp_user_timeout=5000'"
        ));
        assert_eq!(sql.matches("origin = none, copy_data = false").count(), 2);
    }

    #[test]
    fn sequence_demo_appears_from_19() {
        assert!(!major_at_least("18", 19));
        assert!(major_at_least("19", 19));
        assert!(major_at_least("20", 19));
        assert!(!major_at_least("nineteen", 19));
        let sql = sequence_sql(&pair(), "demo");
        assert!(sql.contains("CREATE PUBLICATION pub_sequences FOR ALL SEQUENCES;"));
        assert!(sql.contains(
            "CREATE SUBSCRIPTION orders_from_dolores_1\n  \
             CONNECTION 'host=dolores-1.cider dbname=demo tcp_user_timeout=5000'\n  \
             PUBLICATION pub_orders, pub_sequences;"
        ));
        // The refresh comes after "stop writing", and the first write on the
        // subscriber only after checking the refresh has landed.
        let stop = sql.find("stop writing on dolores-1").unwrap();
        let refresh = sql.find("REFRESH SEQUENCES").unwrap();
        let check = sql.rfind("SELECT last_value").unwrap();
        let write = sql.find("first order on dolores-2").unwrap();
        assert!(stop < refresh && refresh < check && check < write);
    }

    #[test]
    fn subscription_names_are_valid_unquoted_identifiers() {
        let name = subscription_name(&pair(), 2);
        assert_eq!(name, "from_dolores_2");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn parses_a_node_with_nothing_wired_yet() {
        assert_eq!(
            parse_state("logical||\n"),
            Some(NodeState {
                wal_level: "logical".into(),
                publications: vec![],
                subscriptions: vec![],
            })
        );
    }

    #[test]
    fn parses_publications_and_subscriptions() {
        let s = parse_state("logical|pp,qq|from_dolores_2:streaming:0,other:stopped:3").unwrap();
        assert_eq!(s.publications, ["pp", "qq"]);
        assert_eq!(s.subscriptions.len(), 2);
        assert_eq!(
            s.subscriptions[1],
            Subscription {
                name: "other".into(),
                state: "stopped".into(),
                apply_errors: 3,
            }
        );
        assert_eq!(
            describe_subscription(&s.subscriptions[0]),
            "from_dolores_2 streaming"
        );
        assert_eq!(
            describe_subscription(&s.subscriptions[1]),
            "other stopped, 3 apply errors"
        );
    }

    #[test]
    fn rejects_rows_of_the_wrong_shape() {
        assert_eq!(parse_state(""), None);
        assert_eq!(parse_state("logical|pp"), None);
        assert_eq!(parse_state("logical||sub:streaming:notanumber"), None);
    }
}
