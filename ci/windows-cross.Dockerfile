# Cross-compile (check/clippy/build) for x86_64-pc-windows-gnu; wine64 runs pure-logic test binaries.
FROM --platform=linux/amd64 rust:1-trixie
RUN dpkg --add-architecture i386 && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      mingw-w64 cmake clang pkg-config wine wine64 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-pc-windows-gnu && rustup component add clippy
ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine \
    CARGO_TARGET_DIR=/work/target/docker-windows
WORKDIR /work
