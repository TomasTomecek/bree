# Using bree

This page is for people and agents who want to run bree today. It says how to
install it, where it reads its configuration, how to write a first profile, and
what each command does, with one example per command. The full specification,
including the keys and options not shown here, is
[docs/design.md](design.md). For a guided first run, step by step with the
output to expect, follow [docs/tutorial.md](tutorial.md).

bree is early: some commands of the design exit with
`bree: <command>: not implemented` and status 2. The table below says which.

| Command | Status |
|---|---|
| `bree run` | works |
| `bree render` (also `--out`) | works |
| `bree check` | works |
| `bree profile contents` | works |
| `bree image build` | works |
| `bree image status` | works |
| `bree export quadlet` | works |
| `bree capture` | not implemented |
| `bree profile list` | not implemented |
| `bree profile show` | not implemented |
| `bree tiers` | not implemented |
| `bree image pull` | not implemented |
| `bree doctor` | not implemented |
| `bree completions` | not implemented |

## Install

Once the first release is out, install the RPM from Copr on Fedora or CentOS
Stream 10:

```
sudo dnf copr enable ttomecek/tools-stable
sudo dnf install bree
```

Until then, build it from a checkout. You need `cargo` (stable Rust 1.85 or
newer) and `git`:

```
git clone https://github.com/TomasTomecek/bree.git ~/src/bree
cd ~/src/bree
cargo build --release --locked
install -Dm0755 target/release/bree ~/.local/bin/bree
```

`cargo install --locked --path .` does the same in one step. `bree run` and
`bree image build` also need rootless `podman`.

Check the install with:

```
$ bree --version
bree 0.1.0 (e3c1e8ffbd3c)
```

The part in parentheses is the commit bree was built from.

## Config roots

bree holds no configuration of its own. It reads profiles and building blocks
from one or more config roots: directories laid out like this.

```
<root>/
  profiles/<name>.yaml
  rules/<name>.md
  skills/<name>/SKILL.md
  commands/<name>.md
  settings/<harness>/<name>.json
  files/<harness>/<name>/...
  tiers.yaml                         optional
```

bree finds the roots in this order, and the first one that applies wins:

1. `--root DIR` on the command line, repeatable. It replaces every other source.
2. `BREE_ROOTS`, a colon-separated list of directories.
3. `roots` in the user config `~/.config/bree/config.yaml` (another file with
   `--config FILE`).
4. Without any of these, `~/.config/bree/` alone.

A user config with two roots and a default profile:

```yaml
# Config roots, searched in order. A leading ~ is the home directory.
roots:
  - ~/.local/share/bree/roots/shared
  - ~/.config/bree/private
default-profile: home
```

Within the list, profiles and blocks are looked up by kind and name across all
roots. A name defined in two roots is an error, not an override. A root's name
in bree's output is its directory's base name, so `~/.config/bree/` shows up as
`bree`.

`-p NAME` picks the profile. Without it bree uses `default-profile` from the
user config, and fails when there is none.

## A minimal profile

This root has one rule, one skill and one profile, `home`, that starts Claude
Code with them. It lives in `~/.config/bree/`, so it needs no user config.

`~/.config/bree/rules/core.md`:

```
Answer in plain English. Run the tests before you say a change is done.
```

`~/.config/bree/skills/review-text/SKILL.md`:

```
---
name: review-text
description: Review a piece of prose for clarity and length.
---

Read the text, then list the three changes that would help a reader most.
```

`~/.config/bree/profiles/home.yaml`:

```yaml
description: "Home: personal rules"
harness: claude
rules: [core]
skills: [review-text]
mounts:
  - host: ~/src
```

`harness` is the agent `bree run` starts: `claude`, `pi` or `opencode`. The
`rules`, `skills`, `commands`, `settings` and `files` lists name the blocks the
profile uses, and nothing else is rendered. `mounts` are host paths the
container sees, at the same path by default. The design's "Profiles" section
lists every key, including the container options (network, nested podman, KVM),
secrets and tiers.

The examples below run as the user `alice` with this root, in
`~/src/project`.

## Commands

Every command takes the global options `-p`, `--root`, `--config`,
`--relative-to` and `-v`. Exit status is 0 on success, 1 when the command ran
and found a problem (a stale file, a missing image), and 2 on a usage or
configuration error.

### `bree render`

Renders the profile's blocks into the files each harness reads. By default it
writes into the profile's state directory, which `bree run` mounts as the
container's home. `--out DIR` writes the same tree under `DIR` instead, which is
the way to look at a render or test it in CI:

```
$ bree -p home render --out /tmp/bree-home
claude  /tmp/bree-home/.claude/CLAUDE.md                    installed
claude  /tmp/bree-home/.claude/settings.json                installed
claude  /tmp/bree-home/.claude/skills/review-text/SKILL.md  installed
bree: 3 written, 0 current, 0 removed
$ cat /tmp/bree-home/.claude/CLAUDE.md
Answer in plain English. Run the tests before you say a change is done.
```

`settings.json` holds the auto mode description of the container that bree
writes for Claude Code, followed by the profile's settings blocks.
`--target host` renders into the host's `~/.claude`, `~/.pi/agent` and
`~/.config/opencode` instead, and refuses to overwrite files edited by hand
unless you pass `--force` (design, "Host target").

### `bree check`

Renders in memory, compares with the files on disk and writes nothing. It prints
one line per file that is not current and exits 1 when there is one. It takes
the same `--target` and `--out` as `render`:

