# Contributing to avocadoctl

This guide is for anyone who wants to fix a bug or improve `avocadoctl`, the
Avocado Linux control CLI.

## Where to start

- Report bugs and propose changes in [GitHub issues](https://github.com/avocado-linux/avocadoctl/issues).
- Ask questions and talk to the team on [Discord](https://www.peridio.com/join-our-discord).
- Read `README.md` for what the tool does and `docs/` for the varlink API and
  feature notes.

## Build and test locally

You need a stable Rust toolchain with `rustfmt` and `clippy`. `Cargo.toml`
declares a minimum Rust version of 1.89.

The pull request checks run these commands. Run them before you open a pull
request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --verbose
cargo test --verbose
```

`tests/README.md` lists narrower test commands, such as `cargo test --lib` for
unit tests only.

The security check audits `Cargo.lock` against the RustSec advisory database.
To audit your dependencies locally, install `cargo-audit` and run `cargo audit`.

## Make a change

1. Create a topic branch from `main`.
2. Keep each pull request focused on one change.
3. Start each commit subject with the area you changed, then a short summary,
   for example `os_update: Resolve slot partitions on the booted disk` or
   `deps: bump zstd to 0.14 for RUSTSEC zstd-safe advisories`.
4. Open the pull request against `main`.

Pull requests are rebase-merged, so every commit lands on `main` as you wrote
it.

`CHANGELOG.md` follows Keep a Changelog. You may add an entry under
`## [Unreleased]`. The release pull request writes any entries a change did
not add, so a missing entry does not block your change.

## Pull request checks

Every pull request against `main` runs two checks:

- **Run Tests**: formatting, Clippy with warnings as errors, build, and the
  full test suite on `ubuntu-latest` with stable Rust.
- **Security Audit**: the RustSec `audit-check` action over the dependency tree.

## Report a security issue

Do not report security vulnerabilities in public issues, pull requests, or
Discord. Follow the
[Avocado security policy](https://github.com/avocado-linux/avocado-cli/blob/main/SECURITY.md),
which covers every repository in the `avocado-linux` organization.

## License

`avocadoctl` is licensed under the Apache License 2.0 (see `LICENSE`). Under
section 5 of that license, any contribution you intentionally submit for
inclusion is under the same license terms, unless you explicitly state
otherwise.
