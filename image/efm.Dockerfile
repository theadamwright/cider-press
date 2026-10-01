# cider-press — EDB Failover Manager node for `cider efm`, built from scratch:
# EDB Postgres Extended, Failover Manager and HAProxy on Debian.
#
# One image, two roles. The database nodes run image/efm-entrypoint.sh (the
# default). The load balancer runs image/efm-lb-entrypoint.sh, chosen with
# --entrypoint, and uses only the HAProxy in here. Building one image keeps it
# to one `cider efm build`.
#
# The EDB subscription token is handled exactly as in PGD's Dockerfile: a
# BuildKit secret, mounted into one RUN, with the repository files that embed
# it deleted in that same layer and the layer audited afterwards.

# Pinned for the same reason as PGD's image: EDB certifies OS majors slowly.
# edb-efm54 is published for Debian 12 arm64; check before raising this.
ARG DEBIAN_VERSION=12
FROM debian:${DEBIAN_VERSION}-slim

ARG PG_MAJOR=18
ARG EFM_VERSION=5.4

LABEL org.opencontainers.image.title="cider-press-efm" \
      org.opencontainers.image.description="EDB Failover Manager node for Apple container" \
      io.cider-press.pg-major="${PG_MAJOR}" \
      io.cider-press.efm-version="${EFM_VERSION}"

ENV DEBIAN_FRONTEND=noninteractive

# Base tooling, plus what the Failover Manager package needs but does not
# depend on: a Java runtime (11 or later, per EFM's installation docs) and
# sudo, which its /etc/sudoers.d file assumes is present. HAProxy is Debian's
# own package, for the load balancer.
RUN apt-get update -y \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg lsb-release locales \
        default-jre-headless sudo haproxy \
        jq iproute2 iputils-ping procps less \
 && command -v setpriv >/dev/null || { echo "setpriv missing from util-linux" >&2; exit 1; } \
 && command -v runuser >/dev/null || { echo "runuser missing from util-linux" >&2; exit 1; } \
 && java -version 2>&1 | head -1 \
 && sed -i 's/^# *\(en_US.UTF-8\)/\1/' /etc/locale.gen \
 && locale-gen \
 && rm -rf /var/lib/apt/lists/*

ENV LANG=en_US.UTF-8

RUN --mount=type=secret,id=edb_token,required=true \
    set -eu; \
    EDB_SUBSCRIPTION_TOKEN="$(cat /run/secrets/edb_token)"; \
    EFM_PKG="edb-efm$(echo "${EFM_VERSION}" | tr -d .)"; \
    curl -1sLf "https://downloads.enterprisedb.com/${EDB_SUBSCRIPTION_TOKEN}/enterprise/setup.deb.sh" | bash; \
    apt-get install -y --no-install-recommends "edb-postgresextended-${PG_MAJOR}" "${EFM_PKG}"; \
    PG_BINDIR="/usr/lib/edb-pge/${PG_MAJOR}/bin"; \
    EFM_HOME="/usr/edb/efm-${EFM_VERSION}"; \
    test -x "${PG_BINDIR}/postgres" || { echo "no postgres in ${PG_BINDIR}" >&2; exit 1; }; \
    test -x "${EFM_HOME}/bin/runefm.sh" || { echo "no runefm.sh in ${EFM_HOME}" >&2; exit 1; }; \
    install -d /etc/cider-press; \
    { \
      echo "PG_FLAVOR=pge"; \
      echo "PG_MAJOR=${PG_MAJOR}"; \
      echo "PG_BINDIR=${PG_BINDIR}"; \
      echo "PG_SUPERUSER=postgres"; \
      echo "EFM_VERSION=${EFM_VERSION}"; \
      echo "EFM_HOME=${EFM_HOME}"; \
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

# The same state layout as every cider-press image: the named volume mounts at
# /var/lib/cider-press, so PGDATA and the server log travel together.
ENV PGDATA="/var/lib/cider-press/data" \
    CIDER_PRESS_LOG="/var/lib/cider-press/log/postgres.log"

COPY efm-entrypoint.sh /usr/local/bin/cider-press-entrypoint
COPY efm-lb-entrypoint.sh /usr/local/bin/cider-press-lb-entrypoint
RUN chmod 0755 /usr/local/bin/cider-press-entrypoint /usr/local/bin/cider-press-lb-entrypoint

# Helpers every product's entrypoint shares; sourced, so not executable.
COPY lib/node-common.sh /usr/local/lib/cider-press/node-common.sh

# 5432 postgres · 8080 EFM primary health endpoint · 8404 HAProxy stats page
EXPOSE 5432 8080 8404

# The node entrypoint traps this and shuts down in order: the EFM agent first,
# then Postgres with a fast shutdown.
STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/cider-press-entrypoint"]
