//! Live cluster state.
//!
//! This reads the cluster through the shipped `pgd` CLI's public JSON output
//! (`pgd <command> -o json`), documented at
//! https://www.enterprisedb.com/docs/pgd/latest/reference/cli/command_ref/
//!
//! Deliberately *not* by querying PGD's internal catalogs directly: the CLI is
//! the supported interface, so this keeps working across releases and keeps
//! this tool free of any knowledge it should not have.
//!
//! The exact JSON key spellings are not a published contract, so rows are
//! matched leniently (see `field`) and every command degrades to showing the
//! CLI's own output rather than failing.

use crate::config::Config;
use crate::container;
use crate::term;
use serde_json::Value;

/// One row of `pgd nodes list`.
pub struct Node {
    pub name: String,
    pub group: String,
    pub state: String,
    pub kind: String,
    pub status: String,
}

/// A snapshot of the cluster, as rendered by `cider pgd status`.
pub struct Cluster {
    pub nodes: Vec<Node>,
    pub raft_leader: Option<String>,
    pub raft_term: Option<String>,
    pub pool_mode: Option<String>,
    pub source: String,
}

/// Run `pgd <args> -o json` inside a node container and parse the result.
fn pgd_json(cfg: &Config, container_name: &str, args: &[&str]) -> Option<Value> {
    let mut cmd: Vec<&str> = vec!["pgd"];
    cmd.extend_from_slice(args);
    cmd.extend_from_slice(&["-o", "json"]);
    let out = container::exec_capture(container_name, &[("PGPASSWORD", &cfg.password)], &cmd)?;
    serde_json::from_str::<Value>(out.trim()).ok()
}

/// Rows from a specifically named array, wherever it sits in the document.
///
/// `pgd raft show` returns `[{"State": [...]}, {"Followers": [...]}, ...]`.
/// Order in that outer array happens to put `State` first, but relying on that
/// is luck: ask for the key by name instead.
fn rows_named<'a>(v: &'a Value, key: &str) -> Option<Vec<&'a Value>> {
    match v {
        Value::Object(m) => {
            if let Some(Value::Array(a)) = m.get(key) {
                let r: Vec<&Value> = a.iter().filter(|e| e.is_object()).collect();
                if !r.is_empty() {
                    return Some(r);
                }
            }
            m.values().find_map(|e| rows_named(e, key))
        }
        Value::Array(a) => a.iter().find_map(|e| rows_named(e, key)),
        _ => None,
    }
}

/// Find the first array of data rows anywhere in the document.
///
/// `pgd` wraps rows in an envelope whose key is not something to rely on. An
/// array only counts as rows if its objects carry at least one *scalar* field;
/// otherwise it is an envelope and the real rows are nested deeper.
fn rows(v: &Value) -> Vec<&Value> {
    fn is_row(v: &Value) -> bool {
        v.as_object()
            .is_some_and(|m| m.values().any(|x| !x.is_array() && !x.is_object()))
    }
    fn walk<'a>(v: &'a Value, found: &mut Option<Vec<&'a Value>>) {
        if found.is_some() {
            return;
        }
        match v {
            Value::Array(a) if a.iter().any(is_row) => {
                *found = Some(a.iter().filter(|e| is_row(e)).collect());
            }
            Value::Array(a) => a.iter().for_each(|e| walk(e, found)),
            Value::Object(m) => m.values().for_each(|e| walk(e, found)),
            _ => {}
        }
    }
    let mut found = None;
    walk(v, &mut found);
    found.unwrap_or_default()
}

/// Look up a field by any of several candidate names, ignoring case,
/// underscores and spaces ("Node Name" == "node_name" == "nodename").
fn field(row: &Value, candidates: &[&str]) -> Option<String> {
    let obj = row.as_object()?;
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect::<String>()
    };
    let wanted: Vec<String> = candidates.iter().map(|c| norm(c)).collect();
    for (k, v) in obj {
        if wanted.contains(&norm(k)) {
            return match v {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string().trim_matches('"').to_string()),
            };
        }
    }
    None
}

