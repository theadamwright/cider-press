# Architecture

Notes for anyone working *on* cider-press. For using it, see [README.md](README.md).

## What this is

A CLI that stands up clusters on Apple's `container` runtime: a multi-node EDB
Postgres Distributed cluster (`cider pgd`), a pair of community PostgreSQL nodes
for logical replication (`cider logical`), and a primary and two standbys under
EDB Failover Manager behind a load balancer (`cider efm`). It owns two things
and defers everything else:

1. **`src/`** — a Rust CLI that shells out to `container` and to the `pgd` CLI.
   It holds no cluster logic of its own; it orchestrates.
2. **`image/`** — three Debian images, each with an entrypoint that provisions
   one node: PGD's (`Dockerfile`, `entrypoint.sh`), logical's
   (`logical.Dockerfile`, `logical-entrypoint.sh`) and EFM's (`efm.Dockerfile`,
   `efm-entrypoint.sh`, plus `efm-lb-entrypoint.sh` for its load balancer). All
   source `lib/node-common.sh`, the helpers any product's entrypoint shares.

Everything about *how PGD works* lives in `image/entrypoint.sh`. Everything
about *how the Mac and the runtime work* lives in `src/`, and, inside a node,
in `image/lib/node-common.sh`. Keeping that line clean is what makes each part
readable on its own.

## Where to start reading

If you're new to this, roughly an hour in this order:

1. **`README.md`** — use it first. Run `cider doctor`, then `up`. Nothing below
   makes sense until you've seen a cluster come up.
2. **`src/main.rs`** — the whole command surface in one file. Every verb is an
   enum arm pointing at one function.
3. **`src/config.rs`** — the naming and port schemes, both with worked examples
   in comments. `host-1` vs `host-1.cider` vs `node-1` is the distinction that
   trips people up.
4. **`image/entrypoint.sh`** — what actually happens inside a node: provision or
   join, then configure. This is where the PGD knowledge lives. The runtime
   workarounds it calls (privilege drop, waiting for its own name, `pg_hba`,
   `listen_addresses`) are in `image/lib/node-common.sh`.
5. **`src/pgd.rs`** — PGD's verbs. Start at `up()` and follow it down; the
   shared parts it calls into (start-and-wait, retry, stop) are in
   `src/lifecycle.rs`.

If PGD is more than you want to take in at once, read `src/logical.rs` and
`image/logical-entrypoint.sh` first instead. They're the same shape with no
cluster to join, so the lifecycle is easier to see.

Then come back to *Eight things that will bite you* below, which will make a lot
more sense once you've seen the moving parts.

## Module map

| Module | Responsibility |
|---|---|
| `config.rs` | Every tunable, resolved once from env/`.env`: shared settings on `Config`, each product's on `cfg.pgd`, `cfg.logical` and `cfg.efm`. Where every name is formed, and the port maths. No I/O beyond reading env. |
| `container.rs` | **The only** module that runs `container` or parses its output. If the runtime changes its CLI, this is the blast radius. |
| `doctor.rs` | Preflight checks, ordered so the first failure is the root cause. |
| `bootstrap.rs` | One-time host setup: the container DNS domain, the macOS resolver. Edits a file the user owns, so it backs up and verifies. |
| `lifecycle.rs` | What every product shares: build the image, start a node and wait for it (with the retry), stop, start, down, `pomace`, and stable addresses (fixed MACs, `CIDER_PEERS`). Knows nothing about what runs inside a node; a product describes its nodes with a `Deployment` and passes in how to create one and how to tell it is ready. A `Deployment` can also have *extras*: containers that aren't numbered nodes, have no volume, start before the nodes and stop after them, like EFM's load balancer. |
| `pgd.rs` | PGD's half: its container flags and environment, join readiness, pooling, `pg_stat_statements`, status, endpoints, the web UI. |
| `logical.rs` | `cider logical`'s half: its containers, readiness (`wal_level = logical` *and* the name in DNS), a status view of publications, subscriptions and apply errors from public catalogs, and the wiring SQL it prints. Creates no replication itself. |
| `efm.rs` | `cider efm`'s half: the nodes and the load balancer, readiness (Postgres up *and* the agent answering its health endpoint), a status view of each node's role and where the load balancer routes, and `pour`, `ui` and `cli`. |
| `state.rs` | Live cluster state via the `pgd` CLI's JSON output. |
| `monitor.rs` | PGD Monitor probes (the web UI added in PGD 6.5). |
| `term.rs` | Colour, glyphs, banner. |

