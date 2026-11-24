# bree design

Status: agreed design. Sections are implemented one at a time, and a change of
design is a change of this file first.

## What bree does

bree runs a coding agent (Claude Code, pi or opencode) in a rootless podman
container, set up from a profile, and renders the agent's configuration from
building blocks the profile names. It replaces two things that tend to grow
separately: a shell wrapper with one hardcoded work setup, and a Makefile recipe
for a home setup.

It does four jobs, and each one is a command:

- `bree run` turns a profile into one `podman run`: image, mounts, secrets,
  network, nested podman, and the harness to start.
- `bree render` puts together the blocks a profile names (rules, skills,
  commands, settings, plain files) into the files each harness reads, in the
  container or on the host. `bree run` renders the container side by itself at
  every start (through `bree entrypoint`), so nobody calls `render` before
  `run`. You call `render` for the host (`--target host`, what the Ansible role
  that deploys your configuration does; "the role" below) or to write the result
  somewhere to read (`--out DIR`).
- `bree check` tells whether rendered files are stale, so CI and the Ansible
  role can fail on drift.
- `bree capture` goes the other way: it takes what a harness changed in its
  rendered files at runtime (a setting changed with `/config` or `/model`, a
  skill edited in a session) and writes it back into the block it came from, for
  you to review and commit.

bree holds no personal configuration. Profiles and blocks live in a separate
configuration tree, for example a shared config repository that the role deploys
to your hosts. Anyone can point bree at their own tree.

## Terms

| Term | Meaning |
|---|---|
| harness | A coding agent CLI: `claude` (Claude Code), `pi`, `opencode`. |
| config root | A directory with `profiles/` and the blocks (`rules/`, `skills/`, `commands/`, `settings/`, `files/`), and optionally `tiers.yaml` and `capture.yaml`. bree reads one or more roots. |
| block | One named piece of configuration in a root: a rules fragment, a skill, a command, a settings patch for one harness, or a bundle of plain files for one harness. |
| profile | A YAML file in `profiles/` that names its blocks explicitly, the harness, the inference pool, and how the container is set up. `work` and `home` are profiles. |
| tier | A model size a prompt or setting names (`big`, `medium`, `small`) instead of a model. |
| pool | One inference provider's mapping from tiers to models (`claude`, `corp`, `llama`). |
| target | Where rendered files go: `container` (the profile's state directory, mounted into the container) or `host` (`~/.claude`, `~/.pi/agent`, `~/.config/opencode`). |
| state directory | The profile's persistent home inside the container, on the host under `~/.local/share/bree/state/<profile>/`. |

## CLI

The binary is `bree`. Every command takes the global options below; exit status
is 0 on success, 1 when the command ran and found a problem (a stale file, a
failed check), and 2 on a usage or configuration error.

Global options:

| Option | Default | Meaning |
|---|---|---|
| `-p`, `--profile NAME` | `default-profile` from the user config, else an error | Profile to use. |
| `--root DIR` (repeatable) | `roots` from the user config | Config roots, in order. Replaces the configured list when given. `BREE_ROOTS` (colon-separated) does the same. |
| `--relative-to DIR` (at most once) | none | Directory that relative host paths in mounts resolve against, whichever root the profile or block came from (see "Crew profiles and quadlets"). Without it a relative host path is an error. |
| `--config FILE` | `~/.config/bree/config.yaml` | User config file. |
| `-v`, `--verbose` | off | Print what bree does to stderr: resolved blocks, the podman command. |
| `--version` | | Print `bree X.Y.Z (commit)` and exit. |

A profile is a flag and not a positional argument, so every command reads the
same way and `bree run claude` works with the default profile.

### `bree run [HARNESS] [-- ARGS...]`

Starts `HARNESS` (`claude`, `pi`, `opencode` or `shell` for bash) in the
profile's container, in the current directory. Without `HARNESS` it starts the
profile's `harness`. `ARGS` go to the harness. A profile's
`harness-command.<harness>` replaces the argv bree starts; `ARGS` are appended
to it, so `[opencode, mini]` plus `-- --continue` runs
`opencode mini --continue`.

The container is configured by the profile, not hardcoded. The `image`,
`container` (network, nested podman, KVM, timezone, working directory,
environment), `mounts`, `state` and `secrets` keys (see "Profiles") decide every
podman option that differs between setups, and the options below add to them for
one run. What bree fixes for every profile is what the design rests on: the
state directory as the container home at the host home's path, the config roots
read-only, the uid mapping, `--init`, and the entry point. Crew profiles (see
"Crew profiles and quadlets") are the one exception: `state.home: image` keeps
the image's home instead of a state directory, and `command` replaces the entry
point. `--dry-run` shows the exact command a profile gives.

| Option | Meaning |
|---|---|
| `-m`, `--mount SPEC` (repeatable) | Extra bind mount for this run, `HOST[:CONTAINER][:ro]`; a bare host path is mounted at the same path. |
| `-n`, `--dry-run` | Print the `podman run` command, one argument per line, and exit without running it. |
| `--no-pull` | Never pull or build the image; fail when it is missing. |
| `--env NAME` (repeatable) | Pass a host environment variable by name, in addition to the profile's. |

```
$ cd ~/src/shared
$ bree -p work run claude -- --continue
```

```
$ bree -p work run --dry-run claude
podman run --rm -it --init
  --name bree-work-shared-5f2c
  --userns=keep-id:uid=1000,gid=1000
  --passwd-entry=alice:*:1000:1000::/home/alice:/bin/bash
  --security-opt=label=disable
  --tz=local
  --network=pasta
  -e TERM
  -e BREE_PROFILE=work
  -e CORP_KEY
  -e ANTHROPIC_API_KEY
  -v /home/alice/.local/share/bree/state/work:/home/alice
  -v /home/alice/.local/share/bree/roots/shared:/run/bree/roots/0:ro
  -v /home/alice/.config/bree/private:/run/bree/roots/1:ro
  -v bree-work-containers:/home/alice/.local/share/containers
  -v /home/alice/.gitconfig:/home/alice/.gitconfig:ro,z
  -v /home/alice/src:/home/alice/src:z
  -w /home/alice/src/shared
  localhost/bree-base:0.1.0
  bree entrypoint -- claude
```

The four hex digits of the container name are random per run, so two runs in the
same directory do not clash.

The `home` profile, with KVM and no secrets:

```
$ cd ~/src/project
$ bree -p home run --dry-run claude
podman run --rm -it --init
  --name bree-home-project-5f2c
  --userns=keep-id:uid=1000,gid=1000
  --passwd-entry=alice:*:1000:1000::/home/alice:/bin/bash
  --security-opt=label=disable
  --device=/dev/kvm
  --security-opt=unmask=ALL
  --tz=local
  --network=pasta
  -e TERM
  -e BREE_PROFILE=home
  -v /home/alice/.local/share/bree/state/home:/home/alice
  -v /home/alice/.local/share/bree/roots/shared:/run/bree/roots/0:ro
  -v /home/alice/.config/bree/private:/run/bree/roots/1:ro
  -v bree-home-containers:/home/alice/.local/share/containers
  -v /home/alice/.gitconfig:/home/alice/.gitconfig:ro,z
  -v /home/alice/src:/home/alice/src:z
  -w /home/alice/src/project
  localhost/bree-base:0.1.0
  bree entrypoint -- claude
```

The secret values never appear in that command: bree sets them in podman's
environment and passes them by name (`-e NAME`).

`run` refuses a working directory or mount that is `/`, the host home, or a
parent of it, because the state directory is mounted as the container's home at
the host home's path and the host home must never reach the container. It also
refuses when the working directory is not under one of the profile's mounts and
`container.workdir: mount` is set (below).

### `bree render [--target container|host] [--out DIR] [--add KIND=NAME]... [--var NAME=VALUE]... [--harness NAME]... [--force]`

Renders the profile's blocks and writes the files. `--target container` (the
default) writes into the profile's state directory, `--target host` into the
host's harness directories. `--out DIR` writes the same tree under `DIR`
instead, for tests and CI; the target still decides what is in the tree, so the
`--out` tree of the container target is what the container gets, and the host
target never gets the auto mode seed of Claude Code's settings (below, "What the
renderer writes"). `--add` appends a block to one of the profile's lists (`KIND`
is `rules`, `skills`, `commands`, `settings.<harness>` or `files.<harness>`),
and `--var` sets a template variable over the profile's `vars`; the role uses
both for per-host values (below, "Templates"). `--harness` limits rendering to
some harnesses; the default is the profile's `harnesses`.