/// Connection Manager's current pool mode for a group.
///
/// Read from `bdr.node_group_summary.server_pool_mode`, which the connection
/// pooling documentation names as where this is visible.
pub fn pool_mode(cfg: &Config, container_name: &str) -> Option<String> {
    let sql = format!(
        "select server_pool_mode from bdr.node_group_summary \
         where node_group_name = '{}'",
        cfg.group_name
    );
    let out = container::exec_capture(
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
    )?;
    let v = out.trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

/// Read cluster state from the first running node.
///
/// `None` when no node is running or the CLI returned nothing usable; the
/// caller falls back to the container view rather than failing.
pub fn fetch(cfg: &Config) -> Option<Cluster> {
    // Any running node can answer for the whole cluster.
    let name = cfg.host_name(crate::cluster::first_running(cfg).ok()?);

    let nodes_json = pgd_json(cfg, &name, &["nodes", "list"])?;
    let nodes: Vec<Node> = rows(&nodes_json)
        .iter()
        .map(|r| Node {
            name: field(r, &["node_name", "name"]).unwrap_or_else(|| "-".into()),
            group: field(r, &["group_name", "node_group_name", "group"]).unwrap_or_default(),
            state: field(r, &["join_state", "peer_state_name", "state"])
                .unwrap_or_else(|| "-".into()),
            kind: field(r, &["node_kind", "node_kind_name", "kind"]).unwrap_or_else(|| "-".into()),
            status: field(r, &["node_status", "status"]).unwrap_or_else(|| "-".into()),
        })
        .collect();

    if nodes.is_empty() {
        return None;
    }

    let (mut raft_leader, mut raft_term) = (None, None);
    if let Some(raft) = pgd_json(cfg, &name, &["raft", "show"]) {
        // Prefer the "State" array by name; fall back to the generic scan.
        let raft_rows = rows_named(&raft, "State").unwrap_or_else(|| rows(&raft));
        if let Some(r) = raft_rows.first() {
            raft_leader = field(r, &["leader_name", "leader", "raft_leader"]);
            raft_term = field(r, &["current_term", "term"]);
        }
    }

    let pool = pool_mode(cfg, &name);
    Some(Cluster {
        nodes,
        raft_leader,
        raft_term,
        pool_mode: pool,
        source: name,
    })
}

/// Print the status table.
pub fn render(cfg: &Config, c: &Cluster, monitor_health: Option<&str>) {
    println!(
        " {} · PGD 6 · {} node{}",
        term::bold_amber(&cfg.cluster_name),
        c.nodes.len(),
        if c.nodes.len() == 1 { "" } else { "s" }
    );
    println!();
    // Pad first, colour second: escape codes count toward a `{:<10}` width, so
    // styling before padding collapses the columns entirely.
    println!(
        "  {}{}{}{}{}",
        term::dim(&col("NODE", 10)),
        term::dim(&col("GROUP", 12)),
        term::dim(&col("JOIN STATE", 12)),
        term::dim(&col("KIND", 10)),
        term::dim("STATUS")
    );

    for n in &c.nodes {
        let state = colour_by(&n.state, &["ACTIVE"]);
        let status = colour_by(&n.status, &["Up", "UP"]);
        println!(
            "  {}{}{}{}{}{}{}{}{}",
            n.name,
            pad(&n.name, 10),
            n.group,
            pad(&n.group, 12),
            state,
            pad(&n.state, 12),
            n.kind,
            pad(&n.kind, 10),
            status,
        );
    }

    println!();
    let raft = match (&c.raft_leader, &c.raft_term) {
        (Some(l), Some(t)) => format!("raft leader: {l} (term {t})"),
        (Some(l), None) => format!("raft leader: {l}"),
        _ => "raft: unavailable".to_string(),
    };
    let pool = match &c.pool_mode {
        Some(m) => format!("  ·  pooling: {m}"),
        None => String::new(),
    };
    match monitor_health {
        Some(h) => println!("  {raft}{pool}  ·  monitor: {h}"),
        None => println!("  {raft}{pool}"),
    }
    println!(
        "  {}",
        term::dim(&format!("↑ via the pgd CLI on {}", c.source))
    );
}

/// Green for the values that mean "healthy", dim for "unknown", amber
/// otherwise — so an unexpected state stands out without being alarming.
fn colour_by(value: &str, good: &[&str]) -> String {
    if good.contains(&value) {
        term::green(value)
    } else if value == "-" {
        term::dim(value)
    } else {
        term::yellow(value)
    }
}

/// Spaces needed to fill `width`, but never zero: an over-wide value such as
/// "JOIN_START->ACTIVE" would otherwise run straight into the next column.
fn pad(plain: &str, width: usize) -> String {
    " ".repeat(width.saturating_sub(plain.chars().count()).max(1))
}

/// Left-align `s` in `width` columns, before any styling is applied. Headers
/// are chosen to fit, so this pads exactly rather than enforcing a gap.
fn col(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(s.chars().count())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_rows_in_a_bare_array() {
        let v = json!([{"node_name": "node-1"}, {"node_name": "node-2"}]);
        assert_eq!(rows(&v).len(), 2);
    }

    #[test]
    fn finds_rows_inside_an_envelope() {
        let v = json!({"result": {"nodes": [{"node_name": "node-1"}]}});
        assert_eq!(rows(&v).len(), 1);
    }

    // The real shape of `pgd raft show -o json`: an outer array holding one
    // object whose "State" key carries the rows. Taking the outer array gave
    // "raft: unavailable" even though Raft was perfectly healthy.
    /// The real shape of `pgd raft show -o json`: sibling objects, State first.
    fn raft_json() -> Value {
        json!([
            {"State": [
                {"group_name": "cider",   "leader_name": "node-1", "current_term": 0},
                {"group_name": "group-1", "leader_name": "node-1", "current_term": 0}
            ]},
            {"Followers": [{"group_name": "cider", "node_name": "node-2"}]},
            {"Journal": [{"group_name": "cider", "size": 12}]}
        ])
    }

    #[test]
    fn skips_an_envelope_array_and_finds_the_nested_rows() {
        let v = raft_json();
        let r = rows(&v);
        assert_eq!(r.len(), 2, "should find the two State rows");
        assert_eq!(field(r[0], &["leader_name"]).as_deref(), Some("node-1"));
        assert_eq!(field(r[0], &["current_term"]).as_deref(), Some("0"));
    }

    #[test]
    fn rows_named_targets_the_right_array() {
        let v = raft_json();
        let r = rows_named(&v, "State").expect("State array");
        assert_eq!(r.len(), 2);
        assert_eq!(field(r[0], &["leader_name"]).as_deref(), Some("node-1"));

        let f = rows_named(&v, "Followers").expect("Followers array");
        assert_eq!(f.len(), 1);
        assert_eq!(rows_named(&v, "Nope"), None);
    }

    /// If those keys ever shared one object, key ordering would decide the
    /// answer: serde sorts them, so "Followers" would win over "State".
    #[test]
    fn rows_named_is_unambiguous_when_keys_share_an_object() {
        let v = json!([{
            "Followers": [{"group_name": "cider", "node_name": "node-2"}],
            "State": [{"group_name": "cider", "leader_name": "node-1"}]
        }]);
        let r = rows_named(&v, "State").expect("State array");
        assert_eq!(field(r[0], &["leader_name"]).as_deref(), Some("node-1"));
    }

    #[test]
    fn an_object_of_only_arrays_is_not_a_row() {
        let v = json!([{"State": [], "Followers": []}]);
        assert_eq!(rows(&v).len(), 0);
    }

    #[test]
    fn no_rows_when_there_are_no_objects() {
        assert_eq!(rows(&json!({"count": 3})).len(), 0);
        assert_eq!(rows(&json!([])).len(), 0);
    }

    #[test]
    fn field_lookup_ignores_case_spaces_and_underscores() {
        let r = json!({"Node Name": "node-1"});
        assert_eq!(field(&r, &["node_name"]).as_deref(), Some("node-1"));

        let r = json!({"node_name": "node-2"});
        assert_eq!(field(&r, &["Node Name"]).as_deref(), Some("node-2"));

        let r = json!({"NODENAME": "node-3"});
        assert_eq!(field(&r, &["node_name"]).as_deref(), Some("node-3"));
    }

    #[test]
    fn field_falls_through_candidates_and_handles_non_strings() {
        let r = json!({"kind": "data", "term": 4, "missing": null});
        assert_eq!(field(&r, &["node_kind", "kind"]).as_deref(), Some("data"));
        assert_eq!(field(&r, &["term"]).as_deref(), Some("4"));
        assert_eq!(field(&r, &["missing"]), None);
        assert_eq!(field(&r, &["nope"]), None);
    }

    // The header once rendered as "NODEGROUPJOIN STATEKINDSTATUS": the values
    // were styled first, and the ANSI escape bytes counted toward the `{:<10}`
    // width, so no padding was emitted. Columns must be built before styling.
    #[test]
    fn header_columns_are_padded_before_styling() {
        assert_eq!(col("NODE", 10), "NODE      ");
        assert_eq!(col("JOIN STATE", 12), "JOIN STATE  ");
        assert_eq!(col("NODE", 10).len(), 10);
        // Oversized values are not truncated, just unpadded.
        assert_eq!(col("A-VERY-WIDE-HEADING", 4), "A-VERY-WIDE-HEADING");
    }

    #[test]
    fn pads_and_always_leaves_a_gap() {
        assert_eq!(pad("ACTIVE", 12).len(), 6);
        // An over-wide value still gets a separator, so it cannot run into the
        // next column the way "JOIN_START->ACTIVEdata" did.
        assert_eq!(pad("JOIN_START->ACTIVE", 12).len(), 1);
        assert_eq!(pad("exactly-12ch", 12).len(), 1);
    }
}