Adding a verb means an arm and a function, nothing else. If every product
would do it with the *same code*, the arm goes on `SharedVerb` in `main.rs`
and the function in `lifecycle.rs`. Otherwise it goes on the product's own enum
(`PgdVerb`, `LogicalVerb`, `EfmVerb`) and the function in its module. `up` and `status`
exist for every product but are declared per product, because each implements
them differently.

### Why commands are grouped by product

`cider pgd up` and `cider logical up`, rather than one `cider up`. The grouping
predates the second product: it separates host-level setup (`doctor`,
`bootstrap`, which touch your Mac's DNS configuration) from cluster work, and
it left room for `cider logical` without renaming a single existing command.

`cider logical` and `cider efm` are built; *Failover Manager on this runtime*
below records what EFM needed. One more candidate has been considered, not
built: **Patroni**, the popular open-source manager for streaming replication,
with etcd for consensus. Like EFM, its members address each other by IP, so it
would rely on the stable addresses EFM does.

### Adding a second product

This is how `cider logical` was added, kept as the checklist for a third. The
shared code was refactored out of PGD first, in four steps, and only then was
the new product written; nothing of PGD's was copied. The table records what
carried over when this began, measured against the logical-replication pair:

| Piece | Reusable? |
|---|---|
| `container.rs`, `bootstrap.rs`, `term.rs` | As-is. PGD appears only in comments, one "Next:" hint and the banner tagline. |
| `doctor.rs` | Mostly. The token and image checks assume PGD's image. |
| `config.rs` | Yes: step 2 below, done. `Config` holds the domain, sizing, password and OS pin; `PgdConfig` (`cfg.pgd`) holds group, pool mode, monitor, Connection Manager ports and the token. |
| `lifecycle.rs` | Yes: step 1 below, done. Start-and-wait with the retry, stop, start, down, `pomace`, the container table, the build skeleton and the base `container run` flags. |
| `pgd.rs`: `build`, `up`, `start_node` | PGD's remaining half: the token secret, the Connection Manager and monitor ports, the `PGD_*` env and the post-join steps. |
| `pgd.rs`: `node_joined`, `endpoints`, `ui`, pool mode, `pg_stat_statements`, the Connection Manager wait | PGD only. |
| `state.rs`, `monitor.rs` | PGD only, and should stay that way. |
| Verbs | Yes: step 3 below, done. `SharedVerb` (containers, stop, start, down, `pomace`, shell, logs) is flattened into each product's own enum. |
| `image/entrypoint.sh` | Yes: step 4 below, done. The privilege drop, the self-resolve wait, `pg_hba` and `listen_addresses` are in `image/lib/node-common.sh`. |

The alternative, copying the lifecycle into a second module, is the wrong
trade. The stop signal, the retry and the `pomace` confirmation each exist
because of a failure that took a debugging session to find. Two copies would
drift, and the drift would show up as the kind of bug this tool is meant to
avoid. So the order is refactor first, then add the product:

1. **Move the lifecycle out of `cluster.rs`** *(done; that file is now
   `pgd.rs`)* into `lifecycle.rs`,
   which takes a small per-product `Deployment`: container prefix, volume
   prefix, image, node count, Dockerfile, ready timeout, and the
   `cider <group>` name used in messages. How to create a node and how to tell
   it is ready are passed in as closures. There's no trait yet; two
   implementations don't justify one, though a third might. Ports are not in
   `Deployment`: only the product publishes them, so the port base stays the
   product's to choose. PGD's behaviour must not change, and CI can't prove
   that, so the refactor ends with a cold `pomace -y && build && up`.
2. **Split `Config`** *(done)*. `Config` keeps what every product shares;
   PGD's settings are in `cfg.pgd`. Container naming (`host_name`,
   `host_fqdn`, `node_index`) moved onto `Deployment`, because every product
   names and addresses containers the same way. A second product adds its own
   struct beside `PgdConfig`. The environment variable names did not change,
   so existing `.env` files still work.
3. **Split `Verb`** *(done)* into `SharedVerb`, flattened with clap's
   `#[command(flatten)]` into each product's own enum (`PgdVerb`), so
   `cider logical --help` won't offer a web UI. A verb is shared only when one
   function serves every product; `up`, `status`, `endpoints`, `psql` and
   `build` are declared per product so each can say what it does, and so
   dispatch never needs an unreachable arm. The `psql`, `shell` and
   `require_running` helpers take a `Deployment`, so a second product calls
   them as they are. `cluster.rs` was renamed `pgd.rs` in its own commit.
