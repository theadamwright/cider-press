#!/usr/bin/env bash
#
# cider-press node entrypoint for `cider logical`: one community PostgreSQL
# node, ready to publish and to subscribe.
#
# Runs first as root (to fix ownership on the freshly-mounted volume), then
# re-execs itself as postgres so that `postgres` ends up as PID 1.
#
# Much simpler than PGD's entrypoint, because there is no cluster to join: each
# node initialises its own data directory, and the replication between the two
# is left for you to create with CREATE PUBLICATION and CREATE SUBSCRIPTION.
# What every product shares is in lib/node-common.sh.

set -euo pipefail

# shellcheck disable=SC1091
. /etc/cider-press/image.env
export PATH="/opt/cider-press/bin:${PATH}"

CIDER_GROUP="logical"
# The image path; shellcheck is pointed at the source-tree copy instead.
# shellcheck source-path=SCRIPTDIR source=lib/node-common.sh
. /usr/local/lib/cider-press/node-common.sh

# ---------------------------------------------------------------------------
# Stage 1: root-only work, then drop privileges.
# ---------------------------------------------------------------------------
if [ "$(id -u)" = "0" ]; then
    prepare_state_dirs
    become_superuser "$@"
fi

# ---------------------------------------------------------------------------
# Stage 2: running as postgres.
# ---------------------------------------------------------------------------

: "${NODE_FQDN:?NODE_FQDN is required}"
POSTGRES_DB="${POSTGRES_DB:-demo}"
POSTGRES_USER="${POSTGRES_USER:-postgres}"

# Exported, and so inherited by the server `exec`'d below. That is what lets a
# subscription connect to its peer without a password in its CONNECTION
# string: the apply worker's libpq finds PGPASSWORD in the server's
# environment. (A superuser's subscription may do this; others must put
# password= in the connection string.)
export PGPASSWORD="${PGPASSWORD:-}"
[ -n "$PGPASSWORD" ] || die "PGPASSWORD is required"

log "node=${NODE_FQDN} pg=${PG_MAJOR} db=${POSTGRES_DB}"

# --- Provision, once ---------------------------------------------------------
# Finished provisioning is recorded with a marker, not detected from
# PG_VERSION: initdb writes PG_VERSION early, so a first start that died half
# way through would look provisioned on the next one and fail confusingly.
# Without the marker, whatever is there is discarded and done again -- which is
# also what makes `up`'s automatic retry of a failed node safe for this image.
MARKER="${PGDATA}/.cider-press-provisioned"

if [ ! -f "$MARKER" ]; then
    if [ -n "$(ls -A "$PGDATA" 2>/dev/null)" ]; then
        log "discarding a partly initialised PGDATA from an earlier attempt"
        rm -rf "${PGDATA:?}"/* "${PGDATA:?}"/.[!.]* 2>/dev/null || true
    fi

    log "initialising a PostgreSQL ${PG_MAJOR} data directory"
    pwfile="$(mktemp)"
    printf '%s\n' "$PGPASSWORD" > "$pwfile"
    initdb --pgdata="$PGDATA" --username="$POSTGRES_USER" --pwfile="$pwfile" \
           --encoding=UTF8 --locale=en_US.UTF-8 \
           --auth-local=trust --auth-host=scram-sha-256 >/dev/null
    rm -f "$pwfile"

    write_hba "${PGDATA}/pg_hba.conf"

    # Written into postgresql.conf before the first start, rather than set with
    # ALTER SYSTEM on a running server as PGD's entrypoint has to. With
    # listen_addresses '*' from the very first start, nothing needs to resolve
    # this node's own name at boot, so the runtime's DNS registration delay
    # (ARCHITECTURE.md, bite #5) cannot stop Postgres starting.
    cat >> "${PGDATA}/postgresql.conf" <<'EOF'

# --- cider-press ---------------------------------------------------------------
listen_addresses = '*'
# What a publication needs. The defaults for max_replication_slots and
# max_wal_senders (10 each) are plenty for a pair.
wal_level = logical
EOF

    pg_ctl -D "$PGDATA" -l "$CIDER_PRESS_LOG" -w -t 60 start >/dev/null
    if [ "$POSTGRES_DB" != "postgres" ]; then
        # Not run_sql: that connects to $POSTGRES_DB, which does not exist yet.
        psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d postgres -qc \
             "CREATE DATABASE \"${POSTGRES_DB}\"" >/dev/null \
            || die "could not create database ${POSTGRES_DB}"
        log "created database ${POSTGRES_DB}"
    fi
    pg_ctl -D "$PGDATA" -m fast -w stop >/dev/null

    touch "$MARKER"
    log "provisioning complete"
else
    log "existing PGDATA found, skipping provisioning"
fi

log "starting postgres"
exec postgres -D "$PGDATA"
