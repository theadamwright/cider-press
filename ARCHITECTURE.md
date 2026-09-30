# Architecture

Notes for anyone working *on* cider-press. For using it, see [README.md](README.md).

## What this is

A CLI that stands up a multi-node EDB Postgres Distributed cluster on Apple's
`container` runtime. It owns two things and defers everything else:

1. **`src/`** — a Rust CLI that shells out to `container` and to the `pgd` CLI.
   It holds no cluster logic of its own; it orchestrates.
2. **`image/`** — a Debian image with PGD installed, plus an entrypoint that
   provisions or joins one node.

Everything about *how PGD works* lives in `image/entrypoint.sh`. Everything
about *how the Mac and the runtime work* lives in `src/`. Keeping that line
clean is what makes each half readable on its own.

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
   join, then configure. This is where the PGD knowledge lives.
5. **`src/pgd.rs`** — PGD's verbs. Start at `up()` and follow it down; the
   shared parts it calls into (start-and-wait, retry, stop) are in
   `src/lifecycle.rs`.

Then come back to *Six things that will bite you* below, which will make a lot
more sense once you've seen the moving parts.

## Module map

| Module | Responsibility |
|---|---|
| `config.rs` | Every tunable, resolved once from env/`.env`: shared settings on `Config`, PGD's on `cfg.pgd`. Where every name is formed, and PGD's port maths. No I/O beyond reading env. |
| `container.rs` | **The only** module that runs `container` or parses its output. If the runtime changes its CLI, this is the blast radius. |
| `doctor.rs` | Preflight checks, ordered so the first failure is the root cause. |
| `bootstrap.rs` | One-time host setup: the container DNS domain, the macOS resolver. Edits a file the user owns, so it backs up and verifies. |
| `lifecycle.rs` | What every product shares: build the image, start a node and wait for it (with the retry), stop, start, down, `pomace`. Knows nothing about what runs inside a node; a product describes its nodes with a `Deployment` and passes in how to create one and how to tell it is ready. |
| `pgd.rs` | PGD's half: its container flags and environment, join readiness, pooling, `pg_stat_statements`, status, endpoints, the web UI. |
| `state.rs` | Live cluster state via the `pgd` CLI's JSON output. |
| `monitor.rs` | PGD Monitor probes (the web UI added in PGD 6.5). |
| `term.rs` | Colour, glyphs, banner. |

Adding a verb means: an arm on `Verb` in `main.rs`, and a function in
`lifecycle.rs` if every product would do the same thing, or in `pgd.rs` if
it is PGD's own. Nothing else.

### Why commands are grouped under `pgd`

