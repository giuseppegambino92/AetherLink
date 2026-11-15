# CI images

```sh
docker build -t aetherlink-linux -f ci/linux.Dockerfile ci
docker build -t aetherlink-wincross -f ci/windows-cross.Dockerfile ci

# Linux: check / clippy / test (registry cache shared with the host)
docker run --rm -v "$PWD":/work -v "$HOME/.cargo/registry":/usr/local/cargo/registry aetherlink-linux \
  cargo clippy --workspace --exclude aetherlink-desktop --all-targets -- -D warnings

# Windows cross-check
docker run --rm --platform linux/amd64 -v "$PWD":/work -v "$HOME/.cargo/registry":/usr/local/cargo/registry aetherlink-wincross \
  cargo clippy --target x86_64-pc-windows-gnu --workspace --exclude aetherlink-desktop --all-targets -- -D warnings
```
