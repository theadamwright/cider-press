#!/usr/bin/env bash
#
# cider-press load balancer for `cider efm`: HAProxy in front of the database
# nodes, giving one address that always reaches the current primary.
#
# Failover Manager 5.4 added an HTTP health endpoint for exactly this: each
# agent answers 200 on the primary and 404 everywhere else. HAProxy asks every
# node once a second, so only the primary is ever "up" and every connection
# goes to it. After a failover the new primary starts answering 200 and
# connections follow it, as they follow the write leader through PGD's
# Connection Manager.
#
# Runs from the same image as the nodes, started with --entrypoint.

set -euo pipefail

# shellcheck disable=SC1091
. /etc/cider-press/image.env

CIDER_GROUP="efm"
# The image path; shellcheck is pointed at the source-tree copy instead.
# shellcheck source-path=SCRIPTDIR source=lib/node-common.sh
. /usr/local/lib/cider-press/node-common.sh

: "${EFM_NODES:?EFM_NODES is required}"
EFM_HEALTH_PORT=8080

# The node names must resolve before HAProxy starts, because it resolves each
# server once, at startup. From /etc/hosts they always do.
write_peer_hosts

CFG=/etc/haproxy/cider-press.cfg
{
    cat <<'EOF'
global
    log stdout format raw local0 info
    maxconn 256

defaults
    log global
    timeout connect 3s
    timeout check 2s
    # Long, because these are database sessions, not web requests.
    timeout client 1h
    timeout server 1h

# One address for the current primary.
frontend primary
    mode tcp
    bind :::5432 v4v6
    default_backend efm_primary

backend efm_primary
    mode tcp
    # Every node's Failover Manager agent is asked GET / on its health port;
    # only the primary answers 200. Checked every second, and a single answer
    # is enough either way, so a failover is followed within about a second.
    # shutdown-sessions closes connections to a node that stops being primary,
    # rather than leaving clients talking to a server that just became a
    # standby.
    option httpchk GET /
    http-check expect status 200
    default-server inter 1s fall 1 rise 1 on-marked-down shutdown-sessions
EOF
    for fqdn in ${EFM_NODES//,/ }; do
        printf '    server %s %s:5432 check port %s\n' "${fqdn%%.*}" "$fqdn" "$EFM_HEALTH_PORT"
    done
    cat <<'EOF'

# The stats page: which node is primary right now. The primary shows UP and
# the standbys DOWN -- DOWN here means only "not the primary".
listen stats
    mode http
    bind :::8404 v4v6
    stats enable
    stats uri /
    stats refresh 2s
    stats show-legends
EOF
} > "$CFG"

haproxy -c -q -f "$CFG" || die "generated HAProxy configuration is invalid: $CFG"
log "HAProxy routing to the primary among: ${EFM_NODES}"

# HAProxy as PID 1, in master-worker mode and in the foreground, logging to
# the container log. It takes the container's stop signal directly.
exec haproxy -W -db -f "$CFG"