```
$ bree -p home render --target host --add settings.claude=host-a
claude  ~/.claude/CLAUDE.md                          updated
claude  ~/.claude/settings.json                      current
claude  ~/.claude/skills/review-text/SKILL.md        installed
pi      ~/.pi/agent/AGENTS.md                        updated
pi      ~/.pi/agent/APPEND_SYSTEM.md                 current
bree: 3 written, 2 current, 0 removed
```

On the host target a file edited by hand since bree wrote it is a conflict, and
so is a file bree never wrote that differs from the render. bree then writes
nothing at all, saves its version of each conflicting file under
`~/.local/state/bree/pending/`, prints a `diff -u` command per file and exits 1,
which is the role's managed-copy rule; "Host target" below has the details.
`--force` overwrites the conflicts. On the container target, and with `--out`,
there is no conflict check. There `--force` only drops the carried settings
patches of the container target ("Container target" below), and with `--out` it
changes nothing.

### `bree check [--target container|host] [--out DIR] [--add KIND=NAME]... [--var NAME=VALUE]... [--diff]`

Renders in memory and compares with the files on disk, writing nothing. It
prints one line per file that is not current and exits 1 if there is one.
`--diff` adds a unified diff per file.

```
$ bree -p home check --target host --add settings.claude=host-a
claude  ~/.claude/CLAUDE.md          stale      render differs, +4 -1 lines
claude  ~/.claude/settings.json      modified   edited since bree rendered it
pi      ~/.pi/agent/prompts/old.md   extra      no block produces it any more
bree: 3 of 14 files not current
```

The states are `current`, `stale` (rendering now gives different content; the
line counts come from the diff, since bree keeps no source hashes and cannot
tell which block caused it), `missing`, `modified` (the file differs from what
bree last wrote, or on the host target bree never wrote it, so a render would be
a conflict) and `extra` (bree wrote it earlier and no block of the profile
produces it now, so a render would remove it). In the shared config repository's
CI, `bree check --out <dir>` against a committed fixture is how the tree is
tested.

### `bree capture [--target container|host] [--into NAME] [--add PATH]... [--to-root DIR] [--dry-run]`

Brings runtime changes back into the sources. Harnesses write their own
configuration: Claude Code saves some settings to `settings.json`, pi saves the
chosen model to its `settings.json`, and a session may edit a skill or a rules
file in place. `capture` finds every rendered file that changed since bree wrote
it (the `modified` files of `check`, plus the settings patches the container
entry point carries and the other files it saved aside, below), works out the
change against what bree wrote (for a carried patch or a saved-aside copy, the
base saved with it), and applies it to the source file in a config root. It
never commits: the roots are your directories, usually git checkouts, so
`git diff` in the root is the review and `git commit` the decision. `--dry-run`
prints the changes as diffs of the source files and writes nothing.

```
$ bree -p home capture --dry-run
claude  settings.json                 patch    settings/claude/home.json  +1 key (model)
claude  skills/review-text/SKILL.md   file     skills/review-text/SKILL.md
claude  CLAUDE.md                     hunk     rules/core.md:41
claude  skills/scratch/               new      not captured, use --add skills/scratch
pi      settings.json                 ignored  defaultModel (capture.yaml)
bree: 3 to capture, 1 ignored, 1 new
```

How each kind of file goes back:

