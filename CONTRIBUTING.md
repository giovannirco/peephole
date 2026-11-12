# Contributing

- Recipes (YAML pages for a data source) and plugins are the most useful contributions. Open an issue first for a plugin so we agree on the keys.
- Install stable Rust with rustfmt and clippy. `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` must pass. Keep `Cargo.lock` in dependency changes so the checked graph can be reproduced.
- Tests live next to the code; network plugins are tested with `wiremock` fixtures. Use `example.invalid` for hosts.
- No credentials, personal hostnames or addresses in code, fixtures, screenshots or docs.
- Commits: conventional (`feat(plugins): …`, `fix(render): …`).
