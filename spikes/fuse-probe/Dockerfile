FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq fuse3 >/dev/null && rm -rf /var/lib/apt/lists/*
# Enabling allow_other for a non-root mounter is a host-level privileged action.
# Doing it here lets the probe measure what it buys without needing host root.
RUN echo user_allow_other >> /etc/fuse.conf
RUN useradd -u 4242 -m agent