| Rendered file | How it is captured |
|---|---|
| settings (`settings.json`, `opencode.json`) | The difference between what bree wrote and the file now, as a JSON Merge Patch, merged into the last settings block the profile lists for that harness, or into the block `--into NAME` names, which must be in that list. Keys the renderer set itself are skipped: the tier model env vars, `skipAutoPermissionPrompt`, and bree's auto mode entry in `autoMode.environment` together with the `"$defaults"` it seeded, so that what bree says about the container never lands in a block. A key whose value in a source block holds a placeholder is refused and printed, for the same reason as below. A `.jsonc` source keeps its comments only where the patch does not touch them; when that is not possible, `capture` refuses that file and prints the patch. |
| plain files (`files/`) | Copied back over the file of the block the manifest says produced it, since plain files are never templated. |
| skills, commands | Copied back as for plain files only when the source has no placeholder. Otherwise, as for rules, each diff hunk is applied to the source file, and a hunk that touches a line with a placeholder (`{{tier.X}}`, `{{var.X}}`) is refused and printed, so a rendered model id or variable value never replaces the placeholder in the source. Placeholders render within their line, so the other lines map one to one. |
| rules (`CLAUDE.md`, `AGENTS.md`) | Each diff hunk is mapped to the rules block whose lines it falls in (the manifest records each block's line range) and applied there. A hunk that spans two blocks, or touches a line with a placeholder such as `{{tier.big}}`, cannot be reversed safely: `capture` refuses it and prints it with the block names. |
| files the harness created (a new skill, a new command) | Not captured by default, since harnesses also create files nobody wants to keep. `--add PATH` adopts one as a new block in the root `--to-root DIR` names, which must be one of the configured roots (default: the root that holds the profile). A profile with `skills: all` renders it from the next start; otherwise `capture` prints the line to add to the profile, and never edits the profile itself. |

A root can list keys that are runtime churn, not configuration, in
`capture.yaml` under `ignore`, as JSON Pointers per harness
(`claude: [/feedbackSurveyState]`); the lists of all roots add up. `check` does
not report a file as `modified` for them and `capture` skips them. Which keys
each harness churns is found out from the pinned versions.

`capture` runs on the host, because the roots are mounted read-only into the
container on purpose: an agent in a session cannot rewrite its own rules.
`--target container` (the default) reads the profile's state directory, the
carried settings patches and the saved-aside copies, `--target host` the host's
harness directories. When the blocks changed since the last render, `capture`
merges three ways (what bree wrote, the file now, a fresh render) and refuses a
file where your change and the block change touch the same key or lines, instead
of guessing.

### `bree profile list` and `bree profile show [--json]`

`list` prints every profile in the roots with its description and root. `show`
prints the profile after resolving the roots: its block lists with the root each
block comes from, harnesses, pool with the model each tier resolves to, mounts,
and secrets with their source and whether the source is available now, never
their value.

```
$ bree -p work profile show
profile   work  (~/.local/share/bree/roots/shared/profiles/work.yaml)
roots     shared    ~/.local/share/bree/roots/shared
          private   ~/.config/bree/private
rules     core  claude:claude-code  work  (shared)   work-internal  (private, optional)
skills    all  (3 in shared, 1 in private)
commands  review-text  (shared)
settings  claude: work  (shared)   work-internal  (private, optional)
          opencode: work  (shared)
files     claude: statusline   pi: base  work  (shared)
harness   claude  (renders claude, pi, opencode)
pool      corp  big=example/big-model  medium=example/big-model  small=example/small-model
image     localhost/bree-base:0.1.0  (build)
network   pasta
mounts    ~/src -> ~/src  rw
          ~/.gitconfig -> ~/.gitconfig  ro, optional
secrets   corp  env CORP_KEY  available  -> CORP_KEY; claude: ANTHROPIC_API_KEY; pi: OPENAI_API_KEY ANTHROPIC_API_KEY
```

### `bree profile contents [--harness NAME]... [--add KIND=NAME]... [--json]`

Answers "does this profile have everything": the final configuration a render
would produce, as an inventory instead of file contents. It renders in memory,
writes nothing, and lists per harness every rules block, skill, command,
settings file and plain file, with the block and root each one comes from. Rules
show the blocks in the order they are concatenated into `CLAUDE.md` or
`AGENTS.md`; settings show the blocks patched in, in order, after the seeds the
renderer sets itself, `auto mode (bree)` and `tiers.yaml`, where they apply (see
"What the renderer writes"). It renders as the container target. It ends with
the blocks in the roots that the profile does not use, so a skill or command you
forgot to list shows up there.

```
$ bree -p work profile contents --harness claude
claude  ~/.claude
  rules     CLAUDE.md  core                  shared
                       claude-code           shared
                       work                  shared
                       work-internal         private
  skills    example-skill                    shared
            review-text                      shared
            update-dependencies              shared
            internal-deploy                  private
  commands  review-text.md                   shared
  settings  settings.json                    auto mode (bree) < tiers.yaml < work < work-internal
  files     statusline.sh                    statusline (shared)
  unused    commands/release-notes           shared
claude: 4 rules, 4 skills, 1 command, 1 settings file, 1 file, 1 unused
```

It shares the renderer's merge. `--json` gives the same for scripts, for example
a CI test in the shared config repository that no profile leaves a skill unused.
To read the final text itself, `bree -p work render --out /tmp/work` writes the
whole tree.

### `bree tiers [--pool NAME]`

Prints the tiers manifest after merging the roots: each pool, each tier, its
model per harness and its fallback.

### `bree image build|pull|status`

`build` builds the base image locally from the Containerfile compiled into the
binary (below, "Base image"); `--no-cache` rebuilds from scratch for new Fedora
packages (it also pulls the base again). The image is tagged with the profile's
`image.ref`. `pull` pulls the profile's pinned image. `status` prints the image
reference the profile wants, what is present locally, and whether the image's
labels (version, Containerfile hash, the hash of the `bree` executable and its
commit) match this binary's; it exits 1 when the image is missing or they
differ. `bree run` builds a missing image, or one whose
labels differ, itself unless `--no-pull` is given; it pulls nothing until
`pull` is implemented.

```
$ bree -p work image status
image   localhost/bree-base:0.1.0  (build)
local   version 0.1.0  containerfile 5f2c9a1b7d40  exe 9e41c07a3b18  commit 0f56edf2a1c4
bree    version 0.1.0  containerfile 5f2c9a1b7d40  exe 9e41c07a3b18  commit 0f56edf2a1c4
labels  match
```

### `bree export quadlet`

Prints the quadlet `[Container]` section of a crew profile on stdout and exits
0, or fails with status 2 on a profile without `command` and `state.home:
image`. What it writes, and how that differs from `bree run`, is in "Crew
profiles and quadlets".

### `bree doctor`

Checks the environment without changing it: podman version and rootless setup,
subuid ranges, `/dev/kvm` when a profile asks for it, every root readable, every
profile valid, every secret source available. One line per check, exit 1 if one
fails.

### `bree entrypoint [-- COMMAND...]` (inside the container)

The base image's entry point. It renders the profile named by `BREE_PROFILE`
from the roots mounted under `/run/bree/roots/` into the container home, then
`exec`s the command. Users do not run it; it is in `--help` under "internal
commands".

The roots are the entries of `/run/bree/roots/` in numeric order, each named by
its number as `run` mounts them; any other entry there is an error. `--root`
replaces them, for tests; the user config and `BREE_ROOTS` are not read, since
the host's paths mean nothing in the container. `-p` wins over `BREE_PROFILE`.
It renders every harness of the profile into `$HOME`, the same write as
`render --target container` on the host, prints one summary line on stderr (the
file list too with `-v`), and sets `CLAUDE_CONFIG_DIR` to `~/.claude` for the
command unless it is already set. A render error exits 2 and starts nothing.
Without a command it only renders. Until the carry-over of "Container target" is
implemented, it overwrites every rendered file a session changed, settings
included, without saving it aside.

### `bree completions bash|zsh|fish`

Prints a shell completion script.

## User config

`~/.config/bree/config.yaml` is optional and small:

```yaml
# Config roots, searched in order. A leading ~ is the home directory.
roots:
  - ~/.local/share/bree/roots/shared
  - ~/.config/bree/private
default-profile: home
```

Without the file, the roots default to `~/.config/bree/` alone. The role writes
this file on hosts it manages.

## File formats

bree's own files (`config.yaml`, profiles, `tiers.yaml`, `capture.yaml`) are
YAML, and harness settings stay JSON. YAML has its own traps, so bree reads a
strict subset and fails loudly on the rest:

- YAML 1.2 core schema: `no`, `on`, `off` and `yes` are strings, not booleans
  (the 1.1 "Norway problem"), and only `true` and `false` are booleans.
- Every file is read into a typed structure: a key that expects a string gets a
  string, so `name: 5.3` is the text `5.3`, and a key that expects a number or
  boolean rejects anything else, with the file, line and key in the message.
- Unknown keys and duplicate keys are errors.
- One document per file; anchors, aliases, merge keys (`<<`) and tags are
  rejected, so what a file says is what it means without following references.

`bree profile show` prints the profile as bree parsed it, so a value YAML read
differently from what was meant shows there. bree reads YAML with yaml-rust2's
event parser (a YAML 1.2 parser that Fedora packages) and its own strict loader
on top, with a test per rule above.

## Config roots and resolution

A config root is a directory:

```
<root>/
  profiles/<name>.yaml
  rules/<name>.md                    blocks (below, "Building blocks")
  skills/<name>/SKILL.md
  commands/<name>.md
  settings/<harness>/<name>.json
  files/<harness>/<name>/...
  tiers.yaml                         optional
  capture.yaml                       optional
```

A root's name, as `profile show` and `profile contents` print it, is its
directory's base name (`shared`, `private`). Profiles and blocks are found by
kind and name across all roots. A name defined in two roots is an error, not a
silent override, so a private root cannot shadow a public block by accident. To
use different content, a profile names a different block. This is how work-only
values stay out of the public tree: the shared config repository holds the
shared blocks and the `home` and `work` profiles, and a private root on the work
machine holds blocks such as `rules/work-internal.md` with internal plugin names
and endpoints that must not be published. The public `work` profile lists them
under `optional`, so it renders in the shared config repository's CI without the
private root.

## Profiles

A profile is YAML (see "File formats" for the dialect bree accepts). Every key
and its default:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `description` | string | `""` | One line for `profile list`. |
| `harness` | `claude`, `pi`, `opencode` | required, unless `command` is set | Harness `bree run` starts without an argument. A crew profile sets `command` instead (see "Crew profiles and quadlets"). |
| `harnesses` | list | `[harness]` | Harnesses whose configuration is rendered. |
| `harness-command.<harness>` | list of strings | the harness name | The argv bree starts for one harness; an empty list is the default. |
| `rules` | list of block names | `[]` | Rules blocks in the order they are concatenated. |
| `skills` | `all` or list of block names | `[]` | Skills; `all` is every skill in the roots. |
| `commands` | `all` or list of block names | `[]` | Commands; `all` is every command in the roots. |
| `settings.<harness>` | list of block names | `[]` | Settings patches for one harness, applied in order. |
| `files.<harness>` | list of block names | `[]` | Bundles of plain files for one harness. |
| `vars` | map of strings | `{}` | Values for `{{var.NAME}}` (below, "Templates"). |
| `optional` | list of block names | `[]` | Blocks the lists may name that are skipped when no root has them (below, "Composition rules"). |
| `pool` | pool name | required when a block uses tiers | Inference pool for tier substitution. |
| `image.ref` | string | `localhost/bree-base:<bree version>` | Image to run. |
| `image.source` | `build`, `pull` | `build` | `build`: build locally from the embedded Containerfile. `pull`: pull `ref`, which must then be pinned by digest or a version tag. |
| `container.network` | `pasta`, `host`, `none` | `pasta` | podman network mode. |
| `container.nested-podman` | bool | `true` | `label=disable` plus a per-profile storage volume `bree-<profile>-containers`. |
| `container.label-disable` | bool | `false` | `label=disable` without the nested storage volume that `nested-podman: true` adds. |
| `container.kvm` | bool | `false` | Pass `/dev/kvm` (and `unmask=ALL`) for VM tests inside. |
| `container.workdir` | `cwd`, `mount` | `cwd` | `cwd` mounts the current directory at the same path. `mount` requires it to be inside a configured mount and adds no mount of its own. |
| `container.tz` | `local`, a zone name | `local` | `--tz`; `local` is skipped on a host without `/etc/localtime`. |
| `container.env` | map of strings | `{}` | Plain, non-secret environment variables for the container. |
| `container.pass-env` | list of names | `[TERM]` | Host variables passed by name. |
| `state.dir` | path | `~/.local/share/bree/state/<profile>` | The container home (below). |
| `mounts` | list of maps | `[]` | Bind mounts, below. |
| `secrets.<name>` | maps | `{}` | Secrets, see "Secrets". |

A `mounts` entry has `host` (path, `~` is the host home), `container` (default:
the same path, with `~` meaning the host home path too), `mode` (`rw` or `ro`,
default `rw`) and `optional` (default `false`: a missing host path is an error).
It also takes `volume` instead of `host` and `relabel`, and a relative `host`
with `--relative-to` (see "Crew profiles and quadlets").

`harness-command` takes a list, never a bare string: `opencode mini` is
`[opencode, mini]`, and a string would make it one argv element. The list
replaces the whole argv, so it can name another binary, not only add flags. The
harness is still the same one: it decides which `settings`, `files` and secret
`harness-env` a render uses. `shell` is a `bree run` argument, not a harness, so
it has no entry and still starts `bash`.

Unknown keys are an error, so a typo fails `bree profile show` and CI instead of
being ignored.

The `work` profile of the examples above:

```yaml
description: "Work: corporate models, work rules, no personal config"
harness: claude
harnesses: [claude, pi, opencode]
pool: corp

rules: [core, claude:claude-code, work, work-internal]
skills: all
commands: [review-text]
settings:
  claude: [work, work-internal]
  opencode: [work]
files:
  claude: [statusline]
  pi: [base, work]
# only on the work machine, in the private root
optional: [work-internal]

container:
  network: pasta
  nested-podman: true

mounts:
  - host: ~/src
  - host: ~/.gitconfig
    mode: ro
    optional: true

secrets:
  corp:
    source: env
    var: CORP_KEY
    env: [CORP_KEY]
    required: false
    harness-env:
      claude: [ANTHROPIC_API_KEY]
      pi: [OPENAI_API_KEY, ANTHROPIC_API_KEY]
```

The `home` profile keeps the Claude login in the profile's state directory
instead of the host's `~/.claude`:

```yaml
description: "Home: Claude subscription, personal rules"
harness: claude
harnesses: [claude, pi]
pool: claude

rules: [core, claude:claude-code, home]
skills: all
commands: all
settings:
  claude: [home]
files:
  claude: [statusline]
  pi: [base]

container:
  network: pasta
  nested-podman: true
  kvm: true

mounts:
  - host: ~/src
  - host: ~/.gitconfig
    mode: ro
    optional: true
```

A container started by hand often mounts the host's `~/.claude` and
`~/.claude.json`, so the session shares the host's login, history and settings.
With bree the container has its own home, so the first `bree -p home run` asks
for `/login` once and keeps it in the state directory. That is deliberate: the
host's Claude config is no longer writable from a container. A profile that
wants the shared login back can mount `~/.claude` explicitly, at the cost of the
container writing the host's files.

## The container

The container's home is the profile's state directory, mounted at the host
home's path (for the user alice, `/home/alice`), so project paths under the home
are the same inside and out, and sessions keyed by path stay stable. The
harnesses keep their state there between runs: Claude Code's `.claude.json` and
sessions, pi's sessions, and opencode's `~/.local/share/opencode`, which a
throwaway container loses at every exit.

The user inside has uid 1000 (`--userns=keep-id:uid=1000,gid=1000`), so files it
writes in mounts belong to the host user whatever the host uid is. bree passes
`--passwd-entry` with the host user's name and home path, so the image does not
name a user. The image's `/etc/subuid` and `/etc/subgid` give uid 1000 the
nested ranges by number (`1000:1:999`, `1000:1001:64535`), which subuid(5)
allows and which keeps nested podman working for any user name. Tests confirmed
that nested podman accepts this, in Fedora 44 and Fedora 45 VMs with a host user
of uid 2000: a nested `podman run` and `podman build` work and use exactly those
ranges, so the fallback of a fixed user named `agent` is not needed. The image
declares `VOLUME /var/tmp`: podman 6 (Fedora 45) puts an overlay upper directory
for the build context under `/var/tmp`, which fails on the container's overlay
root, and `--rm` removes the anonymous volume with the container.

Mounts, in this order: the state directory as home, each config root read-only
under `/run/bree/roots/<n>`, the nested storage volume, then the profile's
mounts, the working directory and `--mount` from the command line, sorted by
container path so a parent comes before what is under it, and last the
file-secret mounts and the host's gws login. A destination given twice is an
error, except that the working directory is skipped when a mount already covers
it, and the host's gws login is skipped when a mount already has its
destination.

Before it execs podman, `bree run` creates every destination that lies in the
state directory and not inside another mount, as the host user: a directory, or
for a file mount its parent and an empty file. Otherwise crun creates the
missing ones and their parents as the container's root, which is a subuid on the
host, and the harness cannot create its files next to them (`.local` under the
nested storage volume). When a directory on the way or a destination already
exists and belongs to another uid, `bree run` exits 2 and prints the
`podman unshare chown 0:0` command that gives those paths back.

The container runs `bree entrypoint -- <argv> <args>`, where `<argv>` is the
profile's `harness-command` for the harness, or the harness's name. The
entry point renders the configuration into the home, then `exec`s the harness,
so no wrapper process sits between podman and the harness. bree passes `--init`,
so podman's init process reaps the zombies that nested tools leave behind.

## Building blocks

A root holds a library of blocks, each a file or directory with a name, and a
profile names the blocks it uses. There is no stacking and nothing overrides
anything by path: every rendered file comes from exactly one block, or for
settings from the patches the profile lists, so the profile file says what a
render contains.

| Kind | Where in the root | Rendered as |
|---|---|---|
| rules | `rules/<name>.md` | one fragment of `CLAUDE.md` or `AGENTS.md` |
| skills | `skills/<name>/`, with `SKILL.md` and any extra files of the skill | `skills/<name>/` |
| commands | `commands/<name>.md` | a slash command or prompt template |
| settings | `settings/<harness>/<name>.json` (`.jsonc` accepted) | a patch over the harness's settings file |
| files | `files/<harness>/<name>/...` | every file of the bundle, copied into the harness directory at its relative path |

### Composition rules

In the `rules`, `skills` and `commands` lists, an entry `HARNESS:NAME` is used
only when rendering that harness (`claude:claude-code`), and every other entry
for every harness in `harnesses`. A name listed under `optional` is skipped in
every list when no root has that block, so a public profile can name a block
that only a private root holds. (A `?` suffix on the entry was the first idea.
`[work-internal?]` is valid YAML 1.2, and libyaml and the `yaml` npm package
parse it, but PyYAML's pure-Python parser rejects it, so Python tools that read
profile files would break on it. One `optional` list also keeps the private
names in one place.) Any other missing block is an error with the profile file
and line. `skills: all` and `commands: all` take every block of that kind in the
roots, which is the easy way to give a profile "all my skills";
`profile contents` shows what that resolved to.

Rules are concatenated per harness in the order the profile lists them,
separated by one blank line, so the order is the profile's and file names carry
no numbers. This replaces a Jinja `include` in a `CLAUDE.md.j2` and separate
AGENTS.md plus overlay files.

Settings are merged as JSON Merge Patch (RFC 7396) in list order, starting from
an empty object: objects merge key by key, arrays and scalars replace, and
`null` removes a key. `.jsonc` input may have comments and trailing commas, as
opencode allows; output is plain JSON, so comments in opencode's config do not
survive rendering. Put explanations in a README in the root instead.

Two `files` bundles of one profile that both have a file at the same relative
path are an error, and so are a `files` file and a rendered file at the same
path (a bundle with `settings.json`), so nothing is replaced silently.

`tiers.yaml` combines the pools of all roots in root order, so a private root
can add an internal pool; a pool defined in two roots is an error naming both
files, and one pool's tiers come from one file only. `capture.yaml` lists add up
across roots.

### Templates

Rules, commands, skills and settings may contain placeholders, and nothing else
in the way of templating: no conditionals, no loops, no includes. A profile that
needs a different text names a different block.

| Placeholder | Becomes |
|---|---|
| `{{tier.big}}` | The model id of tier `big` in the profile's pool, for the harness being rendered. |
| `{{tier.big.name}}` | Its display name, such as `Opus` or `Qwen 27B`, for prose: "hand off to a {{tier.medium.name}} subagent". |
| `{{var.NAME}}` | A value from the profile's `vars`, or from `--var NAME=VALUE`, which wins. |
| `\{{` | A literal `{{`. |

An unknown tier, pool or variable is an error at render time, with the file and
line. Plain files under `files/` are copied byte for byte, never templated, so
scripts such as `statusline.sh` are safe.

`--var` and `--add` are what replace per-host Ansible templating: the role
renders the host side with, for example,
`--var statusline=~/.claude/statusline.sh` and `--add settings.claude=host-a`,
where `settings/claude/host-a.json` holds that host's settings patch.

## What the renderer writes

Per harness, relative to the harness directory. The directory is `~/.claude`,
`~/.pi/agent` or `~/.config/opencode` under the container home (container
target) or the host home (host target).

| Source | Claude Code (`~/.claude`) | pi (`~/.pi/agent`) | opencode (`~/.config/opencode`) |
|---|---|---|---|
| rules | `CLAUDE.md` | `AGENTS.md` | `AGENTS.md` |
| skills | `skills/<skill>/` | `skills/<skill>/` | `skills/<skill>/` (opencode 2.2.6 reads both `skill/` and `skills/` in its config directory) |
| commands | `commands/<name>.md` | `prompts/<name>.md` | `command/<name>.md` |
| settings | `settings.json` | `settings.json` | `opencode.json` |
| files | `files/claude/<name>/*` as is | `files/pi/<name>/*` as is (`APPEND_SYSTEM.md`, `models.json`, `auth.json`) | `files/opencode/<name>/*` as is |

For Claude Code on a pool other than `claude`, the renderer also sets
`env.ANTHROPIC_DEFAULT_OPUS_MODEL`, `env.ANTHROPIC_DEFAULT_SONNET_MODEL` and
`env.ANTHROPIC_DEFAULT_HAIKU_MODEL` in `settings.json` to the pool's `big`,
`medium` and `small` models, so Claude Code's own model aliases (the `model`
parameter of its Agent tool) land on the pool's models. The profile's settings
blocks win over these. Only the tiers `big`, `medium` and `small` that the pool
defines are set, with fallbacks resolved. When any is set, `settings.json` is
written even if the profile lists no Claude settings blocks, and `tiers.yaml`
(of the root that defines the pool) is a manifest source before the blocks.

