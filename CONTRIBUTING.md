# Contributing

Contributions are welcome when they keep `vps` narrow, safe, and operable as a dependency-light control plane for disposable workers.

## Before making a change

- Open an issue before a large feature, behavior change, or architectural refactor so the scope can be agreed first.
- Keep changes focused. Avoid unrelated cleanup, new frameworks, or dependencies that are not necessary for the contribution.
- Never commit cloud tokens, SSH private keys, GitHub credentials, local `vps.toml` files, or instance state.
- Report security vulnerabilities through GitHub's private vulnerability reporting rather than a public issue when that facility is available.

## Development

Install the Rust version named in `rust-toolchain.toml`, including the `rustfmt` and `clippy` components. Before submitting a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
```

Do not invoke worker creation or destruction merely as a test: those commands can allocate billable infrastructure, mutate remote hosts, and revoke credentials.

## Pull requests

- Explain the problem, the chosen solution, and any operational or security impact.
- Add or update tests for behavior changes.
- Update `README.md` when command syntax, prerequisites, configuration, lifecycle behavior, security guarantees, or recovery steps change.
- Preserve resumability and conservative handling of uncertain cloud operations.
- Ensure every new source file includes these language-appropriate header lines:

  ```text
  SPDX-FileCopyrightText: 2026 Matt Curfman
  SPDX-License-Identifier: Apache-2.0
  ```

## Contribution license

This project is licensed under the Apache License, Version 2.0. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project is provided under that same license, without additional terms or conditions, as described in Section 5 of the license. By submitting a contribution, you represent that you have the right to license it on those terms.