PGD is the only product here, so `cider pgd up` looks redundant next to
`cider up`. It is kept on purpose, for two reasons: it separates host-level
setup (`doctor`, `bootstrap`, which touch your Mac's DNS configuration) from
cluster work, and it leaves room to add a second product without a breaking
rename of every command.

Three candidates have been considered, none built:

- **Core PostgreSQL logical replication** — two PGDG nodes with
  `wal_level = logical`, left for the user to wire up with a publication and a
  subscription in each direction. The simplest of the three, and the only one
  that needs no subscription token. It is the worked example in the next
  section.
- **EFM** is viable. `edb-efm54` is published for Debian 12 arm64, and a Virtual
  IP works on this runtime (`--cap-add NET_ADMIN`; vmnet routes an address it did
  not assign, verified from both the host and a peer container). EFM's
  `primary.health.check.port` — 200 on the primary, 404 elsewhere — is probably a
  better fit for a lab than a VIP.
- **Patroni** — the popular open-source manager for streaming replication, with
  etcd for consensus.

### Adding a second product

The command grammar is ready for a second product. The code is about half
ready. Here is what carries over, measured against the logical-replication
pair:

| Piece | Reusable? |
|---|---|
| `container.rs`, `bootstrap.rs`, `term.rs` | As-is. PGD appears only in comments, one "Next:" hint and the banner tagline. |
| `doctor.rs` | Mostly. The token and image checks assume PGD's image. |
| `config.rs` | Yes: step 2 below, done. `Config` holds the domain, sizing, password and OS pin; `PgdConfig` (`cfg.pgd`) holds group, pool mode, monitor, Connection Manager ports and the token. |
| `lifecycle.rs` | Yes: step 1 below, done. Start-and-wait with the retry, stop, start, down, `pomace`, the container table, the build skeleton and the base `container run` flags. |
| `pgd.rs`: `build`, `up`, `start_node` | PGD's remaining half: the token secret, the Connection Manager and monitor ports, the `PGD_*` env and the post-join steps. |
| `pgd.rs`: `node_joined`, `endpoints`, `ui`, pool mode, `pg_stat_statements`, the Connection Manager wait | PGD only. |
| `state.rs`, `monitor.rs` | PGD only, and should stay that way. |
| `Verb` | Not quite. `ui`, `pour` and `cli` belong to PGD; the rest are generic. |
| `image/entrypoint.sh` | About a third: the privilege drop, the self-resolve wait, `pg_hba` and `listen_addresses`. |

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
3. **Split `Verb`** into shared verbs plus per-product ones, using clap's
   `#[command(flatten)]`, so `cider logical --help` doesn't offer a web UI.
4. **Move the shared entrypoint helpers into one file that both images
   source**, so a fix to the `pg_hba` or listen logic lands once. The build
   context is already `image/`, so a second Dockerfile can sit beside the
   first.

After that comes the product itself: its own Dockerfile, entrypoint and module.

A second product must not share these with PGD, or the two can't run at the
same time: the container prefix (`host-`), the volume prefix, the image tag and
the loopback ports. PGD occupies 5432-5434 and 6432-6457 today. The
logical-replication pair is decided as `cider logical`, with containers named
`dolores-1` and `dolores-2`.

Of the six things below, a logical-replication pair still hits **#1**. A
subscription's `CONNECTION` string is stored in the catalog and resolved again
on every reconnect, so it needs the fully-qualified name for the same reason
PGD's `--listen-addr` does. It also hits **#2**, because a subscription that
connects over IPv6 needs the `::/0` line. It mostly avoids **#3** and **#5**:
if `listen_addresses` is `'*'` from the first start, nothing needs to resolve
the node's own name at boot.

## Why Debian 12, and why the version is pinned

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

**5. Container DNS registration is asynchronous, and occasionally slow.** A
container's `<name>.<domain>` record normally appears in well under a second,
but it has been observed not to land for over a minute. A node that cannot
resolve *its own* name cannot start Postgres, because that name is in
`listen_addresses`. Two mitigations, because this produced the worst failure
this tool has had — `up` died pointing at `cider doctor`, and `doctor` then
correctly reported DNS as healthy, sending you to look in the wrong place:
the entrypoint waits generously (`PGD_SELF_RESOLVE_TIMEOUT`, 180s) and
distinguishes "not configured" from "did not register in time" by checking
whether the domain is in `resolv.conf`; and `up` restarts a failed node once
(`NODE_ATTEMPTS`), which is what a human would do anyway.

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
`container` output parsing, `pgd` JSON shapes, table layout. It runs anywhere.

Only `PG_FLAVOR=pge` has ever been built. The `epas` and `pg` branches of the
Dockerfile are written, and reference packages that exist for Debian 12 arm64,
but no cluster has been stood up on either. Treat them as plausible, not proven.

It does **not** cover standing up a cluster — that needs Apple silicon,
macOS 26+, the runtime, and a subscription token. CI green means the logic is
sound, not that a cluster comes up. Verifying that is a manual `cider pgd up`.

When changing `image/entrypoint.sh`, remember the blast radius is a node that
will not boot, several minutes into `up`. Prefer changes that verify themselves
and roll back, as the `pg_stat_statements` step does.

## Style

`cargo fmt`, and `cargo clippy -- -D warnings` must stay clean; CI enforces
both. Comments explain *why*, not *what* — most of the non-obvious code here is
non-obvious because of a runtime quirk, and that quirk is what belongs in the
comment.
