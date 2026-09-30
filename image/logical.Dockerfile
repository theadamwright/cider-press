# cider-press — community PostgreSQL node for `cider logical`, built from
# scratch. No subscription token: PGDG's repository is public.
#
# Sits beside PGD's Dockerfile, not in a directory of its own, because the
# build context is this directory and the entrypoint needs lib/.

# Pinned to a major version on purpose, independently of PGD's image. PGDG
# publishes for a new Debian release within weeks, where EDB takes months, so
# this one can move first. Check PGDG has arm64 packages before raising it:
#   https://apt.postgresql.org/pub/repos/apt/dists/
ARG DEBIAN_VERSION=13
FROM debian:${DEBIAN_VERSION}-slim

ARG PG_MAJOR=18

LABEL org.opencontainers.image.title="cider-press-logical" \
      org.opencontainers.image.description="Community PostgreSQL node for logical replication on Apple container" \
      io.cider-press.pg-major="${PG_MAJOR}"

ENV DEBIAN_FRONTEND=noninteractive

# Base tooling. setpriv (util-linux) is how the entrypoint drops from root to
# the postgres user while keeping postgres as PID 1; verified here so a missing
# binary is a build failure rather than a confusing runtime one.
RUN apt-get update -y \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg locales \
        iproute2 iputils-ping procps less \
 && command -v setpriv >/dev/null || { echo "setpriv missing from util-linux" >&2; exit 1; } \
 && sed -i 's/^# *\(en_US.UTF-8\)/\1/' /etc/locale.gen \
 && locale-gen \
 && rm -rf /var/lib/apt/lists/*

ENV LANG=en_US.UTF-8

# PostgreSQL from PGDG, the PostgreSQL project's own repository.
#
# Debian's packaging creates and starts a cluster called "main" as part of the
# install. The entrypoint makes its own on the volume instead, so that is
# switched off first (postgresql-common owns the setting), and nothing is left
# under /var/lib/postgresql to be confused with the real PGDATA.
RUN set -eu; \
    install -d /usr/share/keyrings; \
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
      | gpg --dearmor -o /usr/share/keyrings/pgdg.gpg; \
    . /etc/os-release; \
    echo "deb [signed-by=/usr/share/keyrings/pgdg.gpg] https://apt.postgresql.org/pub/repos/apt ${VERSION_CODENAME}-pgdg main" \
      > /etc/apt/sources.list.d/pgdg.list; \
    apt-get update -y; \
    apt-get install -y --no-install-recommends postgresql-common; \
    sed -ri 's/^#?\s*create_main_cluster\s*=.*/create_main_cluster = false/' \
      /etc/postgresql-common/createcluster.conf; \
    grep -q '^create_main_cluster = false' /etc/postgresql-common/createcluster.conf \
      || { echo "could not disable the default cluster" >&2; exit 1; }; \
    apt-get install -y --no-install-recommends "postgresql-${PG_MAJOR}"; \
    PG_BINDIR="/usr/lib/postgresql/${PG_MAJOR}/bin"; \
    test -x "${PG_BINDIR}/postgres" || { echo "no postgres in ${PG_BINDIR}" >&2; exit 1; }; \
    install -d /etc/cider-press; \
    { \
      echo "PG_FLAVOR=pgdg"; \
      echo "PG_MAJOR=${PG_MAJOR}"; \
      echo "PG_BINDIR=${PG_BINDIR}"; \
      echo "PG_SUPERUSER=postgres"; \
    } > /etc/cider-press/image.env; \
    install -d /opt/cider-press; \
    ln -sfn "${PG_BINDIR}" /opt/cider-press/bin; \
    rm -rf /var/lib/apt/lists/*

ENV PATH="/opt/cider-press/bin:${PATH}"

# Debian builds Postgres with its Unix socket in /var/run/postgresql. On a
# normal system that directory is made at boot; in a container nothing makes
# it, and Postgres refuses to start without it.
RUN install -d -o postgres -g postgres -m 2775 /var/run/postgresql

# The same layout as PGD's image: the named volume mounts at
# /var/lib/cider-press, so PGDATA and the server log travel together.
ENV PGDATA="/var/lib/cider-press/data" \
    CIDER_PRESS_LOG="/var/lib/cider-press/log/postgres.log"

COPY logical-entrypoint.sh /usr/local/bin/cider-press-entrypoint
RUN chmod 0755 /usr/local/bin/cider-press-entrypoint

# Helpers every product's entrypoint shares; sourced, so not executable.
COPY lib/node-common.sh /usr/local/lib/cider-press/node-common.sh

EXPOSE 5432

# SIGINT is a fast shutdown, which is what stopping a lab container should
# mean; SIGTERM, the default, is a smart shutdown that waits for clients to
# leave. cider sends SIGINT explicitly; this is for `container stop` by hand.
STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/cider-press-entrypoint"]
