# The image behind scripts/check-windows-remote.sh: scripts/check-windows.sh on a
# Linux build host, so the windows-gnu target dir stays off the Mac.
#
# trixie, not bookworm or the hosts' Ubuntu 22.04: whisper.cpp's ggml-cpu.c uses
# THREAD_POWER_THROTTLING_STATE, which mingw-w64 8 (Ubuntu 22.04) lacks.
FROM rust:1.99-trixie
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy \
    && rustup target add x86_64-pc-windows-gnu
ENV CARGO_TARGET_DIR=/target CARGO_NET_RETRY=5
WORKDIR /src
