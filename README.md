![The village of Bree at a crossroads, travellers arriving](docs/bree.jpg)

# `bree`

Runtime for your local agents. With profiles and agents running in containers.

*Named after the village at the crossroads: every traveller gets a room, none
gets the keys to the town.*

bree runs a coding agent (Claude Code, pi or opencode) in a rootless podman
container set up from a profile, and renders the agent's configuration (rules,
skills, commands, settings) from named building blocks, for the container or for
the host.

```
bree -p work run claude          # Claude Code in the current directory, work setup
bree -p home run pi              # pi with the home setup
bree -p home render --out /tmp/home
bree -p home check --target host # exit 1 when rendered files are stale
```

## Why

- **The agent does not get your home.** The container's home is a state
  directory of the profile. Your real home never reaches the container: only
  the mounts the profile names and the current directory do.
- **One tool, several setups.** A profile says which blocks, which harness,
  which inference pool, and which mounts, secrets and network the container
  gets, so `work` and `home` are two profiles of the same tool, not two
  programs.
- **Configuration written once.** A rule, a skill or a command is one block,
  rendered into the format each harness reads: `CLAUDE.md` for Claude Code,
  `AGENTS.md` for pi and opencode, and so on.
- **Drift is visible.** `bree check` exits 1 when a rendered file no longer
  matches its blocks, in CI or on a host.
- **Secrets stay out of sight.** A profile names a secret's source and where it
  goes. The value never appears on the command line or in `--dry-run`.
- **Nothing personal inside.** bree holds no configuration of its own: profiles
  and blocks live in config roots of your own, for example a git repository you
  share between machines plus a private directory per machine.

## Quick start

bree needs Linux and rootless `podman`. Until the first release is out, build
it from a checkout with `cargo` (stable Rust 1.85 or newer):

```
git clone https://github.com/TomasTomecek/bree.git
cd bree
cargo install --locked --path .
```

A config root is a directory of profiles and blocks, by default
`~/.config/bree/`. One rule and one profile are enough:

```
mkdir -p ~/.config/bree/rules ~/.config/bree/profiles
echo 'Run the tests before you say a change is done.' > ~/.config/bree/rules/core.md
cat > ~/.config/bree/profiles/home.yaml <<'EOF'
description: "Home: personal rules"
harness: claude
rules: [core]
mounts:
  - host: ~/src
EOF
```

Then, in a project under `~/src`:

```
bree -p home run --dry-run   # print the podman command, run nothing
bree -p home run             # build the image on first use, start Claude Code
```

The first run builds the base image `bree-base` (Fedora with the three
harnesses and nested podman), which takes a minute or two, and the harness asks
you to log in once. The login and the sessions stay in the profile's state
directory, `~/.local/share/bree/state/home/`, between runs.

## Status

Early. `run`, `render`, `check`, `profile contents`, `image build`,
`image status` and `export quadlet` work; the other commands of the design exit
with "not implemented" for now ([docs/usage.md](docs/usage.md) has the list).
The runtime is containers only: VMs are in the design as a later step.

## Documentation

- [docs/usage.md](docs/usage.md): start here. Installing bree, config roots, a
  first profile, and every command with an example and whether it works yet.
- [docs/tutorial.md](docs/tutorial.md): a first profile in seven steps, written
  for an agent to run on a clean Fedora 44 or CentOS Stream 10 machine, with
  the output to expect at each step.
- [docs/design.md](docs/design.md): the specification. The CLI, the profile
  format, the building blocks, the renderer, the tiers manifest, secrets and the
  base image.
- [AGENTS.md](AGENTS.md): for contributors, people and agents. The code layout
  and how to build, test and lint.

## Inspiration

bree borrows ideas, not code or text, from two projects by [Colin
Walters](https://github.com/cgwalters):

- [cgwalters-forge/agenthome](https://github.com/cgwalters-forge/agenthome), a
  configuration repository for coding agents: one rules file rendered into each
  harness's format, base settings with overlays per run type, a `check` that
  fails on stale rendered files, and a manifest that maps prompt tiers to models
  per provider pool.
- [cgwalters-forge/agentic-job](https://github.com/cgwalters-forge/agentic-job),
  which runs agents as CI jobs and keeps provider keys off the agent's machine
  behind a credential broker. bree has a `broker` secret source for it.

agenthome has no license, so nothing from it is copied here.

## Building the RPM

`bree.spec` builds offline from the vendored crates, so mock needs no network
and no Fedora crate packages. You need `git`, `cargo`, `rpm-build` and `mock`.

```
make srpm   # rpm/: source tarball, vendor tarball and the SRPM
make rpm    # rebuild that SRPM in mock; MOCK_CONFIG=fedora-44-x86_64
sudo dnf install /var/lib/mock/fedora-44-x86_64/result/bree-*.x86_64.rpm
```

`make rpm` takes the mock config from `MOCK_CONFIG`, for example
`make rpm MOCK_CONFIG=fedora-45-x86_64`. The RPM installs `/usr/bin/bree`, the
`LICENSE` and this README.

## License

MIT, see [LICENSE](LICENSE).