For Claude Code on the container target, the renderer also tells Claude Code's
auto mode classifier that it runs in a bree container, so that it judges a
command knowing what is real and what is scratch. Claude Code 2.1.331, the
version the base image pins, reads these settings from the user settings:
`skipAutoPermissionPrompt` (the user accepted the auto mode opt-in dialog) and
the lists of strings `autoMode.environment`, `autoMode.allow` and
`autoMode.soft_deny`, where the string `"$defaults"` stands for the built-in
entries at its position. The renderer seeds the opt-in as accepted, and one
entry in `autoMode.environment` after `"$defaults"`:

```json
{
  "skipAutoPermissionPrompt": true,
  "autoMode": {"environment": ["$defaults", "<the description of the container>"]}
}
```

The container target is what `bree entrypoint` renders, and what `bree render`
and `bree check` render with `--target container` (the default), with or without
`--out`, since an `--out` render of the container target is what the container
would get. `bree profile contents` renders as the container target too. The host
target (`--target host`, with or without `--out`) gets no seed, because the
description would be false on the host, and pi and opencode get none either.

The description is one English string that bree writes from the profile, and
every sentence says only what `bree run` does. It says, in this order:

- the container: rootless podman, started by bree for the profile, with the
  keep-id user namespace, so uid 1000 in the container is the host user;