4. **Move the shared entrypoint helpers into one file that both images
   source** *(done)*: `image/lib/node-common.sh`, installed at
   `/usr/local/lib/cider-press/`. It has logging, the privilege drop, the
   self-resolve wait, `pg_hba` and `listen_addresses`, so a fix to any of them
   lands once. The `pg_stat_statements` preload stayed in PGD's entrypoint: it
   is generic Postgres, but it is also the most delicate code in the file, and
   nothing needed it moved. The build context is already `image/`, so a second
   Dockerfile can sit beside the first.

After that came the product itself: `image/logical.Dockerfile`,
`image/logical-entrypoint.sh`, `src/logical.rs`, `LogicalConfig` and
`LogicalVerb`. Nothing else changed, apart from two things the second product
exposed. The Debian pin had to become per-product. And `containers_table`
had to match exact names instead of prefixes, because both products' volumes
start `cider-press-`.

Two products must not share these, or they can't run at the same time: the
container names, the volume names, the image tag and the loopback ports. PGD
uses `host-1..3`, image `cider-press:latest`, and ports 5432-5434 and 6432-6457.
`cider logical` uses `dolores-1..2`, image `cider-press-logical:latest`, and
ports 5442-5443. Volume names include the container name, so a shared volume
prefix is fine.

Of the eight things below, the logical pair still hits **#1**. A subscription's
`CONNECTION` string is stored in the catalog and resolved again on every
reconnect, so it needs the fully-qualified name for the same reason PGD's
`--listen-addr` does. It also hits **#2**, because a subscription that connects
over IPv6 needs the `::/0` line. It avoids **#3** and, at boot, **#5**:
`listen_addresses` is `'*'` from the very first start, so nothing needs to
resolve the node's own name before Postgres starts. But a *peer's* subscription
still can't reach a node until its name is registered, which is why
`logical.rs` counts a node as ready only once its name resolves.

## Why Debian, and why the versions are pinned

EDB publishes PGD for both Debian 12 (arm64) and RHEL 9 (aarch64), so the base
image was a free choice. Measured on this runtime:

| base | rootfs | packages |
|---|---|---|
| `debian:12-slim` | 102 MB | 88 |
| `ubi9/ubi-minimal` | 110 MB | 109 |
| the built PGD image | 290 MB | — |

Debian is lighter, but only by 8 MB — under 3% of the finished image, because
PGD and Postgres account for roughly 190 MB whichever base carries them. Size is
therefore *not* a good reason to switch. The reasons to stay are that this one
is built and verified end to end, and that moving to UBI would mean rewriting
the Dockerfile for `microdnf` and different package names (RHEL splits
`edb-postgresextended<N>-server` and `-contrib`, where Debian has one package).

The **major version is pinned deliberately**, both here and for the CI runners.
EDB is conservative about certifying new operating systems, so a newer Debian
usually has no PGD packages for months after release. Floating to `latest` would
eventually break a build at `apt-get install` on a day nothing changed. Bump it
when you decide to, after checking EDB's compatibility matrix — and check arm64
specifically, which lags x86_64.

**`cider logical`'s image is on Debian 13** (`CIDER_LOGICAL_DEBIAN_VERSION`),
pinned separately. PGDG published PostgreSQL 16-18 for Debian 13 arm64 within
weeks of its release, so there was no reason to hold it back. It also makes the
logical image the early test of `lib/node-common.sh` on Debian 13, so when PGD
moves, the library will already be proven there. The cost, until PGD moves too,
is that a change to the library has to be checked on both Debian 12 and 13.

## Eight things that will bite you

These each cost a debugging session. They are not in any documentation, and
every one of them produced a cluster that looked fine and wasn't.

