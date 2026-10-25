# Contributing

- Recipes (YAML pages for a data source) and plugins are the most useful contributions. Open an issue first for a plugin so we agree on the keys.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass; CI runs them.
- Tests live next to the code; network plugins are tested with `wiremock` fixtures. Use `example.invalid` for hosts.
- No credentials, personal hostnames or addresses in code, fixtures, screenshots or docs.
- Commits: conventional (`feat(plugins): …`, `fix(render): …`).
