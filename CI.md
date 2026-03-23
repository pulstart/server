Server CI/CD mirrors the client release flow where the platform support is real:

- Linux x64
- macOS x64
- macOS arm64

The server now vendors `protocol` as a git submodule. Clone with:

```bash
git clone --recurse-submodules git@github.com:pulstart/server.git
```

If the repo is already cloned:

```bash
git submodule update --init --recursive
```

Release flow:

- Tag releases as `vX.Y.Z`
- GitHub Actions stamps that version into `Cargo.toml`
- Linux artifacts are published as `tar.gz`
- macOS artifacts are published as zipped `.app` bundles

The macOS packaging script supports optional Apple signing/notarization when the same
`MACOS_*` secrets used by the client workflow are configured. Without those secrets, the
workflow still produces unsigned macOS `.app` archives.

The server updater downloads the published release assets for the current platform and
replaces the installed package in place. On macOS, full package updates require running
the server from `st-server.app`.
