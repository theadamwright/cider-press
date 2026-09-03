# cider-press — EDB Postgres Distributed node image, built from scratch.
#
# The EDB subscription token is consumed as a BuildKit *secret*, never as an ARG
# and never as an ENV. It is mounted into a single RUN layer, used to configure
# the apt repositories, and the repository files that embed it are deleted in
# that same layer. Nothing token-bearing survives into the final image, so the
# token cannot be recovered from `container image inspect` or the layer history.

# Pinned to a major version on purpose, and not to be bumped casually.
#
# EDB certifies specific operating system majors and is conservative about
# adding new ones, so a newer Debian generally has no PGD packages for months
# after release. Tracking `latest` or `stable` would eventually produce a build
# that fails at `apt-get install` with no obvious cause.
#
# Before raising this, check the row for the target OS in EDB's compatibility
# matrix, and confirm arm64 specifically:
#   https://www.enterprisedb.com/docs/pgd/latest/compatibility/
ARG DEBIAN_VERSION=12
FROM debian:${DEBIAN_VERSION}-slim

# PG_FLAVOR: pge (EDB Postgres Extended) | epas (EDB Postgres Advanced Server)
#            | pg (community PostgreSQL)
ARG PG_FLAVOR=pge
ARG PG_MAJOR=18

LABEL org.opencontainers.image.title="cider-press" \
      org.opencontainers.image.description="EDB Postgres Distributed node for Apple container" \
      io.cider-press.pg-flavor="${PG_FLAVOR}" \
      io.cider-press.pg-major="${PG_MAJOR}"

ENV DEBIAN_FRONTEND=noninteractive