**1. Apple `container` only resolves `<name>.<domain>`.** Bare hostnames are
not supported ([apple/container#1809](https://github.com/apple/container/issues/1809)).
This matters more than it sounds: `pgd node setup --listen-addr` writes that
address into the cluster catalog as the address peers dial *permanently*. A
name that resolves only sometimes gives you a cluster that works today and
half-fails after a restart. Hence fully-qualified names everywhere, and hence
the `default` network — custom networks get isolation but no name resolution.

**2. `pg_hba.conf` needs `::/0`.** Every container gets an IPv4 *and* an IPv6
address, and container DNS answers AAAA. The file `pgd node setup` generates
covers `0.0.0.0/0` only, so joins arriving over IPv6 are rejected with
`no pg_hba.conf entry for host "fd68:..."`. The entrypoint supplies its own via
`--hba-conf`.

**3. `listen_addresses` resolves once, at startup.** Container registers a
node's A and AAAA records moments apart, so a node that starts early binds IPv4
only while its peers bind both. Peers then dial it over IPv6, get nothing, and
it sits at `Unreachable` with Raft failing. The entrypoint sets
`listen_addresses = '*'`.

**4. `ALTER SYSTEM SET x = 'a, b'` stores ONE library name.** `shared_preload_libraries`
is a list GUC; a single quoted string is a single element. Getting this wrong
produces `FATAL: could not access file "$libdir/bdr,pg_stat_statements"` and a
node that will not boot. Each element must be its own SQL value:
`SET x = 'a', 'b'`. The entrypoint does this, then **verifies by restarting**
and rolls back if the server does not come up.

**5. Container DNS registration is asynchronous, and often slow.** A
container's `<name>.<domain>` record can appear in well under a second, but
after a container starts — created *or* restarted — it frequently takes 60–80
seconds, and has been seen on several consecutive starts. During that window
the name does not resolve at all: peers log `could not translate host name`, and
a node that cannot resolve *its own* name cannot start Postgres, because that
name is in `listen_addresses`. This is what "Unreachable for a minute after a
restart" and "one node slow during `up`" both are.

Three mitigations, because this produced the worst failure this tool has had —
`up` died pointing at `cider doctor`, and `doctor` then correctly reported DNS
as healthy, sending you to look in the wrong place: the entrypoint waits
generously (`PGD_SELF_RESOLVE_TIMEOUT`, 180s) and distinguishes "not
configured" from "did not register in time" by checking whether the domain is
in `resolv.conf`; `up` restarts a failed node once (`NODE_ATTEMPTS`), which is
what a human would do anyway; and `up` and `status` detect the missing name
(`container::resolves_inside`) and say so, rather than print dots or an
unexplained `Unreachable`.

**The fix is to not depend on it between nodes.** A restarted container gets a
new random MAC, so a new IPv6 address, and the next IPv4 in sequence. Pinning
the MAC (`--network default,mac=…`, derived from the container's name in
`lifecycle::node_mac`) makes the IPv6 address the same on every start: the
network's /64 prefix plus the MAC's EUI-64 interface ID. Pinning alone did not
help (restarts still took 6–85s), because the delay is the name being *absent*,
not stale. So `cider` also passes every node the name and interface ID of every
node (`CIDER_PEERS`). At each start, as root, `write_peer_hosts` in
`lib/node-common.sh` adds its own current prefix and writes the lot into
`/etc/hosts`; name lookup checks that before DNS. Every start, because the
runtime rewrites `/etc/hosts` on each one. The prefix is read inside the
container rather than passed in, so a new prefix from the runtime is picked up
on the next start.

Measured on a scratch PGD cluster: whole-cluster restarts went from 77, 17, 6,
85 and 78 seconds to 0, 11 and 6, with no `Unreachable` at all. Recreating
containers with `down` and `up` went from 65–70 seconds per node to about 3.
Not one DNS lookup failure was logged. The runtime's DNS still serves macOS,
and is the fallback for a container with no global IPv6 address, or one created
by an older cider without `CIDER_PEERS`. That fallback is why the
`resolves_inside` notes above remain.

**6. The resolver file is `containerization.<domain>`,** not `<domain>`. Never
look for it by path — ask `container system dns list`.

**7. A connection attempt to a peer that just restarted can hang for two
minutes.** Found only once DNS stopped hiding it. When both logical nodes
restarted together, a subscription's first attempt to reach its peer was often
dropped, and with nothing to stop it libpq waited out the kernel's SYN retries
(`tcp_syn_retries = 6`, about 127 seconds) before trying again. Restarts took
136–137 seconds. `connect_timeout` does not help: the apply worker connects
with libpq's asynchronous API, which leaves that timeout to the caller, and
`PGCONNECT_TIMEOUT` was tried and made no difference. Nor can the container
lower `tcp_syn_retries`, because `/proc/sys` is read-only and `container run`
has no `--sysctl`. What works is `tcp_user_timeout=5000` in the connection
string, a socket option the kernel enforces whatever API is used. With it,
restarts took 7–8 seconds. PGD's own node connection strings already set
`tcp_user_timeout`, which is why PGD never showed this. Any new product whose
nodes connect to each other through libpq should set it too. (The dropped first
attempt is probably #8: a peer that has just started can't be reached for a few
seconds. The timeout is still worth having.)

**8. A new container's IPv6 address can't be used for its first few seconds.**
A new IPv6 address is *tentative* while the kernel runs duplicate address
detection, and a tentative address can't be used at all: connecting to it, even
from the same node, fails with "No route to host". It lasted 2–5 seconds after a
container started. EFM's agent hit it first: it starts a few seconds after the
container, connects to its own database by its IPv6 address, and exited. So
`write_peer_hosts`, which runs first on every start, now waits until the
address's tentative flag (0x40 in `/proc/net/if_inet6`) clears before anything
else starts, and logs how long it waited.

## Failover Manager on this runtime

What `cider efm` needed, beyond the stable addresses every product gets. Each
came from a failure, and most would apply to running EFM in any container.

- **Postgres can't be the container's main process.** Failover Manager stops,
  starts and promotes the database itself; fencing a failed primary, say. If the
  container's life were tied to Postgres, that would kill it. The node entrypoint
  stays as PID 1 instead, as root. It runs Postgres as `postgres` and the agent as
  `efm`, and on the stop signal it stops the agent first, then Postgres.
- **The package doesn't pull in everything it needs.** It needs Java 11 or later,
  per its docs, and `sudo`, which its own sudoers file assumes. The Dockerfile
  installs both.
- **`bind.address` and the `.nodes` file take an address, not a name,** as
  `[ipv6]:port`. Each start writes them from `/etc/hosts`, so they're always this
  start's addresses.
- **The properties file must be readable by `postgres`,** not just `efm`. The
  agent runs its database checks as `sudo -u postgres efm_db_functions …`, which
  read it. The file holds no secret: the password comes from
  `script.db.password` (new in 5.4), reading an `efm`-only file.
- **`ping.server.ip` defaults to 8.8.8.8, which doesn't answer from inside these
  containers, and an agent that can't reach it at startup exits.** Neither does
  the network's gateway. It must not be a cluster node, so it's set to the load
  balancer: on the same network, where clients come from, and started first.
  That's why extras start before nodes.
- **Tools EFM runs as `postgres` need a `.pgpass`.** It runs `pg_rewind` and
  `pg_basebackup` through `sudo -u postgres`, which strips `PGPASSWORD` from the
  environment. Without the file, rebuilding a failed primary failed with "no
  password supplied".
- **`wal_keep_size = 512MB`.** After a failover the remaining standby is
  repointed at the new primary. With the default of 0 and no replication slot,
  the new primary had already removed WAL the standby needed, and it retried
  "requested WAL segment has already been removed" for ever.
- **One synchronous standby, the same line on every node:**
  `synchronous_standby_names = 'FIRST 1 ("maeve-1", "maeve-2", "maeve-3")'`. The
  primary's first connected standby in that list is `sync`, and the other is
  `potential`. A node is never its own standby, so the line is right on every
  node and after every failover. Promotion is left to EFM's default
  `use.replay.tiebreaker` (the standby furthest ahead in replay), which picks
  the synchronous standby or one exactly as current. EFM's priority list isn't
  tied to it: Postgres and EFM keep separate orderings, and they drift once a
  failed primary rejoins. The names **must be double-quoted**. They contain a
  hyphen, and unquoted the value is a syntax error, which makes
  `postgresql.conf` invalid and the node unbootable (bite #4 again). The
  entrypoint checks the line with `postgres -C` and removes it if it doesn't
  parse, so a mistake costs synchronous replication, not the node.
- **A node being provisioned clones whichever node is primary *now*,** found by
  asking the others' health endpoints. Otherwise rebuilding `maeve-1` after a
  failover would create a second primary beside the promoted one.
- **No virtual IP.** An IPv4 VIP moves instantly, because EFM sends a gratuitous
  ARP, but the runtime was seen handing the VIP's address to a new container,
  then wrapping round to `.2`. An IPv6 VIP is safe from that, but EFM only
  announces IPv4 VIPs, so peers and macOS kept using the old node for 40
  seconds. The load balancer polling the 5.4 health endpoint has neither
  problem.

Measured with the defaults: `up` in about 40 seconds; a hard kill of the primary
detected in about 60 (Failover Manager's default timing); promotion and the load
balancer following within a few seconds after that; the old primary rebuilt and
rejoined automatically; a whole-cluster `stop` and `start` healthy in 7 seconds
with no false failover.

## Deliberate decisions

- **`state.rs` uses the `pgd` CLI, not SQL against PGD's catalogs.** The CLI is
  the supported interface. Querying internal catalogs would couple this tool to
  PGD's schema and to knowledge it has no business encoding.
- **`pgd`'s JSON key spellings are not a published contract**, so rows are
  matched leniently (case/underscore/space-insensitive, with fallbacks) and
  every path degrades to the container view rather than failing.
- **The subscription token is a BuildKit secret**, never a `--build-arg`. The
  apt repo files that embed it are deleted in the same layer that creates them,
  and the build audits itself afterwards.
- **`exec_interactive` ignores the child's exit status.** `psql` exits non-zero
  for ordinary things; treating that as a tool failure would be noise. The
  trade-off is that a genuine failure there is invisible.
- **SQL built with `format!` in `pgd.rs`/`state.rs`** interpolates values
  that come from config (node and group names). Those are operator-controlled
  in a local lab, not user input. If this ever takes untrusted names, that
  changes.

## Testing

`cargo test` covers the pure logic: config parsing, the TOML editor,
`container` output parsing, `pgd` JSON shapes, table layout, the naming and
port schemes, and the command surface (every verb and alias still parses).
It runs anywhere.

Only `PG_FLAVOR=pge` has ever been built. The `epas` and `pg` branches of the
Dockerfile are written, and reference packages that exist for Debian 12 arm64,
but no cluster has been stood up on either. Treat them as plausible, not proven.

`cider logical` on its defaults (PostgreSQL 18, Debian 13) has been built and
run end to end: the printed wiring SQL pasted as-is, replication in both
directions, no echo loop, and a forced `insert_exists` conflict showing in
`status`. Other `CIDER_LOGICAL_PG_MAJOR` values have not been tried.

`cider efm` on its defaults (EFM 5.4, PGE 18, Debian 12) has been built and run
end to end: writes through the load balancer from macOS reaching both standbys,
a failover with a client writing throughout, the old primary rejoining by
itself, and a whole-cluster restart. Other `CIDER_EFM_VERSION` values have not
been tried; anything before 5.4 lacks the health endpoint.

It does **not** cover standing up a cluster — that needs Apple silicon,
macOS 26+, the runtime, and a subscription token. CI green means the logic is
sound, not that a cluster comes up. Verifying that is a manual `cider pgd up`.

When changing `image/entrypoint.sh` or `image/lib/`, remember the blast radius
is a node that will not boot, several minutes into `up`. Prefer changes that
verify themselves and roll back, as the `pg_stat_statements` step does.
`shellcheck -x` in CI catches a lot, but not a call to a function that no
longer exists.

**You can test an entrypoint change without a token, or touching your own
cluster.** Everything else in the image is unaffected by such a change, so
layer the new files over the image you already have:

```dockerfile
FROM cider-press:latest
COPY entrypoint.sh /usr/local/bin/cider-press-entrypoint
RUN chmod 0755 /usr/local/bin/cider-press-entrypoint
COPY lib/node-common.sh /usr/local/lib/cider-press/node-common.sh
```

Build that from a copy of `image/` as `cider-press:test`, then press a scratch
cluster from it with every name and port moved aside:

```bash
CIDER_IMAGE=cider-press:test CIDER_HOST_PREFIX=scratch- \
CIDER_VOLUME_PREFIX=test- CIDER_CLUSTER_NAME=scratch \
CIDER_PG_PORT_BASE=15432 CIDER_CM_PORT_BASE=16432 ./cider pgd up
```

Empty volumes mean this exercises the cold path: seed, joins, configuration.
Clean up by deleting the `scratch-*` containers, the `test-*` volumes and the
test image **by name**. Using `pomace` with the same overrides would work, but
if an override were lost it would destroy your real cluster and image instead.

## Style

`cargo fmt`, and `cargo clippy -- -D warnings` must stay clean; CI enforces
both. Comments explain *why*, not *what* — most of the non-obvious code here is
non-obvious because of a runtime quirk, and that quirk is what belongs in the
comment.
