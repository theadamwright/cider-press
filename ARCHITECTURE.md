# Architecture

Notes for anyone working *on* cider-press. For using it, see [README.md](README.md).

## What this is

A CLI that stands up clusters on Apple's `container` runtime: a multi-node EDB
Postgres Distributed cluster (`cider pgd`), and a pair of community PostgreSQL
nodes for logical replication (`cider logical`). It owns two things and defers
everything else:

1. **`src/`** — a Rust CLI that shells out to `container` and to the `pgd` CLI.
   It holds no cluster logic of its own; it orchestrates.
2. **`image/`** — two Debian images, each with an entrypoint that provisions one
   node: PGD's (`Dockerfile`, `entrypoint.sh`) and logical's
   (`logical.Dockerfile`, `logical-entrypoint.sh`). Both source
   `lib/node-common.sh`, the helpers any product's entrypoint shares.

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

Then come back to *Six things that will bite you* below, which will make a lot
more sense once you've seen the moving parts.

## Module map

| Module | Responsibility |
|---|---|
| `config.rs` | Every tunable, resolved once from env/`.env`: shared settings on `Config`, each product's on `cfg.pgd` and `cfg.logical`. Where every name is formed, and the port maths. No I/O beyond reading env. |
| `container.rs` | **The only** module that runs `container` or parses its output. If the runtime changes its CLI, this is the blast radius. |
| `doctor.rs` | Preflight checks, ordered so the first failure is the root cause. |
| `bootstrap.rs` | One-time host setup: the container DNS domain, the macOS resolver. Edits a file the user owns, so it backs up and verifies. |
| `lifecycle.rs` | What every product shares: build the image, start a node and wait for it (with the retry), stop, start, down, `pomace`. Knows nothing about what runs inside a node; a product describes its nodes with a `Deployment` and passes in how to create one and how to tell it is ready. |
| `pgd.rs` | PGD's half: its container flags and environment, join readiness, pooling, `pg_stat_statements`, status, endpoints, the web UI. |
| `logical.rs` | `cider logical`'s half: its containers, readiness (`wal_level = logical` *and* the name in DNS), a status view of publications, subscriptions and apply errors from public catalogs, and the wiring SQL it prints. Creates no replication itself. |
| `state.rs` | Live cluster state via the `pgd` CLI's JSON output. |
| `monitor.rs` | PGD Monitor probes (the web UI added in PGD 6.5). |
| `term.rs` | Colour, glyphs, banner. |

Adding a verb means an arm and a function, nothing else. If every product
would do it with the *same code*, the arm goes on `SharedVerb` in `main.rs`
and the function in `lifecycle.rs`. Otherwise it goes on the product's own enum
(`PgdVerb`, `LogicalVerb`) and the function in its module. `up` and `status`
exist for every product but are declared per product, because each implements
them differently.

### Why commands are grouped by product

`cider pgd up` and `cider logical up`, rather than one `cider up`. The grouping
predates the second product: it separates host-level setup (`doctor`,
`bootstrap`, which touch your Mac's DNS configuration) from cluster work, and
it left room for `cider logical` without renaming a single existing command.

`cider logical` is built. Two more candidates have been considered, neither
built:

- **EFM** is viable. `edb-efm54` is published for Debian 12 arm64, and a Virtual
  IP works on this runtime (`--cap-add NET_ADMIN`; vmnet routes an address it did
  not assign, verified from both the host and a peer container). EFM's
  `primary.health.check.port` — 200 on the primary, 404 elsewhere — is probably a
  better fit for a lab than a VIP.
- **Patroni** — the popular open-source manager for streaming replication, with
  etcd for consensus.

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

Of the six things below, the logical pair still hits **#1**. A subscription's
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

## Six things that will bite you

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

Ruled out, so nobody chases it again: a restarted container *does* get new
addresses (a new random MAC, so a new IPv6 address, and the next IPv4 in
sequence), and DNS serves the old ones for 6–20 seconds. But pinning the MAC
(`--network default,mac=...`) kept IPv6 identical across restarts without
shortening the window at all. The delay is the name being absent, not stale.
A possible real fix, not yet tried: with pinned MACs the IPv6 addresses are
predictable, so peers could be written into each node's `/etc/hosts` and the
runtime's DNS bypassed between nodes.

**6. The resolver file is `containerization.<domain>`,** not `<domain>`. Never
look for it by path — ask `container system dns list`.

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
