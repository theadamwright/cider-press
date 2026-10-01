```
                       \ | /
                     .--'--.              .-------.
                    /        \            |~~~~~~~|
                   |    ()    |           |~~~~~~~|
                    \        /            |~~~~~~~|
                     '-.__.-'             '._____.'

                c i d e r - p r e s s
         Postgres hosts, built to be broken,
         pressed on Apple's container runtime
```

A from-scratch local lab for **EDB Postgres Distributed 6.5.0+**, **EDB Failover
Manager 5.4** and **core PostgreSQL logical replication** on Apple silicon, built
on [`apple/container`](https://github.com/apple/container) instead of Docker
Desktop. Build the image, press a cluster, tear it all down again — one small
script, no daemon you have to remember to quit.

A spiritual port of the [PGD Docker quickstart](https://www.enterprisedb.com/docs/pgd/latest/quickstart/first-cluster/),
rebuilt for a runtime with no `compose`.

> [!IMPORTANT]
> cider-press is **not** endorsed or supported by EnterpriseDB, and is not covered 
> by any EDB support agreement or SLA. Please don't raise EDB support tickets about it 
> , open an issue here instead. PGD and Failover Manager are commercial software: you need
> your own valid EDB subscription, and your use of the packages they install is governed by
> your agreement with EDB, not by this repository's licence.

**Contents** · [What this is for](#what-this-is-for) ·
[Why Apple container](#why-apple-container) · [Expanding](#could-this-do-more-than-pgd) ·
[Requirements](#requirements) · [Build](#1-build) · [Up](#2-up) ·
[Write leader](#3-connect-to-the-write-leader) ·
[Read-only](#4-read-only-load-balanced-across-nodes) ·
[Web UI](#5-connect-to-the-web-ui) · [Tear down](#6-tear-down) ·
[Pooling](#connection-pooling) ·
[Logical replication](#cider-logical-core-postgresql-logical-replication) ·
[Failover Manager](#cider-efm-edb-failover-manager) ·
[Commands](#command-reference) ·
[Status](#how-cider-pgd-status-reads-the-cluster) · [DNS](#the-dns-part-the-only-genuinely-tricky-bit) ·
[Configuration](#configuration) · [Token safety](#about-that-subscription-token) ·
[Troubleshooting](#troubleshooting)

---

## What this is for

Getting a working PGD, Failover Manager or logical-replication cluster in front
of you in a few minutes, with the least friction possible: for **evaluation, testing, demos and learning**. Build the
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
or [Hybrid Manager](https://www.enterprisedb.com/docs/pgd/latest/deploying/deploy-hm/),
and for Failover Manager, its
[installation guide](https://www.enterprisedb.com/docs/efm/latest/installing/).

## Could this do more than PGD?

It already does two more:

- [**`cider logical`**](#cider-logical-core-postgresql-logical-replication): a
  pair of community PostgreSQL nodes for wiring up logical replication by hand.
  It needs no EDB subscription.
- [**`cider efm`**](#cider-efm-edb-failover-manager): a primary and two standbys
  under EDB Failover Manager, behind a load balancer that follows the primary
  through a failover. The classic counterpart to PGD's active-active.

Commands are grouped as `cider <product> <verb>`, and the lifecycle underneath
(start, wait, retry, stop, tear down) is shared, so another stack is a new group
reusing both rather than a rewrite.
[ARCHITECTURE.md](ARCHITECTURE.md#adding-a-second-product) describes how a
product is added.

One more candidate, not built and not promised:
**[Patroni](https://patroni.readthedocs.io/)**, the same failover problem solved
in the open-source world, with etcd for consensus. If you'd find it useful, say
so in an issue.

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
| Credentials | An EDB subscription token, to build the PGD and Failover Manager images (`cider logical` needs none) — [get one here](https://www.enterprisedb.com/repos-downloads) |
| RAM | Each node is allowed 2 GB by default: ~6 GB for PGD or Failover Manager, 4 GB for logical. Far less is actually used; see [Notes and limits](#notes-and-limits) |

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

PGD is active-active, but when going through the Connection Manager, writes are routed to one node at a time for strong consistency requirements. Rather than tracking which one, connect through **Connection Manager**, which routes port
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

## `cider logical`: core PostgreSQL logical replication

Two community PostgreSQL 18 nodes, `dolores-1` and `dolores-2`, built from the
[PGDG](https://www.postgresql.org/download/linux/debian/) repository on Debian 13,
both with `wal_level = logical`. cider creates the nodes; the replication between
them is yours to create, which is the point: it's a quick way to see what core
logical replication does, and what it leaves to you.

It needs no EDB subscription or token, and it runs alongside a PGD cluster, with
its own containers, volumes, image and ports (5442 and 5443).

```bash
./cider logical build
```

```bash
./cider logical up
```

The build takes about 20 seconds. `up` ends by printing the SQL that connects the
pair in both directions (`./cider logical endpoints` prints it again):

```sql
-- on BOTH nodes
CREATE TABLE pingpong (id int PRIMARY KEY, msg text);
CREATE PUBLICATION pp FOR TABLE pingpong;

-- on dolores-1 (./cider logical psql 1)
CREATE SUBSCRIPTION from_dolores_2
  CONNECTION 'host=dolores-2.cider dbname=demo tcp_user_timeout=5000'
  PUBLICATION pp
  WITH (origin = none, copy_data = false);

-- on dolores-2 (./cider logical psql 2)
CREATE SUBSCRIPTION from_dolores_1
  CONNECTION 'host=dolores-1.cider dbname=demo tcp_user_timeout=5000'
  PUBLICATION pp
  WITH (origin = none, copy_data = false);
```

Paste each part into `./cider logical psql 1` and `./cider logical psql 2`. Then
`./cider logical status` shows both subscriptions running:

```
  NODE          WAL       PUBLICATIONS    SUBSCRIPTIONS
  dolores-1     logical   pp              from_dolores_2 streaming
  dolores-2     logical   pp              from_dolores_1 streaming
```

A row inserted on either node now appears on the other. It keeps doing so across
`./cider logical stop` and `start`: after a restart each subscription reconnects
to its peer by itself, within about 10 seconds.

The apply-error count in `status` is PostgreSQL's own
(`pg_stat_subscription_stats.apply_error_count`). It counts *every* error the
apply worker hits, including failing to reach its peer after a restart, so a
small count after a restart is expected. Check the node's log
(`./cider logical logs 1`) before assuming a conflict.

Why the SQL looks the way it does:

- **`origin = none`** (PostgreSQL 16+) stops each node sending the other's changes
  straight back to it.
- **`copy_data = false`**, because both tables start empty.
- **The fully-qualified name** in `CONNECTION`. It's stored, and resolved again on
  every reconnect, so it needs the name that always resolves; see
  [the DNS part](#the-dns-part-the-only-genuinely-tricky-bit).
- **`tcp_user_timeout=5000`** makes a connection attempt to the peer give up
  after 5 seconds and try again. Without it, restarting both nodes regularly
  left a subscription's first attempt hanging for about two minutes (the
  kernel's TCP connect timeout) before it retried. `connect_timeout` doesn't
  help here: subscriptions connect in a way that ignores it. PGD sets the same
  option on its own connections.
- **No password.** Each node's server inherits the lab password from its
  environment, and a superuser's subscription can use it. A non-superuser's
  subscription must put `password=` in the connection string.

### Seeing a conflict

Core logical replication detects and logs conflicts, but doesn't resolve them.
Two inserts typed quickly usually won't collide: the first replicates before the
second is typed. To make one happen, pause both subscriptions, insert the same
key on each node, then resume them.

On `dolores-1`:

```sql
ALTER SUBSCRIPTION from_dolores_2 DISABLE;
INSERT INTO pingpong VALUES (4, 'from dolores-1');
```

On `dolores-2`:

```sql
ALTER SUBSCRIPTION from_dolores_1 DISABLE;
INSERT INTO pingpong VALUES (4, 'from dolores-2');
ALTER SUBSCRIPTION from_dolores_1 ENABLE;
```

Back on `dolores-1`:

```sql
ALTER SUBSCRIPTION from_dolores_2 ENABLE;
```

Now each node keeps its own row 4. Each node's log names the problem
(`conflict detected on relation "public.pingpong": conflict=insert_exists`), and
`status` shows the apply worker retrying:

```
  dolores-1     logical   pp              from_dolores_2 stopped, 4 apply errors
```

Nothing written afterwards reaches the other node until the conflict is resolved
by hand. To start again from nothing, run
`./cider logical pomace -y && ./cider logical build && ./cider logical up`.

## `cider efm`: EDB Failover Manager

A primary and two standbys of EDB Postgres Extended 18 on streaming replication,
each watched by an [EDB Failover Manager](https://www.enterprisedb.com/docs/efm/latest/)
5.4 agent, plus `maeve-lb`, an HAProxy load balancer that always reaches the
primary. Kill the primary and watch a standby take over, the load balancer
follow it, and the old primary rebuild itself and rejoin as a standby. Like PGD,
it needs your EDB subscription token to build.

```bash
./cider efm build
```

```bash
./cider efm up
```

`up` takes about 40 seconds: the load balancer, then `maeve-1` as the primary,
then `maeve-2` and `maeve-3` cloned from it.

```
  NODE        POSTGRES    EFM AGENT     HEALTH
  maeve-1     primary     primary       200
  maeve-2     standby     not primary   404
  maeve-3     standby     not primary   404

  load balancer → maeve-1   (127.0.0.1:5450)
```

### How clients find the primary

Failover Manager 5.4 added an HTTP health endpoint to every agent
(`primary.health.check.port`): it answers **200 on the primary and 404
everywhere else**, for a load balancer to poll. HAProxy asks every node once a
second, so its one port reaches whichever node is primary, and follows it after
a failover. It plays the part PGD's Connection Manager plays for the write
leader.

```bash
PGPASSWORD=secret psql -h 127.0.0.1 -p 5450 -U postgres efmdb
```

`./cider efm pour` does the same from inside the load balancer's container, out
of reach of macOS networking. `./cider efm ui` opens HAProxy's stats page at
<http://127.0.0.1:7880/>, which shows which node is primary right now. The
primary is the one marked UP. The standbys show DOWN, which only means "not the
primary". Each node's own endpoint is published too (`:7881`, `:7882`, `:7883`),
so you can watch the 200 move:

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7881/
```

### Trying a failover

Stop the primary's database the hard way, from a shell on the node:

```bash
./cider efm shell 1
```

```bash
su postgres -c 'pg_ctl -D $PGDATA -m immediate stop'
```

Then watch `./cider efm status` from the Mac. What happened, measured:

| after | |
|---|---|
| ~60 s | Failover Manager declares the primary failed. That's its default detection timing (`local.period`, `local.timeout` and friends), tunable in its properties |
| +4 s | `maeve-2` is promoted, answers 200, and the load balancer switches to it within a second. `maeve-3` is repointed at it |
| seconds later | `maeve-1` rebuilds itself as a standby of `maeve-2` and rejoins |

A client writing through `:5450` every half second saw its writes fail for that
minute, then carry on against `maeve-2` through the same address. Failover
Manager's notifications go to each node's log (`./cider efm logs 2`).

### Failover Manager's own CLI

`cider efm cli` runs Failover Manager's `efm` command inside a running node, as
`cider pgd cli` runs the PGD CLI, and fills in the cluster name, so you type
only the command:

```bash
./cider efm cli cluster-status
```

```bash
./cider efm cli replication-status
```

`replication-status`, new in 5.4, shows each standby's upstream, sync state and
lag. Typing the cluster name yourself (`cluster-status maeve`) works too. A few
commands act on the node they run on (`create-standby`, `node-status-json`,
`resume`), and `cli` runs on the first running node. For another node, use
`./cider efm shell 2` and run `efm` there.

The automatic rejoin is Failover Manager's `auto.rewind` and `auto.basebackup`,
which cider turns on. It tries `pg_rewind` first and falls back to
`pg_basebackup`. After a hard crash like the one above, `pg_rewind` declines
because the old primary needs crash recovery first, so expect the
`pg_basebackup` path. Set `CIDER_EFM_AUTO_REJOIN=off` to see the alternative: the
failed primary stays fenced, and Failover Manager leaves a `recovery.conf` in its
data directory so it can't come back as a second primary.

### Why no virtual IP

A VIP was the first idea, and both kinds were tested. An **IPv4** VIP moves
instantly, because Failover Manager announces it with a gratuitous ARP. But the
runtime hands out IPv4 addresses in sequence and doesn't know about the VIP. It
was seen giving the VIP's address to a new container, then wrapping round to
`.2`, so sooner or later a VIP collides. An **IPv6** VIP is safe from that, but
Failover Manager doesn't announce IPv6 VIPs, so clients kept using the old node
for 40 seconds after every move. The load balancer has neither problem, and
everything it publishes is on `127.0.0.1`, out of a VPN's way.

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

**Logical replication** — `cider logical …`

| | |
|---|---|
| `cider logical build [--no-cache]` | Build the node image. No token needed |
| `cider logical up` / `press` | Create both nodes, then print the SQL to wire them up |
| `cider logical status` / `ps` | Each node's publications, subscriptions and apply errors |
| `cider logical endpoints` | How to connect, and the wiring SQL again |
| `cider logical psql [node] [args…]` | psql to a node (default 1) |

`containers`, `stop`, `start`, `down`, `pomace`, `shell` and `logs` work exactly
as they do for `pgd`. `[node]` takes `2` or `dolores-2`. There's no `pour`, `cli`
or `ui`: no leader to route to, no product CLI and no web UI.

**Failover Manager** — `cider efm …`

| | |
|---|---|
| `cider efm build [--no-cache]` | Build the image the nodes and the load balancer share. Needs `EDB_SUBSCRIPTION_TOKEN` |
| `cider efm up` / `press` | Create the load balancer, the primary and the standbys |
| `cider efm status` / `ps` | Each node's role, and where the load balancer is routing |
| `cider efm endpoints` | Every published port, and how to try a failover |
| `cider efm ui` / `web` | Open HAProxy's stats page: which node is primary |
| `cider efm pour` | psql to the primary through the load balancer |
| `cider efm psql [node] [args…]` | psql straight to a node (default 1) |
| `cider efm cli <args…>` | Failover Manager's `efm` command, cluster name filled in, e.g. `cli cluster-status` |

`containers`, `stop`, `start`, `down`, `pomace`, `shell` and `logs` work as they
do for `pgd`, and include the load balancer. `[node]` takes `2`, `maeve-2`, or
`lb` for the load balancer.

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
- **Nodes don't wait for that DNS to find each other.** After a container
  starts, the runtime often takes a minute or two to register its name, and
  every restart used to leave nodes `Unreachable` for that long. Instead, each
  container gets a fixed MAC address derived from its name, which makes its
  IPv6 address the same on every start. Each node's entrypoint then writes every
  node's name and IPv6 address into its own `/etc/hosts` when it starts, and
  lookups check that file before DNS. A whole-cluster restart went from 17–85
  seconds to 0–11. The runtime's DNS still serves your Mac, and is the fallback
  if a node has no global IPv6 address.

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

`cider logical up` creates two more, with nothing in common with those above, so
both can run at once:

| container | volume | postgres |
|---|---|---|
| `dolores-1` | `cider-press-dolores-1` | **5442** |
| `dolores-2` | `cider-press-dolores-2` | 5443 |

Its image is `cider-press-logical:latest`, and its database is `demo`.

`cider efm up` creates four more:

| container | role | volume | postgres | health endpoint |
|---|---|---|---|---|
| `maeve-lb` | HAProxy: always the primary | — | **5450** | stats page **7880** |
| `maeve-1` | primary, at first | `cider-press-maeve-1` | 5451 | 7881 |
| `maeve-2` | standby | `cider-press-maeve-2` | 5452 | 7882 |
| `maeve-3` | standby | `cider-press-maeve-3` | 5453 | 7883 |

Its image is `cider-press-efm:latest`, its database `efmdb`, and Failover
Manager's cluster name is `maeve`.

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

`cider logical` has its own settings, all prefixed `CIDER_LOGICAL_` so none can
be confused with PGD's. The shared ones above (`CIDER_DOMAIN`, `CIDER_PASSWORD`,
`CIDER_CPUS`, `CIDER_MEMORY`, `CIDER_READY_TIMEOUT`) apply to both.

| Variable | Default | |
|---|---|---|
| `CIDER_LOGICAL_PG_MAJOR` | `18` | 16 or later, for `origin = none` |
| `CIDER_LOGICAL_DEBIAN_VERSION` | `13` | Separate from PGD's `DEBIAN_VERSION` (12): PGDG supports new Debian releases months before EDB does |
| `CIDER_LOGICAL_DB` | `demo` | The database both nodes create |
| `CIDER_LOGICAL_PG_PORT_BASE` | `5442` | `dolores-1` gets this, `dolores-2` the next |

`cider efm`'s settings are prefixed `CIDER_EFM_`, and the same shared ones apply.

| Variable | Default | |
|---|---|---|
| `CIDER_EFM_VERSION` | `5.4` | Failover Manager version. 5.4 is the first with the health endpoint the load balancer needs |
| `CIDER_EFM_PG_MAJOR` | `18` | EDB Postgres Extended major version |
| `CIDER_EFM_AUTO_REJOIN` | `on` | A failed primary rebuilds itself as a standby. `off` leaves it fenced |
| `CIDER_EFM_DB` | `efmdb` | The database Failover Manager monitors |
| `CIDER_EFM_PORT_BASE` | `5450` | The load balancer; node *i* is on the port *i* above it |
| `CIDER_EFM_WEB_PORT_BASE` | `7880` | The stats page; node *i*'s health endpoint is *i* above it |

---

## About that subscription token

The token is the credential for your EDB repositories, so this repo makes
committing it hard:

- **`.env` is gitignored**, and `.env.example` carries no value.
- **The builds that need it take it as a BuildKit secret**: PGD's and Failover
  Manager's, with `--secret id=edb_token,env=…`, read from the environment of the
  `cider` process. Never a `--build-arg`, so it never lands in the image config
  or layer history where `container image inspect` would show it.
- **The apt repo files that embed the token are deleted in the same layer that
  creates them.** EDB's `setup.deb.sh` writes the token into
  `/etc/apt/sources.list.d/`, which would otherwise ship inside the image.
- **Each build audits itself**, failing if any credential-bearing
  `downloads.enterprisedb.com` URL survives under `/etc`.

None of this is strictly required for a throwaway local cluster, but a
subscription token is worth being careful with by default. Anyone cloning this
exports their own token — nothing about the subscription is baked into the repo.

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

**A node waits a minute or more at "waiting for host-2", or shows `Unreachable`
after `start`.** After a container starts, Apple's `container` often takes a
minute or two to register its name in DNS. Nodes don't wait for that: every
node is given a fixed MAC address, which fixes its IPv6 address, and each
node's entrypoint writes every node's name and address into its own
`/etc/hosts` at each start (see
[the DNS part](#the-dns-part-the-only-genuinely-tricky-bit)). So a long wait
usually means the containers were created by an older cider, before that
existed. Both `up` and `status` say so when a node's name isn't resolving:

```
  ! host-2.cider is not in the runtime's DNS yet
```

Recreate the containers to pick the fix up. This keeps your data, but the image
needs the new entrypoint too, so rebuild it first:

```bash
./cider pgd build && ./cider pgd down && ./cider pgd up
```

If `status` instead says the container is stopped, run `cider pgd start`.

If a name still doesn't resolve after three minutes, the node gives up and `up`
prints its last 40 log lines. Then the DNS domain may not be in effect:
`cider pgd shell 1`, then `getent hosts host-2.cider`. If nothing comes back,
run `container system stop && container system start`, then `cider doctor`.

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

**An EFM node exits during `up`, or `status` says its agent is "not
answering".** `./cider efm logs 1` shows the entrypoint's steps and Failover
Manager's startup messages, including the reason an agent quit. The agent's full
log is inside the node: `./cider efm shell 1`, then
`less /var/log/efm-5.4/maeve.log`. `./cider efm cli cluster-status` gives
Failover Manager's view of every agent. A node whose database has failed shows
as *Idle* there, and as "not primary" in `status`.

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
│   ├── lifecycle.rs      # what every product shares: start, wait, stop, teardown
│   ├── pgd.rs            # PGD's own: containers, readiness, endpoints, web UI
│   ├── logical.rs        # cider logical: the pair, its status, the wiring SQL
│   ├── efm.rs            # cider efm: the nodes, the load balancer, status, failover
│   ├── state.rs          # live cluster state via `pgd -o json`
│   ├── monitor.rs        # PGD Monitor probes
│   └── term.rs           # colour, glyphs, banner
├── image/
│   ├── Dockerfile        # Debian 12 + PGE 18 + PGD 6.5, token as a secret
│   ├── entrypoint.sh     # per-node provisioning, join, monitor enablement
│   ├── logical.Dockerfile      # Debian 13 + PGDG PostgreSQL 18, no token
│   ├── logical-entrypoint.sh   # initdb, wal_level = logical, the demo database
│   ├── efm.Dockerfile          # Debian 12 + PGE 18 + EFM 5.4 + HAProxy, token as a secret
│   ├── efm-entrypoint.sh       # provision, configure EFM, supervise postgres and the agent
│   ├── efm-lb-entrypoint.sh    # HAProxy, polling every agent's health endpoint
│   └── lib/
│       └── node-common.sh  # helpers every node image shares: DNS wait, pg_hba, listen
├── .env.example
├── ARCHITECTURE.md       # notes for working *on* cider
├── LICENSE
└── README.md
```

Run `cargo test` for the unit tests — they cover the config rewriter, the
`container` output parsing, the `pgd` JSON field matching, the naming and port
schemes, every command and alias, and `cider logical`'s status parsing and
wiring SQL.

## Notes and limits

- Apple `container` has no restart policy, so nodes don't come back after a
  reboot. `cider pgd start` (or `logical start`, `efm start`) brings them back.
- Each node is a lightweight VM, not a process, allowed 2 GB by default. In
  practice they hold much less, because allocation is lazy. Measured with all
  three products running: a PGD node about 350 MB, a Failover Manager node about
  410 MB (its agent is Java), a logical node about 210 MB, and the load balancer
  45 MB. That's under 3 GB for all nine containers, plus about 1.7 GB for the
  runtime's build container while it's up.
- **Nodes are stopped with SIGINT, not SIGTERM.** Postgres reads SIGTERM as a
  *smart* shutdown and waits for every client to disconnect — and PGD nodes
  hold connections open to each other, so that wait never ends. `container
  stop` then kills the server after its five-second default and the node
  crash-recovers on the way back up. `cider pgd stop` / `down` / `pomace` pass
  `--signal SIGINT --time 60` instead, and the image sets `STOPSIGNAL SIGINT`
  so a hand-run `container stop host-1` behaves the same way. Rebuild the image
  if you want that second part.
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
