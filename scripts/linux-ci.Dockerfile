# The Linux verification image behind scripts/check-linux-docker.sh.
#
# rust:1.96-bookworm plus the system libraries the workspace links against
# (fontconfig/freetype/xkbcommon/wayland/xcb for Slint, openssl for reqwest,
# alsa for cpal, cmake+clang for whisper-rs-sys) and the two cargo components
# the check runs. Baking them in means a rerun costs a cargo invocation, not an
# apt-get; the layer is cached by docker build until this file changes.
#
# clang is the C compiler on purpose: the image's GCC 12 fails to build ggml's
# arm64 fp16 kernels ("target specific option mismatch"), clang does not.
FROM rust:1.96-bookworm
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      libfontconfig1-dev libfreetype6-dev libxkbcommon-dev libwayland-dev \
      libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev pkg-config cmake clang \
      libasound2-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt
ENV CARGO_TARGET_DIR=/target CARGO_NET_RETRY=5 CC=clang CXX=clang++ GGML_NATIVE=OFF
WORKDIR /src/rs
