# zjtabname — Agent Instructions

## Build

Default build target is `wasm32-wasip1` (configured in `.cargo/config.toml`).

```sh
mise run build          # or: cargo build --release
```

## Test

Tests must run on the **native host target**, not WASM:

```sh
mise run test           # or: cargo test --target $(rustc -vV | sed -n 's/host: //p')
```

Running bare `cargo test` will fail because the default target is `wasm32-wasip1`.

## Lint

```sh
cargo clippy --target $(rustc -vV | sed -n 's/host: //p') -- -D warnings
```

## Release

Releases use `cargo-release` + `git-cliff`. Mise tasks:

```sh
mise run changelog       # preview unreleased changelog
mise run release-patch   # bump patch version, update changelog, tag
mise run release-minor   # bump minor version, update changelog, tag
mise run release-major   # bump major version, update changelog, tag
```

After a release task completes, review the commit and tag, then push:

```sh
git push --follow-tags
```

Pushing a `v*` tag triggers the GitHub Actions release workflow which builds the WASM plugin and creates a GitHub Release.
