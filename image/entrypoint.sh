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
#
# What is not PGD-specific -- logging, the privilege drop, waiting for our own
# name, pg_hba, listen_addresses -- is in lib/node-common.sh, shared with every
# product's image. What is left here is PGD's own.

set -euo pipefail

# shellcheck disable=SC1091
. /etc/cider-press/image.env
export PATH="/opt/cider-press/bin:${PATH}"

CIDER_GROUP="pgd"
# The image path; shellcheck is pointed at the source-tree copy instead.
# shellcheck source-path=SCRIPTDIR source=lib/node-common.sh
. /usr/local/lib/cider-press/node-common.sh

# ---------------------------------------------------------------------------
# Stage 1: root-only work, then drop privileges.
# ---------------------------------------------------------------------------
if [ "$(id -u)" = "0" ]; then
    prepare_state_dirs
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 /etc/edb/pgd-cli
    write_peer_hosts
    become_superuser "$@"
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
# PGD puts the fully-qualified name in listen_addresses via --listen-addr, so
# postgres will not start until it resolves. See wait_for_self in
# lib/node-common.sh.
PGD_SELF_RESOLVE_TIMEOUT="${PGD_SELF_RESOLVE_TIMEOUT:-180}"

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

wait_for_self "$PGD_HOST_FQDN" "$PGD_SELF_RESOLVE_TIMEOUT"

# --- pg_hba -----------------------------------------------------------------
# The file `pgd node setup` would generate, plus the ::/0 lines it lacks; see
# write_hba in lib/node-common.sh. Handed to `pgd node setup` via --hba-conf.
HBA_FILE="${CIDER_STATE_DIR}/pg_hba.cider.conf"
write_hba "$HBA_FILE"

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

# --- Post-provisioning configuration ----------------------------------------
# Three settings, each applied once and then found in postgresql.auto.conf on
# every later start: listen_addresses (see lib/node-common.sh), the PGD
# Monitor, and pg_stat_statements. They need a running server, so one is
# started only if something is missing.
#
# bdr.monitor_enabled is defined by the bdr extension, so `postgres -C` cannot
# see it (that path does not load shared_preload_libraries) and reports it as
# unrecognised. Asking a running server is also what validates the value
# before it is written.
needs_config() {
    listens_on_all_addresses || return 0
    [ "${PGD_MONITOR_ENABLED:-on}" = "on" ] && ! has_setting 'bdr\.monitor_enabled' && return 0
    [ "${PGD_STAT_STATEMENTS:-on}" = "on" ] && ! has_setting 'shared_preload_libraries' && return 0
    return 1
}

if needs_config; then
    if ! pg_isready -q -h 127.0.0.1 -p 5432 2>/dev/null; then
        pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null 2>&1 || true
    fi

    if pg_isready -q -h 127.0.0.1 -p 5432 2>/dev/null; then
        listen_on_all_addresses

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
