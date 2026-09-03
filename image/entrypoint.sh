#!/usr/bin/env bash
#
# cider-press node entrypoint.
#
# Runs first as root (to fix ownership on the freshly-mounted volume), then
# re-execs itself as the Postgres superuser so that `postgres` ends up as PID 1
# and receives signals directly.
#
# Every address used here is fully qualified (host-1.cider, not host-1).
# Apple's `container` resolves container names only through its embedded DNS
# service as <name>.<domain>; bare hostnames are not guaranteed to resolve
# (apple/container#1809). PGD writes --listen-addr into the cluster catalog as
# the address peers dial, so a name that resolves only sometimes would produce
# a cluster that half-works after a restart.

set -euo pipefail

log()  { printf '%s [cider-press] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }
die()  { printf '%s [cider-press] ERROR: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; exit 1; }

# shellcheck disable=SC1091
. /etc/cider-press/image.env
export PATH="/opt/cider-press/bin:${PATH}"

# ---------------------------------------------------------------------------
# Stage 1: root-only work, then drop privileges.
# ---------------------------------------------------------------------------
if [ "$(id -u)" = "0" ]; then
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 /var/lib/cider-press
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0700 "$PGDATA"
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 "$(dirname "$CIDER_PRESS_LOG")"
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 /etc/edb/pgd-cli

    # A named volume arrives root-owned; without this the first initdb fails.
    chown -R "$PG_SUPERUSER":"$PG_SUPERUSER" /var/lib/cider-press

    log "dropping to ${PG_SUPERUSER}"
    exec setpriv --reuid="$PG_SUPERUSER" --regid="$PG_SUPERUSER" --init-groups \
         --inh-caps=-all -- "$0" "$@"
fi

# ---------------------------------------------------------------------------
# Stage 2: running as the Postgres superuser.
# ---------------------------------------------------------------------------

: "${PGD_NODE_NAME:?PGD_NODE_NAME is required}"
: "${PGD_HOST_FQDN:?PGD_HOST_FQDN is required}"
: "${PGD_JOIN_DSN:?PGD_JOIN_DSN is required}"
: "${PGD_ALL_HOSTS:?PGD_ALL_HOSTS is required}"

PGD_IS_FIRST="${PGD_IS_FIRST:-false}"
PGD_GROUP_NAME="${PGD_GROUP_NAME:-group-1}"
PGD_CLUSTER_NAME="${PGD_CLUSTER_NAME:-cider}"
PGD_INITIAL_NODE_COUNT="${PGD_INITIAL_NODE_COUNT:-3}"
POSTGRES_DB="${POSTGRES_DB:-pgddb}"
POSTGRES_USER="${POSTGRES_USER:-$PG_SUPERUSER}"
PGD_JOIN_TIMEOUT="${PGD_JOIN_TIMEOUT:-300}"

# `pgd node setup` authenticates with this.
export PGPASSWORD="${PGPASSWORD:-${POSTGRES_PASSWORD:-}}"
[ -n "$PGPASSWORD" ] || die "PGPASSWORD (or POSTGRES_PASSWORD) is required"

SELF_DSN="host=${PGD_HOST_FQDN} port=5432 dbname=${POSTGRES_DB} user=${POSTGRES_USER}"

log "node=${PGD_NODE_NAME} host=${PGD_HOST_FQDN} group=${PGD_GROUP_NAME} cluster=${PGD_CLUSTER_NAME}"
log "flavor=${PG_FLAVOR} pg=${PG_MAJOR} first=${PGD_IS_FIRST}"

# --- pgd CLI config --------------------------------------------------------
# PGD_ALL_HOSTS is a comma-separated list of fully-qualified node addresses.
{
    echo "cluster:"
    echo "  name: ${PGD_CLUSTER_NAME}"
    echo "  endpoints:"
    # Trailing newline matters: without it `read` drops the last host.
    printf '%s\n' "$PGD_ALL_HOSTS" | tr ',' '\n' | while IFS= read -r h; do
        [ -n "$h" ] || continue
        echo "    - host=${h} dbname=${POSTGRES_DB} port=5432 user=${POSTGRES_USER}"
    done
} > /etc/edb/pgd-cli/pgd-cli-config.yml

# --- Wait until this node can resolve its own name --------------------------
# postgres refuses to start if a name in listen_addresses does not resolve, and
# the container runtime registers a container in its DNS asynchronously, so this
# races container start. Registration is normally sub-second, but it has been
# observed to take much longer; the timeout is generous because waiting costs
# nothing when things are fast, and a spurious failure here is expensive.
PGD_SELF_RESOLVE_TIMEOUT="${PGD_SELF_RESOLVE_TIMEOUT:-180}"

wait_for_self() {
    local deadline=$(( SECONDS + PGD_SELF_RESOLVE_TIMEOUT ))
    local waited=0
    while [ "$SECONDS" -lt "$deadline" ]; do
        if getent hosts "$PGD_HOST_FQDN" >/dev/null 2>&1; then
            [ "$waited" -gt 5 ] && log "own name took ${waited}s to register"
            log "resolved own name ${PGD_HOST_FQDN} -> $(getent hosts "$PGD_HOST_FQDN" | awk '{print $1}' | head -1)"
            return 0
        fi
        sleep 1
        waited=$(( waited + 1 ))
    done

    # Two very different causes, and pointing at the wrong one wastes real time.
    # If resolv.conf carries our domain then DNS *is* configured and the
    # registration simply did not land -- retrying almost always works.
    local domain="${PGD_HOST_FQDN#*.}"
    if grep -qE "^[[:space:]]*(search|domain)[[:space:]]+.*(^|[[:space:]])${domain}([[:space:]]|\$)" \
         /etc/resolv.conf 2>/dev/null; then
        die "this node did not register in the runtime's DNS within ${PGD_SELF_RESOLVE_TIMEOUT}s.

     The DNS domain IS configured (resolv.conf carries '${domain}'), so this is
     a transient registration delay rather than a setup problem -- 'cider doctor'
     will report everything healthy. Simply run 'cider pgd up' again; it restarts
     this node and normally succeeds immediately."
    else
        die "could not resolve own name ${PGD_HOST_FQDN} after ${PGD_SELF_RESOLVE_TIMEOUT}s,
     and '${domain}' is not in this container's resolv.conf at all.

     The container DNS domain is probably not configured. On the host run:
       cider doctor"
    fi
}

# --- Wait for the seed node to be ready to accept a join --------------------
wait_for_seed() {
    local deadline=$(( SECONDS + PGD_JOIN_TIMEOUT ))
    log "waiting for seed node via: ${PGD_JOIN_DSN}"
    while [ "$SECONDS" -lt "$deadline" ]; do
        if pg_isready -d "$PGD_JOIN_DSN" >/dev/null 2>&1 \
           && psql -d "$PGD_JOIN_DSN" -tAqc \
                "select 1 from bdr.local_node_summary limit 1" >/dev/null 2>&1; then
            log "seed node is up and running PGD"
            return 0
        fi
        sleep 2
    done
    die "seed node not ready after ${PGD_JOIN_TIMEOUT}s: ${PGD_JOIN_DSN}"
}

wait_for_self

# --- pg_hba -----------------------------------------------------------------
# Apple `container` gives every node both an IPv4 and an IPv6 address, and its
# DNS answers <host>.<domain> with the IPv6 one. The pg_hba.conf that
# `pgd node setup` generates only covers 0.0.0.0/0, so every by-name connection
# between nodes is rejected with "no pg_hba.conf entry for host ...".
#
# This is that generated file plus the two ::/0 lines.
HBA_FILE="/var/lib/cider-press/pg_hba.cider.conf"
write_hba() {
    cat > "$HBA_FILE" <<'EOF'
local   all             all                                     trust
host    all             all             127.0.0.1/32            trust
host    all             all             ::1/128                 trust
local   replication     all                                     trust
host    replication     all             127.0.0.1/32            trust
host    replication     all             ::1/128                 trust
host    replication     all             0.0.0.0/0               scram-sha-256
host    replication     all             ::/0                    scram-sha-256
host    all             all             0.0.0.0/0               scram-sha-256
host    all             all             ::/0                    scram-sha-256
EOF
    chmod 0600 "$HBA_FILE"
}
write_hba

# --- Provision, once ---------------------------------------------------------
if [ ! -s "${PGDATA}/PG_VERSION" ]; then
    if [ "$PGD_IS_FIRST" = "true" ]; then
        log "creating new cluster '${PGD_CLUSTER_NAME}', group '${PGD_GROUP_NAME}'"
        pgd node "$PGD_NODE_NAME" setup --verbose \
            --dsn "$SELF_DSN" \
            --listen-addr "${PGD_HOST_FQDN},localhost" \
            --initial-node-count "$PGD_INITIAL_NODE_COUNT" \
            --hba-conf "$HBA_FILE" \
            --pgdata "$PGDATA" \
            --log-file "$CIDER_PRESS_LOG" \
            --cluster-name "$PGD_CLUSTER_NAME" \
            --group-name "$PGD_GROUP_NAME"
    else
        wait_for_seed

        # Re-running `cider up` after a failed join can leave a tombstone for
        # this node name in the cluster catalog; clear it before rejoining.
        log "clearing any stale catalog entry for ${PGD_NODE_NAME}"
        psql -d "$PGD_JOIN_DSN" -qc \
            "SELECT bdr.run_on_all_nodes(\$\$ SELECT bdr.drop_node('${PGD_NODE_NAME}', force := true) \$\$);" \
            >/dev/null 2>&1 || true

        log "joining cluster '${PGD_CLUSTER_NAME}' as ${PGD_NODE_NAME}"
        if ! pgd node "$PGD_NODE_NAME" setup --verbose \
                --dsn "$SELF_DSN" \
                --listen-addr "${PGD_HOST_FQDN},localhost" \
                --hba-conf "$HBA_FILE" \
                --pgdata "$PGDATA" \
                --log-file "$CIDER_PRESS_LOG" \
                --cluster-dsn "$PGD_JOIN_DSN" \
                --cluster-name "$PGD_CLUSTER_NAME" \
                --group-name "$PGD_GROUP_NAME"; then
            # A half-initialised PGDATA would make the next start look
            # "already provisioned" and fail in a much more confusing way.
            log "join failed; discarding partial PGDATA so a retry starts clean"
            rm -rf "${PGDATA:?}"/* "${PGDATA:?}"/.[!.]* 2>/dev/null || true
            exit 1
        fi
    fi
    log "provisioning complete"
else
    log "existing PGDATA found, skipping provisioning"
fi

# `pgd node setup` leaves the server running under its own supervision; stop it
# so it can be re-exec'd as PID 1.
pg_ctl -D "$PGDATA" -m fast stop >/dev/null 2>&1 || true

# --- PGD Monitor (the 6.5.0 web UI) ----------------------------------------
# bdr.monitor_enabled is defined by the bdr extension, so `postgres -C` cannot
# see it (that path does not load shared_preload_libraries) and reports it as
# unrecognised. Ask a running server instead, which is also what validates the
# value before it is written.
# Postgres resolves the names in listen_addresses once, at startup. Apple
# container registers a node's A and AAAA records moments apart, so a node that
# starts early can bind IPv4 only while its peers bind both -- and then Raft
# consensus connections to it over IPv6 fail and the node shows Unreachable.
#
# Listening on every address removes the race. --listen-addr still carries the
# fully-qualified name, so the address peers dial is unchanged.
AUTOCONF="${PGDATA}/postgresql.auto.conf"

has_setting() { grep -qE "^[[:space:]]*$1[[:space:]]*=" "$AUTOCONF" 2>/dev/null; }

needs_config() {
    grep -qE "^[[:space:]]*listen_addresses[[:space:]]*=[[:space:]]*'\*'" "$AUTOCONF" 2>/dev/null || return 0
    [ "${PGD_MONITOR_ENABLED:-on}" = "on" ] && ! has_setting 'bdr\.monitor_enabled' && return 0
    [ "${PGD_STAT_STATEMENTS:-on}" = "on" ] && ! has_setting 'shared_preload_libraries' && return 0
    return 1
}

run_sql() {
    psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qc "$1" >/dev/null 2>&1
}
query_sql() {
    psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAqc "$1" 2>/dev/null
}

if needs_config; then
    if ! pg_isready -q -h 127.0.0.1 -p 5432 2>/dev/null; then
        pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null 2>&1 || true
    fi

    if pg_isready -q -h 127.0.0.1 -p 5432 2>/dev/null; then
        # A fixed literal, never a value read back and re-assembled.
        run_sql "ALTER SYSTEM SET listen_addresses = '*'" \
            && log "listening on all addresses (IPv4 and IPv6)" \
            || log "could not set listen_addresses"

        if [ "${PGD_MONITOR_ENABLED:-on}" = "on" ] && ! has_setting 'bdr\.monitor_enabled'; then
            if [ "$(query_sql "select 1 from pg_settings where name = 'bdr.monitor_enabled'" | tr -d '[:space:]')" = "1" ]; then
                run_sql "ALTER SYSTEM SET bdr.monitor_enabled = 'on'" \
                    && log "PGD Monitor enabled — web UI on port $(( 5432 + 1005 ))" \
                    || log "could not enable PGD Monitor"
            else
                log "bdr.monitor_enabled not available in this PGD build; skipping web UI"
            fi
        fi

        # pg_stat_statements powers the web UI's Query Diagnostics page.
        #
        # shared_preload_libraries is a list GUC, and `ALTER SYSTEM SET x = 'a, b'`
        # stores the whole string as ONE library name -- which is how an earlier
        # version of this file produced a node that would not boot. Each element
        # must be passed as its own SQL value: SET x = 'a', 'b'.
        #
        # Even so, the value belongs to PGD, so the change is verified by an
        # actual restart and rolled back if the server does not come up. A lab
        # without one optional UI page beats a lab that will not start.
        if [ "${PGD_STAT_STATEMENTS:-on}" = "on" ]; then
            spl="$(query_sql 'show shared_preload_libraries')"
            case ",$(printf '%s' "$spl" | tr -d '[:space:]')," in
                *,pg_stat_statements,*)
                    log "pg_stat_statements already preloaded" ;;
                *)
                    if [ -n "$spl" ]; then
                        cp -p "$AUTOCONF" "${AUTOCONF}.cider-press.bak" 2>/dev/null || true
                        # Double any single quotes before embedding in SQL.
                        esc="$(printf '%s' "$spl" | sed "s/'/''/g")"
                        run_sql "ALTER SYSTEM SET shared_preload_libraries = '${esc}', 'pg_stat_statements'"

                        pg_ctl -D "$PGDATA" -m fast stop >/dev/null 2>&1 || true
                        if pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null 2>&1; then
                            log "pg_stat_statements preloaded"
                            rm -f "${AUTOCONF}.cider-press.bak"
                        else
                            log "pg_stat_statements broke startup; reverting"
                            if [ -f "${AUTOCONF}.cider-press.bak" ]; then
                                mv -f "${AUTOCONF}.cider-press.bak" "$AUTOCONF"
                            else
                                sed -i '/^[[:space:]]*shared_preload_libraries[[:space:]]*=/d' "$AUTOCONF"
                            fi
                            pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null 2>&1 || true
                        fi
                    fi ;;
            esac
        fi
    else
        log "could not start a server to apply configuration; skipping"
    fi

    # Always leave the server down; it is re-exec'd as PID 1 below.
    pg_ctl -D "$PGDATA" -m fast stop >/dev/null 2>&1 || true
fi

log "starting postgres"
exec postgres -D "$PGDATA"