# Base tooling. setpriv (util-linux) is how the entrypoint drops from root to
# the postgres user while keeping postgres as PID 1; it is verified here so a
# missing binary is a build failure rather than a confusing runtime failure.
RUN apt-get update -y \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg lsb-release locales \
        jq iproute2 iputils-ping procps less \
 && command -v setpriv >/dev/null || { echo "setpriv missing from util-linux" >&2; exit 1; } \
 && sed -i 's/^# *\(en_US.UTF-8\)/\1/' /etc/locale.gen \
 && locale-gen \
 && rm -rf /var/lib/apt/lists/*

ENV LANG=en_US.UTF-8

# EDB repositories, Postgres and PGD. Everything token-touching happens in this
# one RUN: the repo definitions EDB's setup.deb.sh writes embed the token in
# their URLs, so they are removed before the layer is committed, and the layer
# is then audited for any surviving credential-bearing URL.
# "expanded" in the package names below is a naming artifact, not a choice.
# PGD Essential was sunset in PGD 6.5.0 -- there is no longer a separate tier,
# and the remaining packages simply kept the word in their names.
RUN --mount=type=secret,id=edb_token,required=true \
    set -eu; \
    EDB_SUBSCRIPTION_TOKEN="$(cat /run/secrets/edb_token)"; \
    case "${PG_FLAVOR}" in \
      pge)  SERVER_PKG="edb-postgresextended-${PG_MAJOR}"; \
            PGD_PKG="edb-pgd6-expanded-pgextended${PG_MAJOR}"; \
            PG_BINDIR="/usr/lib/edb-pge/${PG_MAJOR}/bin"; \
            PG_SUPERUSER="postgres" ;; \
      epas) SERVER_PKG="edb-as${PG_MAJOR}-server"; \
            PGD_PKG="edb-pgd6-expanded-epas${PG_MAJOR}"; \
            PG_BINDIR="/usr/lib/edb-as/${PG_MAJOR}/bin"; \
            PG_SUPERUSER="enterprisedb" ;; \
      pg)   SERVER_PKG="postgresql-${PG_MAJOR}"; \
            PGD_PKG="edb-pgd6-expanded-pg${PG_MAJOR}"; \
            PG_BINDIR="/usr/lib/postgresql/${PG_MAJOR}/bin"; \
            PG_SUPERUSER="postgres" ;; \
      *)    echo "unsupported PG_FLAVOR=${PG_FLAVOR} (want pge|epas|pg)" >&2; exit 1 ;; \
    esac; \
    curl -1sLf "https://downloads.enterprisedb.com/${EDB_SUBSCRIPTION_TOKEN}/enterprise/setup.deb.sh" | bash; \
    curl -1sLf "https://downloads.enterprisedb.com/${EDB_SUBSCRIPTION_TOKEN}/postgres_distributed/setup.deb.sh" | bash; \
    if [ "${PG_FLAVOR}" = "pg" ]; then \
      install -d /usr/share/keyrings; \
      curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /usr/share/keyrings/pgdg.gpg; \
      echo "deb [signed-by=/usr/share/keyrings/pgdg.gpg] http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list; \
    fi; \
    apt-get update -y; \
    apt-get install -y --no-install-recommends "${SERVER_PKG}" "${PGD_PKG}"; \
    install -d /etc/cider-press; \
    { \
      echo "PG_FLAVOR=${PG_FLAVOR}"; \
      echo "PG_MAJOR=${PG_MAJOR}"; \
      echo "PG_BINDIR=${PG_BINDIR}"; \
      echo "PG_SUPERUSER=${PG_SUPERUSER}"; \
    } > /etc/cider-press/image.env; \
    install -d /opt/cider-press; \
    ln -sfn "${PG_BINDIR}" /opt/cider-press/bin; \
    rm -f /etc/apt/sources.list.d/*enterprisedb* \
          /etc/apt/sources.list.d/*enterprise_db* \
          /etc/apt/auth.conf.d/*enterprisedb* \
          /etc/apt/auth.conf.d/*enterprise_db* || true; \
    rm -rf /var/lib/apt/lists/* /root/.gnupg /tmp/* ; \
    if grep -rIl 'downloads\.enterprisedb\.com/[0-9a-zA-Z]\{8,\}' /etc 2>/dev/null | grep -q .; then \
      echo "SECURITY: subscription token still present under /etc" >&2; \
      grep -rIl 'downloads\.enterprisedb\.com/[0-9a-zA-Z]\{8,\}' /etc >&2; \
      exit 1; \
    fi

ENV PATH="/opt/cider-press/bin:${PATH}"

# One flavor-independent location for all node state. The named volume mounts
# at /var/lib/cider-press, so PGDATA and the server log travel together and
# survive `cider down`.
ENV PGDATA="/var/lib/cider-press/data" \
    CIDER_PRESS_LOG="/var/lib/cider-press/log/postgres.log"

RUN set -eu; \
    . /etc/cider-press/image.env; \
    install -d -o "${PG_SUPERUSER}" -g "${PG_SUPERUSER}" -m 0750 /var/lib/cider-press; \
    install -d -o "${PG_SUPERUSER}" -g "${PG_SUPERUSER}" -m 0700 /var/lib/cider-press/data; \
    install -d -o "${PG_SUPERUSER}" -g "${PG_SUPERUSER}" -m 0750 /var/lib/cider-press/log; \
    install -d -o "${PG_SUPERUSER}" -g "${PG_SUPERUSER}" -m 0750 /etc/edb/pgd-cli

COPY entrypoint.sh /usr/local/bin/cider-press-entrypoint
RUN chmod 0755 /usr/local/bin/cider-press-entrypoint

# 5432 postgres · 6432/6433 Connection Manager rw/ro · 6434 CM health API
# 6437 PGD Monitor web UI, REST API and /metrics (Postgres port + 1005)
EXPOSE 5432 6432 6433 6434 6437

# Starts as root so it can fix ownership on the freshly-mounted volume, then
# re-execs itself as the Postgres superuser via setpriv.
ENTRYPOINT ["/usr/local/bin/cider-press-entrypoint"]
