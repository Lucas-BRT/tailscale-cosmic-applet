# Repository Conventions

## Git Workflow

- Create `feat/<slug>` or `fix/<slug>` branches from `develop`.
- Open pull requests into `develop`, and promote `develop` into `main` via pull request. Do not push directly to `main`.
- Use Conventional Commits (`<type>(<scope>): <description>`) with small, focused commits.
- Enable the versioned Git hooks once per clone:

```sh
git config core.hooksPath .githooks
```

## Toolchain & Checks

- Toolchain is pinned in `rust-toolchain.toml` (`1.99.1`, `minimal` profile with `rustfmt` and `clippy`).
- Lints (`clippy::pedantic`) and build profiles are configured in `Cargo.toml`. Use `#[expect(..., reason = "...")]` at the smallest scope for justified exceptions.
- Local and CI checks must pass before pushing:

```sh
./scripts/check.sh
```

Individual scripts are in `scripts/` (`fmt.sh`, `lint.sh`, `test.sh`).

## UI Development

- Use `rupicola` (`https://github.com/Lucas-BRT/rupicola`) when planning, building, reviewing, or improving the COSMIC UI.