- the home `~`: the profile's state directory, with its host path, and not the
  host user's home; with `state.home: image` it is the image's own home,
  discarded with the container;
- each mount of the profile in its order: the host path or volume name, the
  container path, read-write or read-only, `(optional)` if it is, and for a
  read-write host path that changes there are real; then the config roots,
  read-only under `/run/bree/roots`;
- the working directory and the mounts that `bree run` adds, which a render
  cannot know: that `bree run -m` adds host mounts, what `container.workdir`
  makes of the current directory (`cwd`: inside a mount, or else mounted
  read-write at the same path; `mount`: inside a mount; a path: that path, with
  nothing mounted for it), and that changes there are real unless a read-only
  mount holds it;
- the network: `pasta` is a network namespace of its own with outbound internet
  access and the host's LAN reachable, `host` is the host's network namespace,
  so services on the host's localhost are reachable, and `none` is no network;
- with `container.nested-podman`, that rootless podman works inside, with its
  storage in the nested volume, and that containers started there stay inside
  the container;
- with `container.kvm`, that `/dev/kvm` is available for VMs that stay inside
  the container, or, for `optional`, when the host has it;
- each secret by the names of the environment variables and the paths of the
  file mounts it reaches the container as, never by value, and never by reading
  a secret source.

Nothing in the string changes between two renders of the same profile and home,
so renders are reproducible, and `bree check` compares it like any other
content: a change to the profile's mounts, network or secrets makes
`settings.json` stale, as a changed block does. The string holds the host paths
of the state directory and the mounts, which come from the profile and `HOME`,
so a committed `--out` tree of the container target is the same only where
`HOME` is: a CI that checks one sets `HOME` to a fixed path. The sentences
follow `bree run`, so a change to what `run` mounts or passes is a change to
this text.

The seed is merged before the profile's settings blocks, so a block can change
it. After all blocks are merged, if `autoMode.environment` is an array that does
not hold the description yet, the renderer appends the description at the end. A
block that sets its own list, with or without `"$defaults"`, for its
organization's or host's entries, keeps its list and gets bree's entry after it.
A block that removes the key with `null`, or sets it to something other than an
array, keeps its choice: `null` is how a profile opts out. A block can set
`skipAutoPermissionPrompt` to `false` or `null` in the same way.
`autoMode.allow` and `autoMode.soft_deny` are never touched.

As with the tier env vars, the seed makes `settings.json` written even if the
profile lists no Claude settings blocks. In the manifest the seed is the first
source of that file, named `auto mode (bree)` for want of a block (its root is
the one that holds the profile), followed by `tiers.yaml` when the pool sets the
model env vars, and then the blocks.

In the container, `CLAUDE_CONFIG_DIR` is set to `~/.claude`, so `.claude.json`
lives there too.

### The manifest

Each harness directory of the container target, and of `--out`, gets
`.bree-manifest.json`: bree's version, the profile, the roots, and for each
rendered file its relative path, the sha256 of what bree wrote, and its sources:
the block and root it came from, or for a settings file the blocks patched in
after the seeds the renderer sets itself (`auto mode (bree)` and `tiers.yaml`,
which are no blocks), or for a rules file each rules block with its line range
in the output. The host target keeps the same manifests in one file,
`host.json`, and writes no `.bree-manifest.json` into the host's harness
directories (below, "Host target"). bree also keeps a copy of what it wrote
under `rendered/<harness>/` in its bookkeeping directory, which is what
`capture` diffs against and merges from; these are small text files.

The manifest is a JSON object with the keys `bree` (version), `profile`,
`harness`, `roots` (a list of `name` and absolute `path`) and `files`, sorted by
path. Each file has `path`, `sha256`, `kind` (`rules`, `skills`, `commands`,
`settings` or `files`) and `sources`, a list of `block` and `root`, plus `lines`
(first and last line, 1-based) for a rules block.

The bookkeeping directory is `~/.local/state/bree/` in the home of the target,
and every `rendered/`, `carried/` and `overridden/` path below is under it. For
the container target that home is the profile's state directory, so the entry
point writes `~/.local/state/bree/` inside the container and `capture` on the
host reads the same files at `<state.dir>/.local/state/bree/` (by default
`~/.local/share/bree/state/<profile>/.local/state/bree/`). The paths need no
profile level, because each profile has its own state directory, and two
profiles never share bookkeeping. For the host target it is the host's own
`~/.local/state/bree/`, which only `render --target host` and
`capture --target host` use; one host has one set of harness directories, so
there is no profile level there either, and the manifests in `host.json` name
the profile that was rendered. `check --target host` reads it and never creates
it.

JSON files are hashed in a canonical form (sorted keys, fixed spacing), so a
file that Claude Code rewrote in its own key order still counts as unchanged, as
the role does today. bree only ever writes, replaces or removes files listed in
the manifest or produced by the current render; sessions, credentials and
anything else the harness keeps there are never touched.

### Container target

The entry point renders at every start, because the blocks are the source of
truth and the container directory is bree's. What it does with a file the
harness changed since the last render depends on the kind of file.

Settings files (`settings.json`, `opencode.json`) carry the change over.
Harnesses write settings at runtime (`/model`, `/config`), and a JSON Merge
Patch re-applies cleanly on a new render. The entry point renders fresh,
computes the session's change against bree's copy of what it wrote last as a
merge patch, and applies that patch on top, so a model switched in a session
survives restarts. It prints one line per carried file on stderr at every start
(`bree: claude settings.json carries 1 key from a session, run bree capture`),
until `bree capture` writes the change into the sources or `bree render --force`
drops it. When a block changed a key the carried patch also sets, the carried
value wins and the line names the key, since it is the newer decision; `capture`
then shows the conflict (below). The carried patch lives under
`carried/<harness>/<file>.json` in the state directory's bookkeeping directory
(above) with its manifest entry, which is what `capture` reads from the host.

Every other file (rules, skills, commands, plain files) is overwritten. A
changed one is first saved to `overridden/<UTC timestamp>/<harness>/` in the
same bookkeeping directory and reported on stderr in one line, so a change made
in a session is never lost silently, only undone. That directory holds three
things per file, because the next render replaces both the rendered copy and the
manifest: the changed file, the base (bree's copy of what it wrote then, from
`rendered/`) and that file's manifest entry with its sources and rules line
ranges. `bree capture` on the host diffs each saved file against its own base,
maps it with its own manifest entry, and merges three ways with a fresh render
when the blocks changed since, so it brings the change into its block however
many starts later it runs. `capture` removes a directory once every file in it
is captured; `--dry-run` and refused files keep it.

