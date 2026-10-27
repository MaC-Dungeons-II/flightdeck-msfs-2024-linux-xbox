# SPDX-License-Identifier: MIT
FROM ubuntu:24.04@sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential flex bison pkg-config gcc-mingw-w64-x86-64 \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    autoconf automake python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
