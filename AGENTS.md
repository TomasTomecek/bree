# AGENTS.md

Conventions for agents (and people) changing this repo. What bree does and why
is in `docs/design.md`; this file says how to work on it.

## Design first

`docs/design.md` is the specification. A change of behaviour (a command, a flag,
a profile key, a file format) is a change of that document first, in the same PR
or an earlier one. Code that disagrees with the document is a bug in one of the
two: say which, do not quietly pick.

## Layout

- `src/main.rs`: entry point and exit statuses.
- `src/cli.rs`: the command tree of the design's "CLI" section, as `clap` derive
  types. A new command or flag goes here and into the design.
- `src/yaml.rs`: the strict YAML loader and its serde `Deserializer` (errors
  with file, line and key path).
- `src/config.rs`: the user config, the `Env` struct that injects the process
  and host, root and profile resolution.
- `src/profile.rs`: profile types, loading and validation.
- `src/check.rs`: `bree check`: compares a render with the disk and the
  manifest, and reads `capture.yaml` for the keys to ignore.
- `src/host.rs`: the host target of `bree render` and `bree check`: the state in
  `host.json` (or the import of the role's `deployed.json`), the one plan of
  what a render does to each file, the conflicts with their pending files, and
  the host's `check` rows.
- `src/role.rs`: the shared config repository's state file and hashing rule:
  reads `deployed.json`, the sha1 comparison, and `python_json`, which prints a
  value the way Python's `json.dumps(sort_keys=True)` does. It goes when no host
  needs the import any more.
- `src/contents.rs`: `bree profile contents`: the inventory of a render, with
  the blocks it leaves unused, as text or JSON.
- `src/render.rs`: block discovery across the roots, composition, templates,
  JSON merge patch, the manifest, and writing a render to a directory
  (`bree render`, container and `--out` semantics; `apply` and the rows are
  shared with the host target).
- `src/automode.rs`: the auto mode seed of Claude Code's container settings: the
  description of the container that `render` writes into `autoMode.environment`
  from the profile (each sentence must stay true of `src/run.rs`), and the rule
  that keeps it there after the settings blocks are merged.
- `src/tiers.rs`: the tiers manifest: reads `tiers.yaml` from the roots,
  validates it, and resolves a tier (with fallbacks) to a model id and display
  name.
- `src/run.rs`: builds the `podman run` invocation of a profile and runs it.
- `src/export.rs`: `bree export quadlet`: the quadlet `[Container]` section of
  a crew profile, from the same profile model as `bree run`.
- `src/image.rs`: `bree image build|status` and the build `bree run` does
  first: the embedded `image/` files, the labels and the decision to build.
- `src/entrypoint.rs`: `bree entrypoint`, the image's entry point: finds the
  roots under `/run/bree/roots/`, then execs the command after `main` rendered
  into the home.
- `src/error.rs`: the error type of the configuration and run code.
- `build.rs`: puts the commit into `bree --version` (`BREE_COMMIT` overrides it,
  for builds outside a git checkout).
- `tests/`: integration tests that run the built binary; `tests/fixtures/roots/`
  holds the config roots they read (one of them holds the design's `work` and
  `home` profiles, which the unit tests parse too).
- `image/`: the base image `bree-base` (design, "Base image"): `Containerfile`
  and the nested podman config in `containers/`. Its build context is this
  directory plus a `bree` executable; `.forgejo/workflows/image.yml` builds it,
  smoke tests it and pushes it from main.

Exit statuses follow the design: 0 success, 1 the command ran and found a
problem, 2 a usage or configuration error. Commands not implemented yet print
`bree: <command>: not implemented` and exit 2.

## Build, test, lint

Fedora's or CentOS Stream's `cargo`, `rustfmt` and `clippy` packages are all you
need; the crate builds with stable Rust from 1.85 (edition 2024).

```
cargo build
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI (`.forgejo/workflows/ci.yml`) runs the last three on every pull request; run
them before you push. `Cargo.lock` is committed, since bree is a binary: add a
dependency with `cargo add`, and commit the lock change with it.

Wrap prose in Markdown at 80 columns; `tests/check-markdown-wrap --fix FILE`
rewraps long lines (CI runs the check). Files listed in
`tests/markdown-wrap-todo` are skipped until they are rewrapped; a PR that
removes one must keep its words exactly as on the base branch. The checker is a
vendored copy of an upstream script, whose tests live upstream.

## Dependencies

Prefer a few well-known crates (`clap`, `serde`, `serde_json`) over hand-rolled
code, and keep the count small: every crate is something Fedora packaging has to
carry. Do not pick `serde_yaml`, which is unmaintained (design, "File formats").
