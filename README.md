```
                       \ | /
                     .--'--.              .-------.
                    /        \            |~~~~~~~|
                   |    ()    |           |~~~~~~~|
                    \        /            |~~~~~~~|
                     '-.__.-'             '._____.'

                  p g d - c i d e r
        three nodes of EDB Postgres Distributed,
          pressed on Apple's container runtime
```

A from-scratch local lab for **EDB Postgres Distributed 6.5.0+** on Apple silicon,
built on [`apple/container`](https://github.com/apple/container) instead of Docker
Desktop. Build the image, press a cluster, tear it all down again — one small
script, no daemon you have to remember to quit.

A spiritual port of the [PGD Docker quickstart](https://www.enterprisedb.com/docs/pgd/latest/quickstart/first-cluster/),
rebuilt for a runtime with no `compose`.

> [!IMPORTANT]
> cider-press is **not** endorsed or supported by EnterpriseDB, and is not covered 
> by any EDB support agreement or SLA. Please don't raise EDB support tickets about it 
> , open an issue here instead. PGD itself is commercial software: you need your own valid
> EDB subscription, and your use of the packages it installs is governed by your
> agreement with EDB, not by this repository's licence.

**Contents** · [What this is for](#what-this-is-for) ·
[Why Apple container](#why-apple-container) · [Expanding](#could-this-do-more-than-pgd) ·
[Requirements](#requirements) · [Build](#1-build) · [Up](#2-up) ·
[Write leader](#3-connect-to-the-write-leader) ·
[Read-only](#4-read-only-load-balanced-across-nodes) ·
[Web UI](#5-connect-to-the-web-ui) · [Tear down](#6-tear-down) ·
[Pooling](#connection-pooling) · [Commands](#command-reference) ·
[Status](#how-cider-pgd-status-reads-the-cluster) · [DNS](#the-dns-part-the-only-genuinely-tricky-bit) ·
[Configuration](#configuration) · [Token safety](#about-that-subscription-token) ·
[Troubleshooting](#troubleshooting)

---

## What this is for

Getting a working PGD cluster in front of you in a few minutes, with the least
friction possible: for **evaluation, testing, demos and learning**. Build the
image once, press a cluster, break it, throw it away, press another. The whole
point is that a cluster is cheap enough to treat as disposable.

**It is not for production, and isn't built to be.** The password is `secret`
and it's written in this repository. Ports bind to loopback only. There is no
TLS between nodes. Every "node" is a container on one Mac, so the cluster can
demonstrate write-leader routing and failover convincingly while surviving
none of the things real high availability exists for — starting with that Mac
going away.

For anything real, use EDB's supported paths:
[PGD CLI](https://www.enterprisedb.com/docs/pgd/latest/deploying/deploy-manual/),
[PGD for Kubernetes](https://www.enterprisedb.com/docs/pgd/latest/deploying/deploy-kubernetes/),
or [Hybrid Manager](https://www.enterprisedb.com/docs/pgd/latest/deploying/deploy-hm/).

## Could this do more than PGD?

Structurally, yes. Commands are grouped as `cider <product> <verb>`, and the
verbs are written to be product-agnostic — so a second stack would be a new
group reusing the same grammar (`cider efm up`, `cider patroni status`) rather
than a rewrite.

Two candidates, neither built and neither promised:

- **[EDB Failover Manager](https://www.enterprisedb.com/docs/efm/latest/)** —
  streaming replication with automatic failover, the classic counterpart to
  PGD's active-active. Evaluated as far as feasibility: `edb-efm54` is published
  for Debian 12 arm64, and EFM's Virtual IP works on this runtime (verified — an
  address added under `--cap-add NET_ADMIN` is reachable from both macOS and a
  peer container, because vmnet routes addresses it did not assign). EFM also
  exposes an HTTP endpoint that answers 200 on the primary and 404 elsewhere,
  which suits a lab better than a VIP.
- **[Patroni](https://patroni.readthedocs.io/)** — the same failover problem
  solved in the open-source world, with etcd for consensus.

Whether any of that happens depends on whether the PGD quickstart proves worth
using first. If you would find one of them useful, say so in an issue. 

## Why Apple container

[`apple/container`](https://github.com/apple/container) is Apple's own container
runtime for Apple silicon, and for a throwaway lab it earns its place over
Docker Desktop:

- **Nothing to install but a signed package, and no licence to think about.**
  Docker Desktop needs a paid subscription for commercial use in larger
  organizations; this doesn't.
- **No daemon sitting in your menu bar.** There's no always-on Linux VM holding
  memory while you're not using it. Containers start on demand and are gone when
  stopped — you pay for three PGD nodes only while three PGD nodes are running.
- **Each container is a lightweight VM** on macOS's own virtualization
  framework, with real isolation rather than shared-kernel namespaces.
- **Every node gets a real IP on your Mac's network.** That's not cosmetic, 
  it's why `psql -h host-1.cider` works directly from macOS, and why the nodes
  can address each other the way PGD expects. See
  [the DNS section](#the-dns-part-the-only-genuinely-tricky-bit).

The trade-off is that `container` is young and has no `compose`, which is most
of the reason this tool exists.


---

## Requirements

| | |
|---|---|
| Hardware | Apple silicon (M1 or later) |
| macOS | **26 or later** — on macOS 15 `container` cannot do container-to-container networking at all, which is the whole game |
| Runtime | [`container`](https://github.com/apple/container/releases/latest) 1.3.0+, from the signed `.pkg` |
| Rust | To build `cider` itself — [rustup](https://rustup.rs), edition 2024 (1.85+) |
| Credentials | An EDB subscription token, for the build only — [get one here](https://www.enterprisedb.com/repos-downloads) |
| RAM | ~6 GB free for three nodes at the 2 GB default |

`cider` is written in Rust, matching PGD 6's own `pgd` CLI. You don't have to
think about that: `./cider` is a launcher that compiles the binary on first use
and then hands over to it. Once built, `./target/release/cider` is a standalone
binary you can put on your `PATH`.

Install the runtime, then check everything at once:

```bash
./cider doctor
```

`doctor` verifies silicon, macOS version, `container`, the DNS domain, your
token and the image, and tells you which step to run next.

---

## The steps you'll actually run

### 0. One-time host setup

```bash
./cider bootstrap
```

Sets the container DNS domain and creates the macOS resolver entry. **The only
setup command that needs your password**, and you only run it once per machine.
See [why DNS matters here](#the-dns-part-the-only-genuinely-tricky-bit).

`./cider pgd pomace --dns` is the reverse of this step, if you ever want your Mac
back exactly as it was.

### 1. Build

```bash
export EDB_SUBSCRIPTION_TOKEN="your-token-here"
./cider pgd build
```

Builds `cider-press:latest` from `image/Dockerfile`: Debian 12 → EDB repos →
EDB Postgres Extended 18 + PGD 6.5. Takes a few minutes on first run.

The token is passed as a BuildKit secret and never stored in the image — see
[About that subscription token](#about-that-subscription-token). Put it in a
gitignored `.env` if you'd rather not export it each time:

```bash
cp .env.example .env    # then edit
```

### 2. Up

```bash
./cider pgd up
```

Creates a named volume per node, starts `host-1` and waits for it to seed the
cluster, then joins `host-2` and `host-3` **in sequence** — PGD joins are not
safe to run concurrently against a fresh cluster.

Along the way it enables the [web UI](#5-connect-to-the-web-ui), sets
[session pooling](#connection-pooling) and preloads `pg_stat_statements`, then
prints the cluster and every published port:

```
  ✔ connection pooling: session
  ✔ pg_stat_statements ready

 cider · PGD 6 · 3 nodes

  NODE      GROUP       JOIN STATE  KIND      STATUS
  node-1    group-1     ACTIVE      data      Up
  node-2    group-1     ACTIVE      data      Up
  node-3    group-1     ACTIVE      data      Up

  raft leader: node-1 (term 1)  ·  pooling: session  ·  monitor: ready
```

All three nodes at `ACTIVE / Up` with a named `raft leader` is what a healthy
cluster looks like. If a join fails, `up` stops there and prints that node's
logs rather than reporting success.

Re-running `cider pgd up` on an existing cluster is safe — it starts whatever is
stopped and leaves running nodes alone.

### 3. Connect to the write leader

PGD is active-active, but when going through the Connection Manager, writes are routed to one node at a time for string consistency requirements. Rather than tracking which one, connect through **Connection Manager**, which routes port
6432 to whichever node currently holds write leadership.

The short way:

```bash
./cider pgd pour
```

Or from your own tools, straight at loopback:

```bash
PGPASSWORD=secret psql -h 127.0.0.1 -p 6432 -U postgres pgddb
```

Confirm you landed on the leader:

```sql
select node_name from bdr.local_node_summary;
```

```
 node_name
-----------
 node-1
```

Now stop that node and run the same query again — Connection Manager will have
moved you to the new leader:

```bash
container stop host-1
PGPASSWORD=secret psql -h 127.0.0.1 -p 6442 -U postgres pgddb -c \
  'select node_name from bdr.local_node_summary'
```

Every node runs its own Connection Manager, so 6432 / 6442 / 6452 are all valid
front doors — use a different one when the node you were using is the one that
went away.

| | port | |
|---|---|---|
| read-write | 6432 | routed to the write leader |
| read-only | 6433 | routed across read nodes |
| health API | 6434 | JSON/health endpoints, not a UI |
| direct | 5432 | bypasses routing, hits `host-1` specifically |

Use `./cider pgd psql 2` to bypass routing and land on a named node deliberately.

### The PGD CLI, without installing it

`cider pgd pour` gets you a *psql* session on the write leader. `cider pgd cli`
is the same idea for the **PGD CLI**:

```bash
./cider pgd cli nodes list
./cider pgd cli cluster show
./cider pgd cli group group-1 set-option server_pool_mode session
```

Nothing is installed on your Mac and you don't open a shell in a container. The
`pgd` binary already exists in the node image, so this runs it in place and
streams the output back — including `-o json`, which pipes cleanly:

```bash
./cider pgd cli nodes list -o json | jq '.[0]'
```

Like `pour`, it points at Connection Manager's read-write port rather than a
particular node, so **the command follows the write leader**. Stop the leader
and the next invocation reaches the newly elected one without you changing
anything:

```console
$ container stop host-1                    # host-1 was the write leader
$ ./cider pgd cli nodes list
 Node Name | Group Name | Node Kind | Join State | Node Status
-----------+------------+-----------+------------+-------------
 node-1    | group-1    | data      | ACTIVE     | Unreachable
 node-2    | group-1    | data      | ACTIVE     | Up
 node-3    | group-1    | data      | ACTIVE     | Up
```

Both commands enter through whichever node is up, not always `host-1` — which
matters precisely when `host-1` is the node that went away.

The DSN is passed as `PGD_CLI_DSN`, so it is only a default: supply your own
`--dsn` and it wins. To target one node deliberately, bypass the routing:

```bash
./cider pgd cli --dsn "host=host-2.cider port=5432 dbname=pgddb user=postgres" nodes list
```

### 4. Read-only, load balanced across nodes

For read traffic, connect to the read-only port on **every** node at once and
let libpq spread sessions across them. `cider pgd up` prints this ready to paste:

```bash
PGPASSWORD=secret psql "postgresql://postgres@127.0.0.1:6433,127.0.0.1:6443,127.0.0.1:6453/pgddb?load_balance_hosts=random"
```

Each entry is one node's Connection Manager **read-only** port.
`load_balance_hosts=random` (libpq 16+) makes libpq shuffle that list on every
connection — without it, every session tries the first host first and the other
read nodes sit idle. Connection Manager then routes each connection on to a
current read node, so this survives a node going away.

Watch it spread by opening several sessions:

```bash
for i in 1 2 3 4 5 6; do
  PGPASSWORD=secret psql "postgresql://postgres@127.0.0.1:6433,127.0.0.1:6443,127.0.0.1:6453/pgddb?load_balance_hosts=random" \
    -tAc "select node_name from bdr.local_node_summary"
done
```

If your Mac's `psql` predates libpq 16 the option is ignored, and you get the
first reachable host every time. `./cider pgd psql` uses the psql inside the
container, which is always current.

### 5. Connect to the web UI

PGD 6.5.0 added **PGD Monitor**, a monitoring web app served by every node as a
background worker — cluster overview, connection management, replication, Raft,
commit scopes, activity, query diagnostics and error logs.

```bash
./cider pgd ui
```

This checks the worker is enabled, waits for its `/is-live` probe to answer,
then opens your default browser. If it can't reach the UI it says whether the
monitor isn't running or the published port isn't reaching it, rather than
opening a dead tab.

Or just browse straight to it:

**<http://127.0.0.1:6437/>** — sign in with `postgres` / `secret`.

Each node serves its own copy, but any one shows a **cluster-wide** view, so
node 1 is normally all you need:

| node | web UI |
|---|---|
| `host-1` | <http://127.0.0.1:6437/> |
| `host-2` | <http://127.0.0.1:6447/> |
| `host-3` | <http://127.0.0.1:6457/> |

If you ran `bootstrap`, you can also use the node's own name, which skips the
port forward entirely and uses the container's real address:

**<http://host-1.cider:6437/>**

Three things worth knowing, because PGD does not do them for you:

- **PGD ships this off.** `bdr.monitor_enabled` defaults to `false`; `cider`
  turns it on at provisioning time. Set `CIDER_MONITOR=off` for stock behaviour.
- **Inside the container it is always Postgres port + 1005** (5432 → 6437). The
  tidy 6437/6447/6457 numbering is only how `cider` publishes them to loopback.
- **It's plain HTTP here.** `monitor_use_https` defaults off, which is what
  makes `http://127.0.0.1:6437/` work out of the box. Turning HTTPS on without
  also giving the node a certificate your browser trusts will just get you a
  warning page.

The same server also carries a JSON REST API and a Prometheus scrape endpoint:

```bash
# Log in, keep the session cookie, then call the API
curl -c /tmp/c.txt -X POST http://127.0.0.1:6437/api/v1/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"postgres","password":"secret"}'
curl -b /tmp/c.txt http://127.0.0.1:6437/api/v1/cluster/health

# Prometheus metrics
curl http://127.0.0.1:6437/metrics
```

Sign in with a superuser to see query text on **Activity** and to open **Error
Log** at all; `pg_monitor` membership is enough for everything else. The REST
API is explicitly best-effort and may change between releases.

### 6. Tear down

Three levels, depending on how much you want back.

**Keep the data.** Removes containers, leaves the volumes:

```bash
./cider pgd down
```

`./cider pgd up` then brings the *same* cluster back — same node identities, same
data — rather than rebuilding it. For a quick pause without removing anything,
`./cider pgd stop` and `./cider pgd start`.

**Destroy the cluster.** Containers, volumes and the image:

```bash
./cider pgd pomace
```

*(Pomace is what's left in the press after the juice is gone.)* It lists exactly
what it will delete and makes you type the cluster name to confirm; add `-y` to
skip that in a script. This is irreversible — the volumes hold all node state.

The host DNS setup survives on purpose, so you never need to `bootstrap` twice.

**Full teardown.** Everything above, plus the host DNS setup `bootstrap` created:

```bash
./cider pgd pomace --dns
```

This puts your Mac back the way it was. On top of the cluster it removes the
`[dns] domain` key from `~/.config/container/config.toml`, restarts the
container system, and deletes `/etc/resolver/cider` — the last of which needs
your password. It asks separately before touching any of it, even after you've
confirmed the cluster teardown.

Three things it deliberately will *not* do:

- **It won't remove a domain that isn't ours.** `[dns] domain` is a global
  `container` setting, not this tool's property. If it holds some other value —
  because you use `container` for other things — it's reported and left alone.
- **It won't restore your pre-`bootstrap` backup.** That file may have changed
  for unrelated reasons since, so `--dns` points you at it instead of
  overwriting your config with it.
- **It won't leave `[dns]` half-dismantled.** The table goes only if removing
  `domain` left it empty; sibling keys keep it.

Anything else on your Mac that resolves `*.cider` names stops working after
this. `./cider bootstrap` sets it all up again.

---

## Connection pooling

Connection Manager pools backend connections. PGD's own default is `none` — no
pooling — and `cider` sets **`session`** instead, applied once to the node group
after the cluster forms:

```
✔ connection pooling: session
```

`session` hands a client its own backend for the life of the connection, then
runs `DISCARD ALL` and returns it to the pool. Nothing an application can see
changes, which is why it is the default here.

`transaction` pools far more aggressively — a backend is held only for the
duration of a transaction — but then session `SET`s, `LISTEN`, `WITH HOLD`
cursors, advisory locks and temporary tables no longer survive between
transactions. Choose it deliberately:

```bash
CIDER_POOL_MODE=transaction ./cider pgd up      # or set it in .env
```

`CIDER_POOL_MODE=leave` leaves whatever the cluster already has, and
`none` restores PGD's stock behaviour. The current mode shows in
`cider pgd status` and in `bdr.node_group_summary.server_pool_mode`.

---

## Command reference

Commands are `cider <product> <verb>`, matching the grammar of EDB's own CLIs
(`pgd node setup`, `efm cluster-status`). Host setup is product-agnostic and
stays at the top level, because every product shares one container DNS domain
and one runtime.

**Setup** (host-level)

| | |
|---|---|
| `cider doctor` | Check silicon, macOS, `container`, DNS, token, image |
| `cider bootstrap [-y]` | One-time host setup. The only command needing sudo |

**Cluster** — `cider pgd …`

| | |
|---|---|
| `cider pgd build [--no-cache]` | Build the node image. Needs `EDB_SUBSCRIPTION_TOKEN` |
| `cider pgd up` / `press` | Create volumes, seed node 1, join the rest |
| `cider pgd status` / `ps` | Live cluster state — nodes, join state, Raft, pooling, monitor |
| `cider pgd containers` | This cluster's containers and volumes |
| `cider pgd endpoints` | Every published port, including the web UI |
| `cider pgd stop` / `start` | Pause / resume the containers |
| `cider pgd down` | Remove containers, **keep** volumes |
| `cider pgd pomace [-y]` | Destroy containers, volumes and image. Irreversible |
| `cider pgd pomace --dns` | The above **plus** the host DNS setup — a full teardown |

**Access** — `cider pgd …`

| | |
|---|---|
| `cider pgd ui` / `web [node]` | Open the PGD Monitor web UI |
| `cider pgd pour` | psql to the write leader via Connection Manager |
| `cider pgd psql [node] [args…]` | psql straight to a node (default 1) |
| `cider pgd cli <args…>` | The PGD CLI, aimed at the write leader |
| `cider pgd shell [node]` | bash inside a node |
| `cider pgd logs [node]` | Container logs |

`[node]` takes either `2` or `host-2`.

---

## How `cider pgd status` reads the cluster

```
 cider · PGD 6 · 3 nodes

  NODE      GROUP       JOIN STATE  KIND      STATUS
  node-1    group-1     ACTIVE      data      Up
  node-2    group-1     ACTIVE      data      Up
  node-3    group-1     ACTIVE      data      Up

  raft leader: node-3 (term 1)  ·  pooling: session  ·  monitor: ready
  ↑ via the pgd CLI on host-1
```

The [PGD monitoring docs](https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/)
describe several supported ways to observe a cluster. This tool uses two of them:

- **The `pgd` CLI**, run inside a node container with its public `-o json`
  output ([command reference](https://www.enterprisedb.com/docs/pgd/latest/reference/cli/command_ref/)).
  That is where the node table and Raft line come from.
- **PGD Monitor's `/is-live` and `/is-ready` probes** for the `monitor:` field.

The third route, [monitoring through SQL](https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/sql),
is available to you directly with `cider pgd psql` whenever you want more detail
than this summary.

`status` deliberately goes through the CLI rather than querying catalogs
itself, so it stays on the interface EDB supports and keeps working as the
schema changes underneath. Key spellings in the JSON are matched leniently, and
if a field can't be found the command degrades to the container view rather
than failing.

---

## The DNS part (the only genuinely tricky bit)

PGD nodes do not talk through a proxy. Each node records, in the cluster
catalog, the address every other node should dial it on — that is what
`pgd node setup --listen-addr` writes. So node addressing must be correct *and*
stable, or you get a cluster that works today and half-fails after a restart.

Apple's `container` resolves container names through an embedded DNS service and
registers them as `<name>.<domain>`, the domain coming from `[dns]` in
`~/.config/container/config.toml`. Looking a container up by its **bare**
hostname is explicitly unsupported
([apple/container#1809](https://github.com/apple/container/issues/1809)).

So `cider-press` uses fully-qualified names everywhere — `host-1.cider`,
`host-2.cider`, `host-3.cider` — and those exact strings go into `--listen-addr`,
every DSN, and the `pgd` CLI config. `cider bootstrap` sets the domain up, and
`cider pgd up` refuses to start if it doesn't match.

Two consequences:

- **This cluster runs on the `default` network.** Custom networks from
  `container network create` are isolated but get no name resolution, so nodes
  would have to use raw IPs — which change on restart and would corrupt the
  catalog.
- **`sudo container system dns create cider`** only affects *your Mac's*
  resolver, letting you `psql -h host-1.cider` from the host. The cluster works
  without it; the published `127.0.0.1` ports always work.

`listen_addresses` is also what Connection Manager and PGD Monitor bind to, which
is why publishing their ports works at all.

### Why the nodes listen on every address

Each container gets **both** an IPv4 and an IPv6 address, and `host-N.cider`
resolves to both. Two consequences shaped this setup:

- **`pg_hba.conf` needs `::/0`.** The file `pgd node setup` generates covers
  `0.0.0.0/0` only, so a join arriving over IPv6 is rejected with
  `no pg_hba.conf entry for host "fd68:..."`. The entrypoint supplies its own
  hba — PGD's, plus the two `::/0` lines — via `--hba-conf`.
- **`listen_addresses` is set to `*`.** Postgres resolves the names in
  `listen_addresses` once, at startup. Apple container registers a node's A and
  AAAA records moments apart, so a node that starts early can bind IPv4 only
  while its peers bind both. Its peers then dial it over IPv6, get nothing, and
  the node sits at `Unreachable` with Raft consensus failing. Listening on
  every address removes the race; `--listen-addr` still carries the
  fully-qualified name, so the address peers dial is unchanged.

Neither is something you need to do — both are handled in `image/entrypoint.sh`
— but they explain why a hand-rolled port of the Docker quickstart tends to
half-work here.

---

## What gets created

Three containers on the default network, each with its own named volume:

| container | node | volume | postgres | cm-rw | cm-ro | cm-health | web UI |
|---|---|---|---|---|---|---|---|
| `host-1` | `node-1` | `cider-press-host-1` | 5432 | **6432** | 6433 | 6434 | **6437** |
| `host-2` | `node-2` | `cider-press-host-2` | 5433 | 6442 | 6443 | 6444 | 6447 |
| `host-3` | `node-3` | `cider-press-host-3` | 5434 | 6452 | 6453 | 6454 | 6457 |

All published on `127.0.0.1` only. Node state lives in the volume at
`/var/lib/cider-press` — `PGDATA` and the server log together.

---

## Configuration

Everything is an environment variable, and a gitignored `.env` beside the script
is sourced automatically. Start from `.env.example`.

| Variable | Default | |
|---|---|---|
| `EDB_SUBSCRIPTION_TOKEN` | — | Required for `build`. Never committed |
| `CIDER_NODES` | `3` | Node count. `5` works; each node is a VM |
| `PG_FLAVOR` | `pge` | `pge`, `epas`, or `pg` — only `pge` is verified, see below |
| `PG_MAJOR` | `18` | Postgres major version |
| `CIDER_MONITOR` | `on` | PGD Monitor web UI |
| `CIDER_POOL_MODE` | `session` | Connection Manager pooling: `session`, `transaction`, `none`, `leave` |
| `CIDER_STAT_STATEMENTS` | `on` | Preload `pg_stat_statements` and create the extension |
| `CIDER_DOMAIN` | `cider` | Container DNS domain |
| `CIDER_MEMORY` | `2G` | Per node |
| `CIDER_USER` / `CIDER_PASSWORD` | derived / `secret` | Superuser follows `PG_FLAVOR`. Also the web UI login |

Defaults give you **PGD 6.5 on EDB Postgres Extended 18**, the newest supported
pairing — PGD 6.5+ requires PGE 18.6+, and the two must move together. See the
[compatibility matrix](https://www.enterprisedb.com/docs/pgd/latest/compatibility/)
for other combinations.

> [!NOTE]
> **Only `PG_FLAVOR=pge` has actually been run.** All three flavors are
> implemented — the image installs the right server and PGD packages and picks
> the right binary directory and superuser for each — and every package they
> reference exists for Debian 12 arm64. But EDB Postgres Extended is the only
> one a cluster has ever been stood up on. `epas` and `pg` *should* work; nobody
> has confirmed it.
>
> If you try one, expect to iterate on the image rather than have it work first
> time, and open an issue either way — a confirmed success is as useful as a
> failure. You should not need to set `CIDER_USER`: it defaults to
> `enterprisedb` for `epas` and `postgres` otherwise.

Changing `PG_FLAVOR` or `PG_MAJOR` means a rebuild, and existing
volumes will not be compatible:

```bash
./cider pgd pomace -y && ./cider pgd build && ./cider pgd up
```

---

## About that subscription token

The token is the credential for your EDB repositories, so this repo makes
committing it hard:

- **`.env` is gitignored**, and `.env.example` carries no value.
- **The build takes it as a BuildKit secret** — `--secret id=edb_token,env=…`,
  read from the environment of the `cider` process. Never a `--build-arg`, so it
  never lands in the image config or layer history where `container image inspect`
  would show it.
- **The apt repo files that embed the token are deleted in the same layer that
  creates them.** EDB's `setup.deb.sh` writes the token into
  `/etc/apt/sources.list.d/`, which would otherwise ship inside the image.
- **The build audits itself**, failing if any credential-bearing
  `downloads.enterprisedb.com` URL survives under `/etc`.

The EDB quickstart's own Dockerfile passes the token as a build arg and leaves
the repo files in place; both are fixed here. Anyone cloning this exports their
own token — nothing about the subscription is baked into the repo.

---

## Troubleshooting

**`bootstrap` fails with "Permission denied", or `doctor` says it cannot read
the container config.** Your `~/.config` is probably owned by `root` — a stray
`sudo` can create it that way, and then nothing of yours can read beneath it:

```bash
ls -ld ~/.config          # drwx------  root  staff   ← the problem
sudo chown -R "$(id -un):staff" ~/.config && chmod 700 ~/.config
```

Worth fixing regardless of this tool: `container` reads its own configuration
from `~/.config/container/config.toml`, so while that is unreadable it silently
runs on defaults — and other tools (`git`, for one) quietly lose their config
too. `doctor` names the exact directory to fix.

**`psql -h host-1.cider` fails with "No route to host", but `127.0.0.1` works.**
Your traffic is being intercepted before it reaches the container network —
almost always a VPN or endpoint-security agent (Netskope, Zscaler, Cisco Secure
Client and similar). Use the loopback ports, which never leave the host:

```bash
PGPASSWORD=secret psql -h 127.0.0.1 -p 6432 -U postgres pgddb
```

The error is misleading, so it is worth knowing how to tell this apart from a
real network fault. If the route is present and the address answers ping, but a
TCP connection is refused, nothing is wrong with your network:

```bash
route -n get 192.168.65.3          # interface should be bridgeN, not utunN
ping -c2 192.168.65.3              # replace with your node's IP from `cider pgd containers`
ps aux | grep -iE "netskope|zscaler|globalprotect|cisco"
```

A per-application proxy is the one thing that can produce this exact split — the
route is correct and ICMP passes, but the proxy steers the TCP connection
somewhere with no path to your container subnet and returns `EHOSTUNREACH`. It
can also affect one process and not another, so "it works in my other terminal"
does not rule it out.

Everything in this README works over `127.0.0.1`. Connecting by name is a
convenience, not a requirement — nothing in the cluster depends on it, because
the nodes talk to *each other* inside the container network where your Mac's
policies do not apply.

**`bootstrap` fails with `sudo: container: command not found`.** `sudo` resets
`PATH` to a secure default that excludes Homebrew's `/opt/homebrew/bin`, so a
Homebrew-installed `container` is invisible to it. cider now calls the binary by
absolute path, so this should not recur — but if you hit it on an older build,
run the step by hand:

```bash
sudo "$(command -v container)" system dns create cider
```

This only affects installs from Homebrew. The signed `.pkg` puts `container` in
`/usr/local/bin`, which sudo does search.

**`doctor` says the DNS domain is wrong.** You probably already use `container`
with a different domain (often `test`). Either set `CIDER_DOMAIN=test` in `.env`
and keep yours, or run `cider bootstrap` to switch — it backs your config up first.

**A node hangs at "waiting for host-2".** `cider pgd up` prints the last 40 log lines
on timeout. Usually name resolution: `cider pgd shell 1`, then
`getent hosts host-2.cider`. Nothing back means the DNS domain isn't in effect —
`container system stop && container system start`, then `cider doctor`.

**A join failed and left a mess.** The entrypoint discards a half-initialised
`PGDATA` rather than leaving something that looks provisioned but isn't, so
`cider pgd down && cider pgd up` retries cleanly. For a truly fresh start, `cider pgd pomace`.

**The web UI won't load in my browser.** Run `./cider pgd ui` first — it diagnoses
this and tells you which of the two cases you're in. To check by hand:

```bash
# 1. Is the worker enabled?
./cider pgd psql 1 -c "show bdr.monitor_enabled"

# 2. Did its listener bind? Every bound address is logged at startup.
./cider pgd logs 1 | grep -i "HTTP.*server"

# 3. Does it answer from your Mac?
curl -i http://127.0.0.1:6437/is-live
```

`off` at step 1 means provisioning ran with `CIDER_MONITOR=off`. Enable it live —
the GUC is `PGC_SIGHUP`, so no restart:

```bash
./cider pgd psql 1 -c "ALTER SYSTEM SET bdr.monitor_enabled='on'" -c "select pg_reload_conf()"
```

If step 2 shows it listening but step 3 fails, the published port is the
problem, not PGD — `./cider pgd down && ./cider pgd up` recreates the mapping. The
monitor binds to whatever is in `listen_addresses`, which for these nodes is
`host-N.cider,localhost`, and `--publish` forwards to that same interface.

**The web UI's Query Diagnostics page is empty.** That page needs
`pg_stat_statements`, which `cider` enables by default. Check it took:

```bash
./cider pgd psql 1 -tAc "show shared_preload_libraries"   # want: "$libdir/bdr", pg_stat_statements
./cider pgd psql 1 -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements"
```

If `up` said `pg_stat_statements broke startup; reverting`, the node came back
without it rather than failing to boot — that rollback is deliberate. Set
`CIDER_STAT_STATEMENTS=off` to stop trying.

Adding it by hand is easy to get wrong, because `shared_preload_libraries` is a
*list* GUC. `ALTER SYSTEM SET shared_preload_libraries = 'a, b'` stores the
whole string as **one** library name and the node then refuses to start. Pass
each element as its own value:

```bash
# right — separate values
./cider pgd psql 1 -c "ALTER SYSTEM SET shared_preload_libraries = '\$libdir/bdr', 'pg_stat_statements'"

# wrong — one quoted string, node will not boot
./cider pgd psql 1 -c "ALTER SYSTEM SET shared_preload_libraries = '\$libdir/bdr, pg_stat_statements'"
```

To recover a node that will not start for this reason:

```bash
./cider pgd psql 1 -c "ALTER SYSTEM RESET shared_preload_libraries"
```

**Can't sign in to the web UI.** The role must be a superuser or a member of
`pg_monitor`. Default is `postgres` / `secret` — i.e. `CIDER_USER` and
`CIDER_PASSWORD`.

**The build can't find `edb-postgresextended-18`.** Either your token lacks
access to that repository, or that pairing isn't published for Debian 12 arm64
yet. `PG_MAJOR=17` is the known-good fallback the EDB quickstart ships.

**`container` commands hang or error oddly.** `container system stop && container system start`
fixes most of it; `container system logs` has the detail.

---

## Layout

Working on the tool itself? [ARCHITECTURE.md](ARCHITECTURE.md) covers the
module map, the runtime quirks that shaped the design, and what CI can and
cannot verify.

```
cider-press/
├── cider                 # launcher: cargo build --release, then exec
├── Cargo.toml
├── src/
│   ├── main.rs           # clap command surface
│   ├── config.rs         # env/.env → typed config, naming, port math
│   ├── container.rs      # the only place apple/container output is parsed
│   ├── doctor.rs         # preflight checks
│   ├── bootstrap.rs      # container DNS domain, via toml_edit
│   ├── cluster.rs        # build / up / status / teardown / web UI
│   ├── state.rs          # live cluster state via `pgd -o json`
│   └── monitor.rs        # PGD Monitor probes
├── image/
│   ├── Dockerfile        # Debian 12 + PGE 18 + PGD 6.5, token as a secret
│   └── entrypoint.sh     # per-node provisioning, join, monitor enablement
├── .env.example
├── ARCHITECTURE.md       # notes for working *on* cider
├── LICENSE
└── README.md
```

Run `cargo test` for the unit tests — they cover the config rewriter, the
`container` output parsing, and the `pgd` JSON field matching.

## Notes and limits

- Apple `container` has no restart policy, so nodes don't come back after a
  reboot. `cider pgd start` brings them back.
- Each node is a lightweight VM, not a process — three at 2 GB is ~6 GB of RAM.
- The web UI is plain HTTP, and `pg_stat_statements` aside, nothing here is
  tuned — the defaults are whatever PGD ships. See
  [What this is for](#what-this-is-for) for the rest of the caveats.

## Links

- [EDB Postgres Distributed docs](https://www.enterprisedb.com/docs/pgd/latest/)
- [PGD 6.5.0 release notes](https://www.enterprisedb.com/docs/pgd/latest/rel_notes/pgd_6.5.0_rel_notes/)
- [PGD Monitor](https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/pgd-monitor/) · [web UI tour](https://www.enterprisedb.com/docs/pgd/latest/lifecycle/monitoring/pgd-monitor/web-ui/)
- [Connection Manager](https://www.enterprisedb.com/docs/pgd/latest/connection-manager/)
- [PGD Docker quickstart](https://www.enterprisedb.com/docs/pgd/latest/quickstart/first-cluster/) — the original
- [apple/container](https://github.com/apple/container) · [networking docs](https://github.com/apple/container/blob/main/docs/networking.md)
