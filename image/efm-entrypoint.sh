#!/usr/bin/env bash
#
# cider-press node entrypoint for `cider efm`: one PostgreSQL server watched by
# an EDB Failover Manager agent.
#
# Unlike the PGD and logical entrypoints, this one does not hand over to
# postgres. Failover Manager stops, starts and promotes the database itself --
# fencing a failed primary, say -- so the container's life cannot be tied to
# postgres. It is tied to this script instead, which stays as PID 1 (as root,
# so it can run each process as its own user) and supervises both: postgres as
# `postgres`, the agent as `efm`. On the stop signal it shuts the agent down
# first, then postgres.
#
# What every product shares is in lib/node-common.sh.

set -euo pipefail

# shellcheck disable=SC1091
. /etc/cider-press/image.env
export PATH="/opt/cider-press/bin:${EFM_HOME}/bin:${PATH}"

CIDER_GROUP="efm"
# The image path; shellcheck is pointed at the source-tree copy instead.
# shellcheck source-path=SCRIPTDIR source=lib/node-common.sh
. /usr/local/lib/cider-press/node-common.sh

[ "$(id -u)" = "0" ] || die "must start as root: it runs postgres and the EFM agent as their own users"

: "${NODE_NAME:?NODE_NAME is required}"
: "${NODE_FQDN:?NODE_FQDN is required}"
: "${EFM_CLUSTER:?EFM_CLUSTER is required}"
: "${EFM_NODES:?EFM_NODES is required}"
: "${EFM_PRIMARY_FQDN:?EFM_PRIMARY_FQDN is required}"
: "${EFM_PING_HOST:?EFM_PING_HOST is required}"
EFM_IS_FIRST="${EFM_IS_FIRST:-false}"
EFM_AUTO_REJOIN="${EFM_AUTO_REJOIN:-on}"
POSTGRES_DB="${POSTGRES_DB:-efmdb}"
POSTGRES_USER="${POSTGRES_USER:-postgres}"
EFM_AGENT_PORT=7800
EFM_HEALTH_PORT=8080

# Exported so the server inherits it: a standby's WAL receiver connects to the
# primary with it, as a logical subscription does in `cider logical`.
export PGPASSWORD="${PGPASSWORD:-}"
[ -n "$PGPASSWORD" ] || die "PGPASSWORD is required"

EFM_ETC="/etc/edb/efm-${EFM_VERSION}"
EFM_LOG_DIR="/var/log/efm-${EFM_VERSION}"
EFM_PROPS="${EFM_ETC}/${EFM_CLUSTER}.properties"
EFM_NODES_FILE="${EFM_ETC}/${EFM_CLUSTER}.nodes"
EFM_PID="/run/efm-${EFM_VERSION}/${EFM_CLUSTER}.pid"
NOTIFICATIONS="${EFM_LOG_DIR}/cider-notifications.log"

# Run a command as postgres, with a HOME it can read (setpriv keeps root's).
as_postgres() {
    HOME=/var/lib/postgresql setpriv --reuid=postgres --regid=postgres --init-groups -- "$@"
}
as_efm() { runuser -u efm -- "$@"; }
pg_up() { as_postgres pg_isready -q -h 127.0.0.1 -p 5432 2>/dev/null; }

