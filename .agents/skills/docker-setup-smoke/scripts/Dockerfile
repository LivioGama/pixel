# The smoke environment only: packages and the unprivileged test user.
# run.sh passes the pinned base image; fetching or building pixel and the
# checks stay in `docker run`, so a failed run still exports its evidence.
ARG BASE
FROM ${BASE}
RUN apt-get update -qq \
    && apt-get install -y -qq curl ca-certificates git python3 >/dev/null \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash tester \
    && install -d -o tester -g tester /evidence /home/tester/.local /home/tester/.local/bin
