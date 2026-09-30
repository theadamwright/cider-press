# shellcheck shell=bash
#
# cider-press: what every node image's entrypoint shares.
#
# Sourced by an entrypoint, never run on its own. Each function here exists
# because of a specific failure on Apple's container runtime -- see
# ARCHITECTURE.md, "Six things that will bite you" -- and lives in one file so
# that a fix reaches every product's image at once instead of being copied.
#
# The entrypoint must set, before sourcing:
#   CIDER_GROUP    the `cider <group>` the image belongs to, used in hints
# and the image must provide (see image.env and the Dockerfile's ENV):
#   PG_SUPERUSER  PGDATA  CIDER_PRESS_LOG
# The SQL helpers also read POSTGRES_USER and POSTGRES_DB.

log()  { printf '%s [cider-press] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }
die()  { printf '%s [cider-press] ERROR: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >&2; exit 1; }

# Every node's state lives under this one mount point, whatever the product:
# the named volume is mounted here, so PGDATA and the server log travel
# together and survive `cider <group> down`.
CIDER_STATE_DIR="/var/lib/cider-press"

# --- Root stage ---------------------------------------------------------------

# Give the freshly mounted volume to the Postgres superuser.
#
# A named volume arrives root-owned; without this the first initdb fails.
prepare_state_dirs() {
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 "$CIDER_STATE_DIR"
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0700 "$PGDATA"
    install -d -o "$PG_SUPERUSER" -g "$PG_SUPERUSER" -m 0750 "$(dirname "$CIDER_PRESS_LOG")"
    chown -R "$PG_SUPERUSER":"$PG_SUPERUSER" "$CIDER_STATE_DIR"
}

# Pin every node's name to its stable IPv6 address in /etc/hosts. Root only;
# call on every start, before anything resolves a peer.
#
# The runtime's DNS often takes a minute or more to register a container's name
# after it starts (ARCHITECTURE.md, bite #5), so nodes do not wait for it.
# cider gives each container a fixed MAC, which fixes the low 64 bits of its
# IPv6 address, and passes every node's name and those bits in CIDER_PEERS:
#   host-1.cider=0074:56ff:fe18:cc15,host-2.cider=...
# This adds the network's /64 prefix, read from this container's own address,
# and writes the result. Name lookup checks /etc/hosts before DNS, so every
# peer resolves at once. Reading the prefix here, rather than being told it,
# means a new prefix from the runtime is picked up on the next start.
#
# Every start, because the runtime rewrites /etc/hosts on each start. Without
# CIDER_PEERS (an older cider) it does nothing, and the runtime's DNS is used as
# before.
write_peer_hosts() {
    [ -n "${CIDER_PEERS:-}" ] || return 0

    # This container's global IPv6 address, as 32 hex digits. Normally present
    # by now; SLAAC can take a moment, so wait up to five seconds.
    local hex="" tries=0
    while [ -z "$hex" ] && [ "$tries" -lt 50 ]; do
        hex="$(awk '$4 == "00" && $6 != "lo" {print $1; exit}' /proc/net/if_inet6 2>/dev/null)"
        [ -n "$hex" ] || { sleep 0.1; tries=$(( tries + 1 )); }
    done
    if [ -z "$hex" ]; then
        log "no global IPv6 address; peers will be resolved through the runtime's DNS"
        return 0
    fi
    local prefix="${hex:0:4}:${hex:4:4}:${hex:8:4}:${hex:12:4}"

    # Replace, never append twice: keep the runtime's own lines, drop any of
    # ours from an earlier start, then add the current set.
    local tmp="/etc/hosts.cider-press" entry count=0
    grep -v '# cider-press$' /etc/hosts > "$tmp" || true
    for entry in ${CIDER_PEERS//,/ }; do
        printf '%s:%s\t%s\t# cider-press\n' "$prefix" "${entry#*=}" "${entry%%=*}" >> "$tmp"
        count=$(( count + 1 ))
    done
    # Copied over rather than moved: the file keeps its inode, in case the
    # runtime is watching it.
    cat "$tmp" > /etc/hosts
    rm -f "$tmp"
    log "pinned ${count} node names to their IPv6 addresses (${prefix}::/64) in /etc/hosts"
}

# Re-exec the calling entrypoint as the Postgres superuser. Call as
#   become_superuser "$@"
# with the entrypoint's own arguments. Does not return.
#
# setpriv rather than su or gosu: it replaces this process instead of forking,
# so `postgres` ends up as PID 1 and receives the container's stop signal
# directly. ($0 inside a function is still the entrypoint's path.)
become_superuser() {
    log "dropping to ${PG_SUPERUSER}"
    exec setpriv --reuid="$PG_SUPERUSER" --regid="$PG_SUPERUSER" --init-groups \
         --inh-caps=-all -- "$0" "$@"
}

# --- Waiting for our own name --------------------------------------------------

# Wait until this node can resolve its own fully-qualified name.
#   wait_for_self FQDN TIMEOUT_SECONDS
#
# postgres refuses to start if a name in listen_addresses does not resolve, and
# the container runtime registers a container in its DNS asynchronously, so this
# races container start. Registration is normally sub-second, but it has been
# observed to take much longer; the timeout is generous because waiting costs
# nothing when things are fast, and a spurious failure here is expensive.
wait_for_self() {
    local fqdn="$1" timeout="$2"
    local deadline=$(( SECONDS + timeout ))
    local waited=0
    while [ "$SECONDS" -lt "$deadline" ]; do
        if getent hosts "$fqdn" >/dev/null 2>&1; then
            [ "$waited" -gt 5 ] && log "own name took ${waited}s to register"
            log "resolved own name ${fqdn} -> $(getent hosts "$fqdn" | awk '{print $1}' | head -1)"
            return 0
        fi
        sleep 1
        waited=$(( waited + 1 ))
    done

    # Two very different causes, and pointing at the wrong one wastes real time.
    # If resolv.conf carries our domain then DNS *is* configured and the
    # registration simply did not land -- retrying almost always works.
    local domain="${fqdn#*.}"
    if grep -qE "^[[:space:]]*(search|domain)[[:space:]]+.*(^|[[:space:]])${domain}([[:space:]]|\$)" \
         /etc/resolv.conf 2>/dev/null; then
        die "this node did not register in the runtime's DNS within ${timeout}s.

     The DNS domain IS configured (resolv.conf carries '${domain}'), so this is
     a transient registration delay rather than a setup problem -- 'cider doctor'
     will report everything healthy. Simply run 'cider ${CIDER_GROUP} up' again; it
     restarts this node and normally succeeds immediately."
    else
        die "could not resolve own name ${fqdn} after ${timeout}s,
     and '${domain}' is not in this container's resolv.conf at all.

     The container DNS domain is probably not configured. On the host run:
       cider doctor"
    fi
}

# --- pg_hba --------------------------------------------------------------------

# Write the pg_hba.conf every node uses.
#   write_hba PATH
#
# Apple `container` gives every node both an IPv4 and an IPv6 address, and its
# DNS answers <host>.<domain> with the IPv6 one. A pg_hba.conf that covers only
# 0.0.0.0/0 -- which is what `pgd node setup` generates, and what most examples
# show -- rejects every by-name connection between nodes with "no pg_hba.conf
# entry for host ...". Hence the two ::/0 lines.
#
# `replication` lines cover physical replication only. Logical replication
# connects to a named database, so the `all all` lines are the ones that let a
# subscription in.
write_hba() {
    cat > "$1" <<'EOF'
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
    chmod 0600 "$1"
}

# --- Talking to the local server -----------------------------------------------

AUTOCONF="${PGDATA}/postgresql.auto.conf"

# Is SETTING already written to postgresql.auto.conf? SETTING is an ERE, so
# escape dots: has_setting 'bdr\.monitor_enabled'.
has_setting() { grep -qE "^[[:space:]]*$1[[:space:]]*=" "$AUTOCONF" 2>/dev/null; }

run_sql() {
    psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qc "$1" >/dev/null 2>&1
}
query_sql() {
    psql -h 127.0.0.1 -p 5432 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAqc "$1" 2>/dev/null
}

# --- listen_addresses ----------------------------------------------------------

# Postgres resolves the names in listen_addresses once, at startup. Apple
# container registers a node's A and AAAA records moments apart, so a node that
# starts early can bind IPv4 only while its peers bind both -- and then peers
# dialling it over IPv6 get nothing. Under PGD that shows up as the node stuck
# Unreachable with Raft consensus failing.
#
# Listening on every address removes the race. The address peers *dial* is set
# elsewhere (for PGD, --listen-addr), so it is unchanged.

# Is listen_addresses already '*' in postgresql.auto.conf?
listens_on_all_addresses() {
    grep -qE "^[[:space:]]*listen_addresses[[:space:]]*=[[:space:]]*'\*'" "$AUTOCONF" 2>/dev/null
}

# Set it, on a running server. A fixed literal, never a value read back and
# re-assembled.
listen_on_all_addresses() {
    run_sql "ALTER SYSTEM SET listen_addresses = '*'" \
        && log "listening on all addresses (IPv4 and IPv6)" \
        || log "could not set listen_addresses"
}