# The node whose Failover Manager agent says it is the primary right now: its
# health endpoint answers 200. Asked of every other node. Prints nothing if
# none does -- on a brand new cluster, say.
current_primary() {
    local fqdn
    for fqdn in ${EFM_NODES//,/ }; do
        [ "$fqdn" = "$NODE_FQDN" ] && continue
        if [ "$(curl -s -o /dev/null -m 2 -w '%{http_code}' "http://${fqdn}:${EFM_HEALTH_PORT}/")" = "200" ]; then
            printf '%s' "$fqdn"
            return 0
        fi
    done
}

log "node=${NODE_NAME} host=${NODE_FQDN} cluster=${EFM_CLUSTER} first=${EFM_IS_FIRST} efm=${EFM_VERSION} pg=${PG_MAJOR}"

prepare_state_dirs
write_peer_hosts

# The password for anything that runs as postgres and connects to another
# node: pg_basebackup here, and pg_rewind and pg_basebackup when Failover
# Manager rebuilds a failed primary. Failover Manager runs those through
# `sudo -u postgres`, which strips the environment -- PGPASSWORD included --
# so without this file its rebuilds fail with "no password supplied".
install -o postgres -g postgres -m 0600 /dev/null /var/lib/postgresql/.pgpass
printf '*:*:*:%s:%s\n' "$POSTGRES_USER" "$PGPASSWORD" > /var/lib/postgresql/.pgpass

# --- Provision, once ---------------------------------------------------------
# Finished provisioning is recorded with a marker rather than detected from
# PG_VERSION, as in the logical entrypoint: a first start that died half way
# is discarded and done again, which is what makes `up`'s retry safe.
MARKER="${PGDATA}/.cider-press-provisioned"

if [ ! -f "$MARKER" ]; then
    if [ -n "$(ls -A "$PGDATA" 2>/dev/null)" ]; then
        log "discarding a partly initialised PGDATA from an earlier attempt"
        rm -rf "${PGDATA:?}"/* "${PGDATA:?}"/.[!.]* 2>/dev/null || true
    fi

    # A node being provisioned clones whichever node is primary *now*. Only
    # when there is none -- a brand new cluster -- does node 1 initialise a new
    # primary. Without this check, rebuilding node 1 after a failover would
    # create a second primary beside the one Failover Manager promoted.
    primary="$(current_primary)"
    if [ -n "$primary" ]; then
        EFM_IS_FIRST=false
        EFM_PRIMARY_FQDN="$primary"
        log "${primary} is the primary; provisioning this node as its standby"
    fi

    if [ "$EFM_IS_FIRST" = "true" ]; then
        log "initialising the primary"
        pwfile="$(mktemp)"
        printf '%s\n' "$PGPASSWORD" > "$pwfile"
        chown postgres "$pwfile"
        as_postgres initdb --pgdata="$PGDATA" --username="$POSTGRES_USER" --pwfile="$pwfile" \
                           --encoding=UTF8 --locale=en_US.UTF-8 \
                           --auth-local=trust --auth-host=scram-sha-256 >/dev/null
        rm -f "$pwfile"

        write_hba "${PGDATA}/pg_hba.conf"
        chown postgres:postgres "${PGDATA}/pg_hba.conf"

        # listen_addresses '*' from the first start, as in the logical image.
        # wal_log_hints lets Failover Manager rewind a failed primary with
        # pg_rewind to rejoin it as a standby. wal_keep_size is for the other
        # standby after a failover: it is repointed at the new primary and
        # must find the WAL it still needs there. With the default of 0 and no
        # slot, the new primary had already removed it, and the standby
        # retried "requested WAL segment has already been removed" for ever.
        # The defaults for wal_level (replica), max_wal_senders and
        # hot_standby already suit streaming replication. Standbys copy this
        # file, so it is set once, here.
        cat >> "${PGDATA}/postgresql.conf" <<'EOF'

# --- cider-press ---------------------------------------------------------------
listen_addresses = '*'
wal_log_hints = on
wal_keep_size = 512MB
EOF

        as_postgres pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null
        if [ "$POSTGRES_DB" != "postgres" ]; then
            as_postgres psql -X -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d postgres -qc \
                "CREATE DATABASE \"${POSTGRES_DB}\"" >/dev/null \
                || die "could not create database ${POSTGRES_DB}"
            log "created database ${POSTGRES_DB}"
        fi
        as_postgres pg_ctl -D "$PGDATA" -m fast -w stop >/dev/null
    else
        log "waiting for the primary at ${EFM_PRIMARY_FQDN}"
        waited=0
        until as_postgres pg_isready -q -h "$EFM_PRIMARY_FQDN" -p 5432; do
            [ "$waited" -lt 300 ] || die "primary ${EFM_PRIMARY_FQDN} not ready after 300s"
            sleep 2
            waited=$(( waited + 2 ))
        done

        # -R writes standby.signal and a primary_conninfo from this connection
        # string, which is why the options below are in it. tcp_user_timeout
        # stops a dropped connection attempt hanging for the kernel's two
        # minutes (ARCHITECTURE.md, bite #7). application_name is how the
        # primary's pg_stat_replication names this standby.
        log "cloning the primary with pg_basebackup"
        as_postgres pg_basebackup --pgdata="$PGDATA" --wal-method=stream --write-recovery-conf \
            --checkpoint=fast \
            --dbname="host=${EFM_PRIMARY_FQDN} port=5432 user=${POSTGRES_USER} tcp_user_timeout=5000 application_name=${NODE_NAME}"
    fi

    as_postgres touch "$MARKER"
    log "provisioning complete"
else
    log "existing PGDATA found, skipping provisioning"
fi

# --- Failover Manager configuration, every start -----------------------------
# Written from the package's own template, because Failover Manager rejects
# unknown properties: editing the shipped file keeps every name valid. Every
# start, because the addresses come from /etc/hosts, which write_peer_hosts
# has just rebuilt with this container's current network prefix.

# This node's and every node's address, as Failover Manager wants them:
# "[ipv6]:port". IPv6 because those addresses are stable across restarts, and
# Failover Manager's bind.address and .nodes file take an address, not a name.
efm_addr() {
    local ip
    ip="$(getent ahostsv6 "$1" | awk 'NR == 1 {print $1}')"
    [ -n "$ip" ] || die "could not find an IPv6 address for $1 in /etc/hosts"
    printf '[%s]:%s' "$ip" "$EFM_AGENT_PORT"
}

# Set one property in the properties file, and insist it was there to set.
set_prop() {
    grep -q "^$1=" "$EFM_PROPS" || die "${EFM_PROPS} has no property '$1'"
    sed -i "s|^$1=.*|$1=$2|" "$EFM_PROPS"
}

install -d -o efm -g efm -m 0755 "/run/efm-${EFM_VERSION}"
install -d -o efm -g efm -m 0755 "$EFM_LOG_DIR"

# The lab password, handed to the agent through the external-script option
# added in 5.4, from a file only the agent can read.
install -o efm -g efm -m 0600 /dev/null "${EFM_ETC}/cider.password"
printf '%s\n' "$PGPASSWORD" > "${EFM_ETC}/cider.password"
cat > "${EFM_ETC}/cider-db-password" <<EOF
#!/bin/sh
exec cat ${EFM_ETC}/cider.password
EOF

# Notifications, which Failover Manager insists on, go to a file this script
# copies into the container log, so failovers show in `cider efm logs`.
cat > "${EFM_ETC}/cider-notify" <<EOF
#!/bin/sh
printf '%s EFM: %s -- %s\n' "\$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "\$1" "\$2" >> ${NOTIFICATIONS}
EOF
chmod 0755 "${EFM_ETC}/cider-db-password" "${EFM_ETC}/cider-notify"
install -o efm -g efm -m 0644 /dev/null "$NOTIFICATIONS"

cp "${EFM_ETC}/efm.properties.in" "$EFM_PROPS"
set_prop db.user "$POSTGRES_USER"
set_prop db.password.encrypted ""
set_prop script.db.password "${EFM_ETC}/cider-db-password"
set_prop script.db.password.encrypted false
set_prop db.port 5432
set_prop db.database "$POSTGRES_DB"
set_prop db.service.owner postgres
set_prop db.bin "$PG_BINDIR"
set_prop db.data.dir "$PGDATA"
set_prop bind.address "$(efm_addr "$NODE_FQDN")"
set_prop is.witness false
set_prop application.name "$NODE_NAME"
set_prop primary.health.check.port "$EFM_HEALTH_PORT"
set_prop script.notification "${EFM_ETC}/cider-notify"
set_prop user.email ""
# What the agent pings to check it is not cut off: something that should always
# be reachable and is not a cluster node. The default, 8.8.8.8, does not answer
# from inside these containers, and an agent that cannot reach it at startup
# exits. The load balancer is on the same network, is where clients come from,
# and starts before the nodes.
ping_ip="$(getent ahostsv6 "$EFM_PING_HOST" | awk 'NR == 1 {print $1}')"
[ -n "$ping_ip" ] || die "could not find an IPv6 address for ${EFM_PING_HOST} in /etc/hosts"
set_prop ping.server.ip "$ping_ip"
# The addresses never change, so let the first agent admit all of them and
# stop agents rewriting the .nodes file as members come and go. The docs
# recommend both for exactly this case.
set_prop auto.allow.hosts true
set_prop stable.nodes.file true
# After a failover, the failed primary's agent rebuilds its database as a
# standby of the new primary -- pg_rewind first, pg_basebackup if that fails --
# and the node rejoins by itself. The docs advise caution for production,
# where the cause of a failure is not known; in a lab it is the point.
# wal_log_hints, which pg_rewind needs, is on (see provisioning above). Off,
# a failed primary stays fenced until rebuilt by hand.
if [ "$EFM_AUTO_REJOIN" = "on" ]; then
    set_prop auto.rewind true
    set_prop auto.basebackup true
else
    set_prop auto.rewind false
    set_prop auto.basebackup false
fi

nodes=""
for fqdn in ${EFM_NODES//,/ }; do
    nodes="${nodes}$(efm_addr "$fqdn") "
done
printf '%s\n' "$nodes" > "$EFM_NODES_FILE"
# Readable by the postgres group too: the agent runs its database checks as
# postgres (sudo -u postgres efm_db_functions ...), and those read this file.
# It holds no secret -- the password is in its own efm-only file above.
chown efm:postgres "$EFM_PROPS" "$EFM_NODES_FILE"
chmod 0640 "$EFM_PROPS" "$EFM_NODES_FILE"
log "EFM bind.address $(efm_addr "$NODE_FQDN"); nodes: ${nodes}"

# --- One synchronous standby, every start --------------------------------------
# Every node lists every node, in order, as candidates for synchronous standby:
#   synchronous_standby_names = 'FIRST 1 (maeve-1, maeve-2, maeve-3)'
# Whichever node is primary then has exactly one synchronous standby -- the
# first connected one in that list -- and the other is "potential": not waited
# for, but synchronous at once if the first goes away. A node is never its own
# standby, so the same line is right on every node and after every failover,
# with nothing to rewrite. The names are each standby's application_name, which
# is its container name (application.name above, and in primary_conninfo).
#
# Which standby Failover Manager promotes is left to its default
# use.replay.tiebreaker: the one furthest ahead in replay. A synchronous
# standby is never behind on committed data, so a failover promotes it, or one
# exactly as current -- nothing committed is lost either way.
#
# Each name is double-quoted: they contain a hyphen, and unquoted the whole
# setting is a syntax error -- which makes postgresql.conf invalid and the node
# unbootable. So the line is also checked with `postgres -C`, which parses the
# configuration without starting a server, and removed again if it does not
# parse. A mistake here costs synchronous replication, not the node.
#
# Ensured on every start, not just at provisioning, so a data directory made
# before this setting existed gets it too. Written as postgres, so the file
# keeps its owner.
sync_names=""
for fqdn in ${EFM_NODES//,/ }; do
    sync_names="${sync_names:+${sync_names}, }\"${fqdn%%.*}\""
done
sync_line="synchronous_standby_names = 'FIRST 1 (${sync_names})'  # cider-press"
pg_conf="${PGDATA}/postgresql.conf"
if [ -f "$pg_conf" ] && ! grep -qxF "$sync_line" "$pg_conf"; then
    as_postgres sed -i '/^synchronous_standby_names = .*# cider-press$/d' "$pg_conf"
    printf '%s\n' "$sync_line" | as_postgres tee -a "$pg_conf" >/dev/null
    if as_postgres postgres -D "$PGDATA" -C synchronous_standby_names >/dev/null 2>&1; then
        log "one synchronous standby: FIRST 1 (${sync_names})"
    else
        as_postgres sed -i '/^synchronous_standby_names = .*# cider-press$/d' "$pg_conf"
        log "synchronous_standby_names did not parse; removed it, so replication stays asynchronous"
    fi
fi

# --- Supervise ----------------------------------------------------------------

shutdown() {
    trap - INT TERM
    log "stopping: EFM agent first, then postgres"
    as_efm "${EFM_HOME}/bin/runefm.sh" stop "$EFM_CLUSTER" >/dev/null 2>&1 || true
    if pg_up; then
        as_postgres pg_ctl -D "$PGDATA" -m fast -w -t 60 stop >/dev/null 2>&1 || true
    fi
    log "stopped"
    exit 0
}
trap shutdown INT TERM

if ! pg_up; then
    # A former primary that Failover Manager fenced refuses to start, by design,
    # until it is rebuilt as a standby. That is not a reason to stop the
    # container: the agent should still run.
    if as_postgres pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null; then
        log "postgres started"
    else
        log "postgres did not start; leaving it to Failover Manager (see $CIDER_PRESS_LOG)"
    fi
fi

# Copy notifications into the container log as they arrive.
tail -n 0 -F "$NOTIFICATIONS" 2>/dev/null &

log "starting the EFM agent"
as_efm "${EFM_HOME}/bin/runefm.sh" start "$EFM_CLUSTER" \
    || die "the EFM agent did not start; see ${EFM_LOG_DIR}/startup-${EFM_CLUSTER}.log"
log "EFM agent running; health endpoint on port ${EFM_HEALTH_PORT}"

# Wait for the stop signal. `sleep & wait` rather than a bare sleep, so the
# trap runs as soon as the signal arrives.
while :; do
    sleep 5 &
    wait $! || true
    if [ ! -s "$EFM_PID" ] || ! kill -0 "$(cat "$EFM_PID")" 2>/dev/null; then
        log "the EFM agent has exited; see ${EFM_LOG_DIR}/${EFM_CLUSTER}.log"
        shutdown
    fi
done
