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
5. **`src/cluster.rs`** — the verbs. Start at `up()` and follow it down.

Then come back to *Five things that will bite you* below, which will make a lot
more sense once you've seen the moving parts.

## Module map

| Module | Responsibility |
|---|---|
| `config.rs` | Every tunable, resolved once from env/`.env`. Naming and port maths. No I/O beyond reading env. |
| `container.rs` | **The only** module that runs `container` or parses its output. If the runtime changes its CLI, this is the blast radius. |
| `doctor.rs` | Preflight checks, ordered so the first failure is the root cause. |
| `bootstrap.rs` | One-time host setup: the container DNS domain, the macOS resolver. Edits a file the user owns, so it backs up and verifies. |
| `cluster.rs` | The verbs: build, up, status, endpoints, ui, lifecycle, teardown. |
| `state.rs` | Live cluster state via the `pgd` CLI's JSON output. |
| `monitor.rs` | PGD Monitor probes (the web UI added in PGD 6.5). |
| `term.rs` | Colour, glyphs, banner. |

Adding a verb means: an arm on `Verb` in `main.rs`, and a function in
`cluster.rs`. Nothing else.

### Why commands are grouped under `pgd`

PGD is the only product here, so `cider pgd up` looks redundant next to
`cider up`. It is kept on purpose, for two reasons: it separates host-level
setup (`doctor`, `bootstrap`, which touch your Mac's DNS configuration) from
cluster work, and it leaves room to add a second product without a breaking
rename of every command.

Two candidates have been considered, neither built:

- **EFM** is viable. `edb-efm54` is published for Debian 12 arm64, and a Virtual
  IP works on this runtime (`--cap-add NET_ADMIN`; vmnet routes an address it did
  not assign, verified from both the host and a peer container). EFM's
  `primary.health.check.port` — 200 on the primary, 404 elsewhere — is probably a
  better fit for a lab than a VIP.
- **Patroni** popular open source project for managing streaming replication, 

Either would mean a new `Top::<Product>(Verb)` arm reusing the existing verbs,
plus its own image and entrypoint. The verbs are deliberately generic for that
reason; `ui` is the only PGD-specific one, and a second product would reject it.

Note that a second product needs its own **port base**. PGD occupies 5432-5434
and 6432-6457 on loopback today, and two stacks running at once would collide.

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
- **SQL built with `format!` in `cluster.rs`/`state.rs`** interpolates values
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
