//! Health and reachability probes against a node.
//!
//! Covers both PGD Monitor and Connection Manager, since the mechanics are the
//! same and keeping every HTTP call in one module makes the timeouts easy to
//! reason about.
//!
//! Only the endpoints published in the PGD Monitor documentation are used:
//! the unauthenticated `/is-live` and `/is-ready` probes, and `/api/v1/`.
//! https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/pgd-monitor/

use crate::config::{Config, MONITOR_CONTAINER_PORT};
use crate::container;
use std::time::Duration;

/// Does the monitor answer its liveness probe on the published host port?
pub fn live_from_host(port: u16) -> bool {
    probe(&format!("http://127.0.0.1:{port}/is-live"))
}

/// Same probe, from inside the container. Distinguishes "the monitor is not
/// running" from "the monitor is fine but the published port is not reaching it".
pub fn live_in_container(name: &str) -> bool {
    container::exec_ok(
        name,
        &[],
        &[
            "curl",
            "-fsS",
            "-m",
            "3",
            "-o",
            "/dev/null",
            &format!("http://127.0.0.1:{MONITOR_CONTAINER_PORT}/is-live"),
        ],
    )
}

/// Readiness, for the status line. None when the monitor is not reachable.
pub fn health(port: u16) -> Option<&'static str> {
    if !live_from_host(port) {
        return None;
    }
    Some(if probe(&format!("http://127.0.0.1:{port}/is-ready")) {
        "ready"
    } else {
        "live, not ready"
    })
}

/// GET a URL, treating any 2xx as success.
///
/// Short timeout on purpose: these are liveness probes on loopback, so a slow
/// answer is a failed answer.
fn probe(url: &str) -> bool {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .new_agent();
    matches!(agent.get(url).call(), Ok(r) if r.status().is_success())
}

/// Can this Mac open a TCP connection to `host:port`?
///
/// Used to find out whether the by-name path actually works here before
/// advertising it. It frequently does not: a VPN or endpoint-security agent
/// (Netskope, Zscaler and similar) can intercept connections to the container
/// subnet per-process and return `EHOSTUNREACH`, which surfaces as the very
/// misleading "No route to host" even though routing and ICMP are fine.
///
/// Deliberately a real TCP connect rather than a ping: ICMP passing proves
/// nothing about whether a proxy will allow the TCP connection.
pub fn tcp_reachable(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1500)).is_ok())
}

/// Is Connection Manager ready to route read-write connections?
///
/// `/connection/is-ready-rw` answers 200 when it is and 503 when it is not,
/// which is exactly the question worth asking before telling someone to point
/// psql at port 6432.
/// <https://www.enterprisedb.com/docs/pgd/latest/connection-manager/monitoring/>
pub fn cm_ready_rw(cm_http_port: u16) -> bool {
    probe(&format!(
        "http://127.0.0.1:{cm_http_port}/connection/is-ready-rw"
    ))
}

/// Is the monitor worker switched on for this node?
pub fn guc_enabled(cfg: &Config, name: &str) -> Option<bool> {
    let out = container::exec_capture(
        name,
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
            "show bdr.monitor_enabled",
        ],
    )?;
    match out.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}