```
$ bree -p home check --out /tmp/bree-home
bree: 0 of 3 files not current
$ echo extra >> /tmp/bree-home/.claude/CLAUDE.md
$ bree -p home check --out /tmp/bree-home
claude  /tmp/bree-home/.claude/CLAUDE.md  modified  edited since bree rendered it
bree: 1 of 3 files not current
```

The second run exits 1. `--diff` adds a unified diff per file.

### `bree profile contents`

Lists what a render of the profile would contain, per harness, with the block
and root each piece comes from, and the blocks in the roots the profile leaves
unused. It writes nothing. `--json` gives the same for scripts:

```
$ bree -p home profile contents
claude  ~/.claude
  rules     CLAUDE.md      core  bree
  skills    review-text          bree
  settings  settings.json        auto mode (bree)
claude: 1 rule, 1 skill, 0 commands, 1 settings file, 0 files, 0 unused
```

### `bree image build` and `bree image status`

`bree run` runs the harness in the base image `bree-base`, which bree builds
locally from the Containerfile compiled into it. `image status` shows which
image the profile wants and whether the local one matches this bree. It exits 1
when the image is missing or its labels differ:

```
$ bree -p home image status
image   localhost/bree-base:0.1.0  (build)
local   missing
bree    version 0.1.0  containerfile 2579cee3220a  exe e796b0a9e061  commit e3c1e8ffbd3c
labels  no image
```

`image build` builds it, and `--no-cache` rebuilds from scratch for new Fedora
packages. `bree run` builds a missing or outdated image by itself, so you only
need this to build ahead of time:

```
$ bree -p home image build
bree: building localhost/bree-base:0.1.0
STEP 1/17: FROM fedora:45
...
Successfully tagged localhost/bree-base:0.1.0
$ bree -p home image status
image   localhost/bree-base:0.1.0  (build)
local   version 0.1.0  containerfile 2579cee3220a  exe e796b0a9e061  commit e3c1e8ffbd3c
bree    version 0.1.0  containerfile 2579cee3220a  exe e796b0a9e061  commit e3c1e8ffbd3c
labels  match
```

The build takes a minute or two and pulls `fedora:45`.

### `bree run`

Starts the profile's harness in its container, in the current directory. Name a
harness to start another one (`claude`, `pi`, `opencode`, or `shell` for bash),
and pass arguments to it after `--`:

```
$ cd ~/src/project
$ bree -p home run
$ bree -p home run claude -- --continue
```

Before the harness starts, the container renders the profile into its home and
says so on stderr. A shell shows what the harness sees:

```
$ bree -p home run shell -- -c 'cat ~/.claude/CLAUDE.md; ls ~/.claude/skills; pwd'
bree: rendered profile home: 3 written, 0 current, 0 removed
Answer in plain English. Run the tests before you say a change is done.
review-text
/home/alice/src/project
```

The container's home is the profile's state directory,
`~/.local/share/bree/state/home/`, mounted at the host home's path. The harness
keeps its login and sessions there between runs, so the first run asks you to
log in once. Your real home never reaches the container, only the profile's
mounts, the current directory, the host's gws login and its netrc do.
`--dry-run` prints the podman command without running it:

```
$ bree -p home run --dry-run
podman run --rm -it --init
  --name bree-home-project-9350
  --userns=keep-id:uid=1000,gid=1000
  --passwd-entry=alice:*:1000:1000::/home/alice:/bin/bash
  --security-opt=label=disable
  --tz=local
  --network=pasta
  -e TERM
  -e BREE_PROFILE=home
  -v /home/alice/.local/share/bree/state/home:/home/alice
  -v /home/alice/.config/bree:/run/bree/roots/0:ro
  -v bree-home-containers:/home/alice/.local/share/containers
  -v /home/alice/src:/home/alice/src:z
  -w /home/alice/src/project
  localhost/bree-base:0.1.0
  bree entrypoint -- claude
```

`-m HOST[:CONTAINER][:ro]` adds a mount for one run, and `--env NAME` passes a
host variable. The container renders the profile into its home on every start,
so edit the blocks in the root, not the rendered files.

### `bree export quadlet`

Prints the quadlet `[Container]` section of a crew profile: one that runs a
fixed `command` in its own image instead of a harness, for an agent that runs
as a systemd service. The design's "Crew profiles and quadlets" has the keys.
With this profile in `~/.config/bree/profiles/reviewer.yaml`:

```yaml
description: "Unattended reviewer service"
command: [review-loop, --interval, 1h]
image:
  ref: localhost/reviewer:latest
  source: local
state:
  home: image
container:
  name: reviewer
  workdir: /work
  nested-podman: false
mounts:
  - host: ~/src/project
    container: /work
    mode: ro
```

```
$ bree -p reviewer export quadlet
[Container]
Image=localhost/reviewer:latest
Pull=never
ContainerName=reviewer
Timezone=local
UserNS=keep-id
RunInit=true
LogDriver=passthrough
Network=pasta
WorkingDir=/work
Exec=review-loop --interval 1h
Volume=%h/src/project:/work:ro,z
PodmanArgs=--env=TERM
```

A `~` in a host path becomes `%h`, so systemd fills in the home of the user who
runs the unit. Put the section into your unit file next to its `[Unit]`,
`[Service]` and `[Install]` sections, which bree does not write.

A profile with a harness is refused with status 2.

### Not implemented yet

`bree capture`, `bree profile list`, `bree profile show`, `bree tiers`,
`bree image pull`, `bree doctor` and `bree completions` are specified in the
design but print `bree: <command>: not implemented` and exit 2:

```
$ bree -p home tiers
bree: tiers: not implemented
```