### Host target

`render --target host` applies the managed-copy rule (above, `bree render`) to
`~/.claude`, `~/.pi/agent` and `~/.config/opencode`. It decides every file
before it writes any. A rendered file is `installed` when it is not there,
`current` when it already is what the render would write (and executable,
where the render says so), `updated` when it differs and is
unchanged since bree wrote it, and a `conflict` when it differs and was edited
since bree wrote it, or when bree never wrote it. `current` wins over whatever
bree recorded, so an edit that lands on the rendered content is no conflict.
What the render would write means the same bytes or, for a `.json` file that
parses, the same value in any key order or spacing, so a harness that rewrote
its settings in its own key order is left as it is; keys `capture.yaml` ignores
still count here. A
file with the rendered content that only lacks the executable bit is `updated`,
since nothing of yours is lost.

A file is unchanged since bree wrote it when it equals bree's copy of what it
wrote (under `rendered/<harness>/`; key order and the keys `capture.yaml`
ignores do not count, as in `check`), or, when that copy is gone, when its
sha256 is the recorded one. A file that bree wrote and the render no longer
produces is `removed` while it is unchanged, and a conflict once it was edited,
so bree never deletes an edited file silently; `--force` removes it. A recorded
file that is already gone only leaves the state, without a row. bree touches no
other file in the harness directories: sessions, credentials and `.claude.json`
are neither rendered nor recorded.

When any file is in conflict and `--force` is not given, bree writes nothing to
the harness directories, to `host.json` or to its copies. It empties
`~/.local/state/bree/pending/`, saves its version of each conflicting rendered
file there under the path the file has in the home, prints one row per conflict
with a `diff -u` command (for an edited file that no block produces any more,
the row says so instead) and exits 1:

```
claude  ~/.claude/settings.json  conflict  diff -u ~/.claude/settings.json ~/.local/state/bree/pending/.claude/settings.json
bree: 1 conflict, nothing written; take bree's version from ~/.local/state/bree/pending/ or move your change into the blocks, then render again (--force overwrites)
```

You take bree's version from `pending/` or move your change into the blocks,
and render again. `--force` turns every conflict into an update, or into a
removal for a file no block produces any more. A render that goes through,
forced or not, removes `pending/`, so it only ever holds the last refusal.

The state is `~/.local/state/bree/host.json`, a JSON object with the keys `bree`
(version) and `harnesses`: the manifest of each harness directory bree rendered
(above, "The manifest"), by harness name in sorted order. A render of some
harnesses (`--harness`) replaces only their entries and keeps the others. bree
writes it atomically after the files and its copies, and the host's harness
directories get no `.bree-manifest.json`.

`host.json` takes over the state file that the role, the shared config
repository's deploy tool, writes to `~/.local/state/ai-role/deployed.json`.
While `host.json` does not exist, bree reads that file instead, so the first
`bree render --target host` on a host the role set up does not see every file
the role wrote as a conflict. The role records the sha1 of each file by its
destination in the home (`{".claude/CLAUDE.md": "3f2a..."}`). An entry belongs
to the harness whose directory starts the destination, and the rest is the path
in it; entries for other directories, and paths that leave the harness
directory, are skipped. A file is unchanged since the role wrote it when the
sha1 of its bytes is the recorded one or, for a `.json` file that parses, when
the sha1 of what Python's `json.dumps(value, sort_keys=True)` prints for it is.
That is the role's own rule, and it keeps a harness that rewrites its settings
in another key order from counting as an edit. A float that Python prints
differently from serde_json (`1e-05`) can only make such a file a conflict,
never an overwrite. A `deployed.json` that is not a JSON object of strings is an
error that names the file (exit 2). Once `host.json` exists, `deployed.json` is
never read again, and bree never changes or deletes it. So the first render
records every harness: while it reads `deployed.json`, a render that leaves out
a harness the role recorded files for, because of `--harness` or because the
profile's harnesses do not list it, exits 2 and writes nothing.

### How `check` detects stale output

`check` renders into memory and compares each file with the disk. It needs no
timestamps or source hashes: a file is stale exactly when its content differs
from what a render would write now. `modified` and `extra` come from the
manifest. This makes `check` usable in three places: in the shared config
repository's CI against an expected tree under `--out`, on a host before a role
run (`--target host`), and in a running container to see whether a block changed
since the start.

