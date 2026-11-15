# Native Linux build/test image (arm64 on Apple Silicon hosts, amd64 elsewhere).
FROM rust:1-trixie
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      pkg-config clang libclang-dev cmake \
      libpipewire-0.3-dev libspa-0.2-dev libdbus-1-dev libudev-dev libfuse3-dev fuse3 \
      libx11-dev libxext-dev libxtst-dev libxrandr-dev libxfixes-dev libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev libxcb-xtest0-dev \
      libasound2-dev libpulse-dev libbluetooth-dev libopus-dev \
      xvfb xauth x11-utils dbus dbus-x11 at-spi2-core libatspi2.0-dev gtk-3-examples \
      pipewire pipewire-pulse wireplumber pulseaudio-utils nfs-common \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# OCR fallback backend (aether-core ocr: tesseract CLI, exercised by an #[ignore]d test).
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends tesseract-ocr tesseract-ocr-eng \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
ENV CARGO_TARGET_DIR=/work/target/docker-linux
WORKDIR /work
