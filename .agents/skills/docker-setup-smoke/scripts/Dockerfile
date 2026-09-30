# The smoke environment only: packages and the unprivileged test user.
# run.sh passes the pinned base image; fetching or building pixel and the
# checks stay in `docker run`, so a failed run still exports its evidence.
ARG BASE
FROM ${BASE}
# homebrew/brew defaults to its linuxbrew user; the bootstrap runs as root.
USER root
# homebrew/brew adds a third-party apt source whose key rotation breaks
# `apt-get update`; run.sh points this at a missing directory for that base.
ARG APT_SOURCE_PARTS=/etc/apt/sources.list.d/
RUN apt-get -o "Dir::Etc::SourceParts=$APT_SOURCE_PARTS" update -qq \
    && apt-get -o "Dir::Etc::SourceParts=$APT_SOURCE_PARTS" install -y -qq \
        curl ca-certificates git python3 >/dev/null \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -U -s /bin/bash tester \
    && install -d -o tester -g tester /evidence /home/tester/.local /home/tester/.local/bin