On the host target `check` asks the plan that `render` makes (above, "Host
target"), so the two cannot disagree: `modified` means a render would refuse the
file, and a file that already has the rendered content is current whatever bree
recorded. The record is `host.json`, or the role's `deployed.json` while there
is no `host.json`. `check` writes nothing there, not even the bookkeeping
directory.

## Tiers manifest

Prompts and settings name a tier, the profile names a pool, and the renderer
substitutes the pool's model. The tiers are fixed by name in the manifest, not
in bree: a manifest can define any tier names, and the examples here use `big`,
`medium` and `small`.

`tiers.yaml` lives at the top of a config root, and the pools of all roots are
combined in root order, so the private root can add an internal pool (the same
pool in two roots is an error):

```yaml
pools:
  claude:
    big:    {model: claude-opus-5-5,   name: Opus}
    medium: {model: claude-sonnet-5-5, name: Sonnet}
    small:  {model: claude-haiku-5-5,  name: Haiku}

  corp:
    big:
      model: example/big-model
      name: Big Model
      opencode: corp/example/big-model
    medium: {fallback: big}
    small:
      model: example/small-model
      name: Small Model
      opencode: corp/example/small-model

  llama:
    big:
      model: Qwen3.8-27B-UD-Q6_K_XL
      name: Qwen 27B
      pi: llama/Qwen3.8-27B-UD-Q6_K_XL
    medium: {fallback: big}
    small:  {fallback: medium}
```

A tier entry is either `model` (the id), with optional per-harness ids under the
harness name (`claude`, `pi`, `opencode`; opencode and pi address models as
`provider/model`) and an optional `name` (the display name, which defaults to
the model id), or `fallback` alone, naming another tier of the same pool whose
entry is used as a whole. Fallback chains are followed until a tier with a
model. Pool and tier names are letters, digits, `-` and `_`. A tier with neither
`model` nor `fallback`, a `fallback` next to any other key, a fallback to an
unknown tier, and a fallback cycle (reported as `medium -> small -> medium`) are
errors when the manifest is loaded, for every pool whether a profile uses it or
not, so every chain ends in a tier with a model. A profile whose `pool` is not
in the manifest is an error at render time, even when no block uses a tier.

## Secrets

A profile declares each secret by a name, a source and where it goes. It never
holds a value, and a `value` key is a validation error.

| Key | Meaning |
|---|---|
| `source` | `env`, `file`, or `broker`. |
| `var` | For `env`: the host variable to read. |
| `path` | For `file`: the host file to read (`~` allowed). It must not be group- or world-readable. |
| `url` | For `broker`: the broker's base URL (`http://` or `https://`; a trailing `/` is dropped). |
| `token` | For `broker`: the host file with the operator token (`~` allowed). Checked like a `file` secret's `path`. |
| `route` | For `broker`: the provider route; only `anthropic`. |
| `env` | Container variables that get the value (for `broker`, the token). |
| `harness-env.<harness>` | Extra variables only when that harness runs, for providers that read their own names. |
| `mount` | For `file`: also mount the file read-only at this container path (for example `/run/secrets/forgejo-token`), instead of or in addition to `env`. |
| `required` | Default `true`. `false` starts the container with a warning when the source is missing. |

Values go into podman's environment and are passed by name (`-e NAME`), so they
never appear on the command line or in `--dry-run`. File secrets with `mount`
are bind-mounted read-only; bree does not copy them anywhere.
`bree profile show` and `doctor` report whether a source is available, never its
content.

`bree run` also shares the host's gws login with the container when it is there:
`~/.config/gws/credentials.json`, or `credentials.enc` together with
`.encryption_key`, is mounted read-only at the same path under the container
home. gws's token cache stays in the state directory, so a refresh writes there
and not to the host. A profile mount or file secret at one of those
destinations replaces the automatic mount.

`broker` is the source for an unattended job that spends through the credential
broker agentic-job uses for inference, so that the job never holds the provider
key. It needs `url`, `token` and `route: anthropic`, takes `env` for extra
container variables that get the token, and takes neither `var`, `path`,
`mount` nor `harness-env`. The token file is read like a `file` secret's: it
is an error when it is missing (unless `required: false`) or group- or
world-readable, and a trailing newline is dropped.

For the `claude` harness `bree run` sets `ANTHROPIC_BASE_URL` to the `url` plus
`/anthropic`, `ANTHROPIC_AUTH_TOKEN` to `praxis-substitute:anthropic`, and
`ANTHROPIC_CUSTOM_HEADERS` to `x-run-token: <token>`. The first two are not
secret and are plain `-e NAME=value` like `container.env`; the header holds the
token, so it and every `env` variable are passed by name like any secret value.
The profile never gets `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`: a
secret, `container.env`, `container.pass-env` or `--env` that names either, or
any of the three variables above, is an error.

Any other harness is refused. A profile with a broker secret must say
`harness: claude`, no other entry in `harnesses` and no `command`, and only one
secret may be a broker; `bree run pi`, `opencode` or `shell` on it is an error.
`bree run` also refuses a broker profile whose state directory holds
`.claude/.credentials.json`, with a message that says to move it away, so a job
never spends from a stored login. `bree export quadlet` keeps refusing the
source.

This is the first step: bree has no HTTP client and registers no run, so the
token is the operator token. Per-run tokens, where `bree run` registers the run
with the broker and passes its short-lived token instead, are the later step.

## Base image

`bree-base` is a generic image with nothing personal in it:

- Fedora (the current release; 45 today);
- the harnesses, each pinned: Claude Code (by the installer's version argument,
  auto-updater off), pi and opencode (npm, pinned versions);
- nested rootless podman with `containers/*.conf` settings based on those of
  `quay.io/podman/stable`, and `rpm --setcaps shadow-utils` for `newuidmap`.
  Since the image has no home of its own, the settings that would live in the
  user's `~/.config/containers` go to `/etc/containers` (`image/containers/`);
- the generic tools the harnesses call: git, jq, make, diffutils, findutils,
  which, less, file, procps-ng, openssh-clients, ripgrep, fd-find, nodejs with
  full ICU;
- `gws` (googleworkspace/cli), pinned by release version; a generic tool for
  now, until the base image can be extended per profile (issue #2);
- the `bree` binary itself as the entry point.

The Containerfile is in the repository at `image/Containerfile` and is compiled
into the binary (`include_str!`), so `bree image build` works from any install
without a checkout. `bree image build` copies its own executable into the build
context, so the `bree` in the image is always the one that built it, and the
image carries the labels `org.opencontainers.image.version` (bree's version),
`bree.containerfile-sha256`, `bree.exe-sha256` (the SHA-256 of the copied
executable) and `bree.commit` (the commit in `bree --version`). When the
embedded Containerfile or the `bree` binary changes, the next `bree run` sees a
different label and rebuilds. The executable's hash is the one that catches
every new build, also one with the same version and commit (a local build with
uncommitted changes, an RPM rebuild); the commit is there for people reading
`image status`. Since only the image's last layer holds the binary, such a
rebuild reuses the build cache for the rest. `bree` hashes and copies
`/proc/self/exe`, the file it runs, so an update that replaces the binary on
disk during a run does not mix two builds.

The project's CI builds the image and, on every merge, pushes
`<registry>/bree-base:<version>` and `:<version>-<YYYYMMDD>` to the project's
Forgejo registry, and `:<version>-<YYYYMMDD>-<commit>` (the commit's
12-character hash) only when the registry does not have it yet. The first two
move, and Forgejo drops a manifest when its tag is pushed again, so only the
third keeps its digest. A downstream image can start `FROM` that per-commit tag
pinned by digest, so that a bump is a reviewed one-line change, like the Claude
Code pin in the Containerfile. Once bree is on GitHub, the same workflow pushes
to a public registry, for hosts outside the project's network.

`bree-base` is meant as the base of downstream images: they add their own
packages and configuration on top and install no harnesses of their own.

## Installing bree

On a laptop, from the RPM in a Copr project, built by Packit from the GitHub
repository. Until that exists, from a checkout:
`cargo install --locked --path .`.

The release binary is statically linked (`x86_64-unknown-linux-musl`), so the
same file runs on the host and in the image whatever their glibc. If Fedora's
Rust packages lack the musl target, the release falls back to a glibc build, and
the image's Fedora must then be at least as new as the oldest host's.

The role installs bree: the Copr package where the host has the repository
enabled, or a release binary into `~/.local/bin/bree` with a checksum check, and
writes `~/.config/bree/config.yaml` and the shared config repository's root to
`~/.local/share/bree/roots/shared/`.

In an image built on `bree-base`, `bree` comes with the base.

## Example configuration layout

The shared config repository keeps its blocks in one config root, for example at
`config/`:

```
config/
  tiers.yaml
  profiles/
    home.yaml
    work.yaml
  rules/
    core.md                       rules every profile shares
    claude-code.md                the Claude Code section, with tiers instead of model names
    home.md
    work.md
    unattended.md                 no questions, report in the final message
  skills/
    example-skill/
    review-text/
    update-dependencies/
  commands/
    review-text.md
  settings/
    claude/home.json
    claude/work.json
    claude/host-<name>.json       per-host settings patches
    opencode/work.jsonc
  files/
    claude/statusline/statusline.sh
    pi/base/APPEND_SYSTEM.md
    pi/work/models.json
    pi/work/auth.json
```

A work machine adds a private root `~/.config/bree/private/` with blocks such
as `rules/work-internal.md` and `settings/claude/work-internal.json`, which is
not in any repository.

## Crew profiles and quadlets

A crew is a set of unattended agents that run as services on a server, for
example a code review agent woken by a timer. Each agent runs as a quadlet
unit that a deploy tool renders from a template, and most of each template's
`[Container]` section repeats the others. A crew profile describes one such
agent, and `bree export quadlet` turns the profile into that section. The crew's
image already carries its own configuration, so a crew profile renders nothing
and keeps the image's home; it describes only the container.

The deploy tool keeps what is about the service, not the container: `[Unit]`,
`[Service]`, `[Install]`, the timers, and the `Environment=` lines of the knobs
it sets from its variables (models, intervals, quota thresholds). It includes
the exported `[Container]` section in the unit's template. It runs the exporter
from the crew image, not from a bree installed on the server, so the profile
format is always that of the bree the crew image pins:

```
podman run --rm -v CHECKOUT:CHECKOUT:ro CREW-IMAGE \
  bree --root CHECKOUT/bree --relative-to CHECKOUT -p UNIT export quadlet
```

CHECKOUT is the crew's repository checkout, as the server sees it. The exporter
only reads it, so it is mounted read-only at the same path, and the paths bree
resolves inside the container are the host paths the unit needs. The profiles
live in `bree/profiles/` there, a config root of their own, and host paths such
as `secrets/ssh` are written relative to the checkout top and resolve to
`CHECKOUT/secrets/ssh` in the unit. Relative paths get their own option instead
of resolving against the config root because the exporter runs inside the crew
image, so the paths written into the generated unit must not depend on where the
config root is mounted. For the same reason the export does not expand `~` in a
host path: bree would expand it to the home of the user it runs as, which is the
crew image's home, not the server user's. A host path written `~/x` is exported
as `%h/x`, and a bare `~` as `%h`, so systemd fills in the home of the user who
runs the unit, and the same profile still works with `bree run` on the server.
Other host paths outside the checkout are absolute. They do not exist in the
exporter's container, which mounts only the checkout, so `bree export quadlet`
resolves host paths but checks none of them: a missing host path fails when
podman starts the unit, not at export.

The keys and options this needs, all additions to what is above:

| Key or option | Meaning |
|---|---|
| `image.source: local` | Use the local image `image.ref` as it is: `--pull=never`, never build or pull. |
| `command` | List of strings: the argv the container runs instead of `bree entrypoint -- <harness>`, so nothing is rendered. An error together with a harness. |
| `state.home: image` | Mount no state directory and keep the image's home. An error together with configuration to render. |
| `container.name` | Fixed `--name` instead of `bree-<profile>-<dir>-<hex>`. |
| `container.workdir: <path>` | Besides `cwd` and `mount`, an absolute path: a fixed `--workdir`; the current directory is neither used nor mounted. |
| `container.kvm: optional` | Besides `true` and `false`: pass `/dev/kvm` only when it exists on the host. |
| `container.storage-volume` | Name of the nested storage volume instead of `bree-<profile>-containers`. |
| `container.label-disable` | `label=disable` without the nested storage volume; `nested-podman: true` keeps disabling labels whatever this says. |
| `mounts[].volume` | A named podman volume instead of `host`; `container` is then required. |
| `mounts[].relabel` | `true` (default) adds `:z`; `false` leaves the SELinux label of the host path alone. |
| read-only directory mounts | A directory mounted with `mode: ro`, such as `secrets/ssh`, is a plain mount: only file secrets get the mode check of "Secrets". |
| relative `mounts[].host` | Resolves against `--relative-to DIR` (see "CLI"), whichever root the profile or block came from; an error without the option, as before. Absolute and `~` paths ignore it. |
| `bree export quadlet` | Prints the profile's quadlet `[Container]` section on stdout, from the same profile model as `bree run`, with `AddDevice=-/dev/kvm` for `kvm: optional` and `PodmanArgs=` for what quadlet has no key for. It checks no host path for existence (see above). It writes no `[Unit]`, `[Service]`, `[Install]` or knob `Environment=` lines, and fails on a profile with a harness. |

How `bree run` treats the image, command, state and container keys where the
table leaves it open. A profile with `command` takes no `HARNESS` argument, and
`ARGS` after `--` are appended to the command. `command` also excludes
`harnesses` and `harness-command`, and `state.home: image` excludes `state.dir`,
`harness`, `harnesses`, `rules`, `skills`, `commands`, `settings` and `files`.
`bree render` and `bree check` of such a profile need `--out`, since it has no
state directory. `container.workdir` set to a path skips the refusal of a
working directory that is the home, and the generated container name takes that
path's last component instead of the current directory's.
`container.storage-volume` needs `nested-podman: true`. `bree image build`
refuses an image with `source: local`.

How `bree run` treats the mount keys. `relabel` applies to the profile's host
mounts only: `--mount`, the working directory, the state directory, the config
roots and file secrets keep their labels as before. `:z` relabels the host path
even with `nested-podman: true`, since podman still gives a `label=disable`
container a mount label. So the `home` and `work` profiles above relabel all of
`~/src` on their first run, unless the mount sets `relabel: false`. A `volume`
entry takes neither `optional` nor `relabel`, and its options are only `ro`.
When `--relative-to` is given and the profile starts a harness, `bree run`
passes the option on to `bree entrypoint`, which loads the profile again inside
the container.

How `bree export quadlet` differs from `bree run`. It accepts only a profile
with `command` and `state.home: image`, and for such a profile it writes what
the unit needs, not `bree run`'s argv:

- It always writes `LogDriver=passthrough`, so the unit logs to the journal.
  There is no profile key for it, as there is none for `RunInit=true`, which
  stands for `--init`.
- It writes `UserNS=keep-id`, without `uid=1000,gid=1000`, and no passwd
  entry: those give the host user the image's uid and a home at the state
  directory, and an image-home profile needs neither.
- It writes no `BREE_PROFILE` and no `/run/bree/roots/N` mounts, since a
  command profile never runs the entry point, and nothing for `-it` and
  `--rm`.
- A host path written with `~` becomes `%h` (above). Relative paths resolve
  against `--relative-to` and absolute paths stay as they are, as in `bree run`.
- It writes `WorkingDir=` only when `container.workdir` is a path. With `cwd`
  or `mount` it writes none and mounts no current directory, so the image's
  working directory applies.

It reads no secret value and checks no host path, so `optional` on a mount has
no effect: the line is always written. A file secret with a `mount` becomes a
read-only `Volume=` line; a secret that passes its value in the environment is
an error, since the export cannot pass it without reading it. A literal `%` in a
written value becomes `%%` and a literal `$` becomes `$$`, since quadlet copies
the values into `ExecStart=`, where systemd expands `$NAME`. An `Exec=` word or
`Environment=` assignment with a space, quote, backslash or control character is
double-quoted, with control characters as C escapes (`\n`); a control character
in any other value is an error. The lines come in this order,
each left out when it has nothing to say: `Image`, `Pull` (`never` for `source:
local`), `ContainerName`, `Timezone`, `UserNS`, `SecurityLabelDisable` (for
`nested-podman: true` or `label-disable: true`), `AddDevice` (`-/dev/kvm` for
`kvm: optional`, `/dev/kvm` for `true`), `Unmask=ALL` (with `AddDevice`),
`RunInit`,
`LogDriver`, `Network`, `WorkingDir`, `Exec`, the `Volume` lines in `bree run`'s
order (the nested storage volume first, then the mounts by destination, then
the file secrets), the `Environment` lines of `container.env` in the profile's
order, and a `PodmanArgs=--env=NAME` line per `container.pass-env` name, which
quadlet has no key for. A crew profile sets `pass-env: []`, since the default
passes `TERM`.

The profile of a code review agent that a timer starts every hour (no model
login, no nested podman, no ssh key):

```yaml
description: "Hourly code review agent"
command: [crew-agent, code-review]

image:
  ref: localhost/crew:latest
  source: local

state:
  home: image

container:
  name: crew-code-review
  network: host
  nested-podman: false
  tz: UTC
  workdir: /home/agent/code
  env:
    CODE_HOME: /home/agent/code

mounts:
  # relative: resolves against --relative-to, the crew's checkout
  - host: secrets/code/api-token
    container: /run/secrets/api-token
    mode: ro
  - host: /srv/crew/code
    container: /home/agent/code
  # another agent's state, read-only, keeps its own SELinux label
  - host: /srv/crew/triage/state
    container: /home/agent/triage-state
    mode: ro
    relabel: false
```

## Later: a VM runtime

The README says "containers, and VMs". This design implements containers only,
and leaves room for VMs: a profile gets a `runtime` key (`container`, the
default, or `vm`), and the `container` keys stay container-only. A `vm` runtime
would boot a bootc image built from `bree-base` with bcvk, mount the same state
directory and roots over virtiofs, and run the same entry point, for agents that
must not share the host kernel. It is not part of the current plan and has no
issue yet.

## Non-goals

- Network egress filtering, a second user, or other sandboxing beyond rootless
  podman. agentic-job's `secure-host` covers that ground for CI.
- macOS, Windows and Docker. bree needs rootless podman on Linux.
- A general templating language. Placeholders are tiers and variables only.
- Harness login and credential management. bree passes secrets in; logging in
  (`/login`, `opencode auth login`) happens inside, and the state directory
  keeps it.
- Editing configuration interactively, or syncing runtime changes back on its
  own. bree renders files from blocks; `bree capture` writes a harness's changes
  back into the blocks when you run it, and you commit them.

## Decisions

These are the choices this document makes where another one was reasonable; a
later change to one is a change to this list first.

1. Profiles are a `-p` flag, not a positional argument
   (`bree -p work run claude`, not `bree work claude`).
2. YAML for profiles, tiers and capture lists, in the strict subset under "File
   formats"; JSON (and JSONC input) for harness settings, because that is what
   the harnesses read.
3. The container home is a per-profile state directory, so the `home` profile no
   longer shares the host's `~/.claude` login.
4. Rendering happens in the container at every start. A settings change made in
   a session is carried over until captured; any other changed file is
   overwritten and saved aside. On the host bree refuses conflicts. To revisit
   if overwriting the other files gets in the way.
5. Tier names `big`, `medium`, `small`, as a trial in the MVP; renaming them is
   a change to `tiers.yaml` and the prompts, not to bree.
6. Templating is limited to tiers and the profile's `vars:` (and `--var`).
7. The image is built locally by default, with the image's `bree` copied from
   the host binary.
8. `bree-base` is a base for downstream images.
9. Runtime changes come back through an explicit `bree capture` on the host that
   writes into the sources and leaves the commit to you, not an automatic sync,
   and it refuses what it cannot reverse safely (hunks across rules blocks,
   lines with placeholders) instead of guessing.
10. Profiles compose named building blocks from a library in the roots, listed
    explicitly per kind, instead of stacking layers that override each other by
    path. A profile is longer, and in return it says what it contains, with
    nothing replaced or removed implicitly.
11. Crew profiles keep the image's home (`state.home: image`): no state
    directory is mounted and nothing is rendered, since the crew image already
    carries its configuration.
12. `bree export quadlet <profile>` writes only the `[Container]` section, while
    the crew's deploy tool keeps `[Unit]`, `[Service]`, `[Install]`, the timers
    and the knob `Environment=` lines, so bree learns nothing about systemd.
13. The deploy tool runs the exporter from the crew image
    (`podman run --rm -v CHECKOUT:CHECKOUT:ro CREW-IMAGE bree export quadlet
    ...`), so the profile format is always that of the bree the crew image pins,
    and the server needs no bree install.
