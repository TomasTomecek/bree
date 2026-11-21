# Tutorial: a first bree profile

This tutorial is written for an agent, and it works for a person too. Run the
steps in order on a clean Fedora 44 or CentOS Stream 10 machine. Each step gives
the commands and what you should see. If you see something else, stop at that
step and read "When a step fails" at the end. [usage.md](usage.md) explains
every command used here in more detail.

The tutorial uses the user `alice` and the project directory `~/src/project`.
Your user name and home directory will differ. Paths under `/home/alice` in the
expected output will show your own home.

You need:

- a regular user that can run `sudo`,
- network access to the Fedora or CentOS mirrors and to crates.io,
- for step 7 only, rootless `podman` and network access to the Fedora registry
  and to the harness's download site.

## 1. Install bree

Once the first release is out, install the RPM from Copr:

```
sudo dnf copr enable -y ttomecek/tools-stable
sudo dnf install -y bree
```

Before the release, build it from a checkout. Install the build tools first:

```
sudo dnf install -y cargo git
```

Then get the source. If a checkout of bree is already at `~/src/bree`, skip the
clone:

```
git clone https://github.com/TomasTomecek/bree.git ~/src/bree
```

Build and install it into `~/.cargo/bin`. This compiles about 50 crates and
takes from under a minute to a few minutes, depending on the machine:

```
cd ~/src/bree
cargo install --locked --path .
export PATH="$HOME/.cargo/bin:$PATH"
```

Expect `cargo install` to end with a line like
`Installed package `bree v0.1.0 (/home/alice/src/bree)` (executable `bree`)`,
possibly followed by a warning to add `~/.cargo/bin` to your `PATH`. The
`export` line does that for this shell only, so run it again in every new shell,
or add it to `~/.bashrc` to keep it.

Check the install:

```
bree --version
```

You should see one line like this, with the commit bree was built from in
parentheses (a Copr build shows the same). Your commit will differ:

```
bree 0.1.0 (392e253e273e)
```

## 2. Create a config root

bree reads profiles and building blocks from config roots. Without any other
setting it uses `~/.config/bree/` as its only root, so this tutorial puts
everything there. Create the directories:

```
mkdir -p ~/.config/bree/rules ~/.config/bree/skills/review-text ~/.config/bree/profiles
```

Add one rule. A rule is a Markdown file that ends up in the harness's
instructions file (`CLAUDE.md` for Claude Code, `AGENTS.md` for pi):

```
cat > ~/.config/bree/rules/core.md <<'EOF'
Answer in plain English. Run the tests before you say a change is done.
EOF
```

Add one skill. A skill is a directory with a `SKILL.md` whose front matter has
a `name` and a `description`:

```
cat > ~/.config/bree/skills/review-text/SKILL.md <<'EOF'
---
name: review-text
description: Review a piece of prose for clarity and length.
---

Read the text, then list the three changes that would help a reader most.
EOF
```

Check what you created:

```
find ~/.config/bree -type f | sort
```

You should see:

```
/home/alice/.config/bree/rules/core.md
/home/alice/.config/bree/skills/review-text/SKILL.md
```

## 3. Write a profile

A profile names the harness and the blocks it uses. Nothing that the profile
does not name gets rendered. Create the profile `tutorial` and the project
directory it will run in:

```
cat > ~/.config/bree/profiles/tutorial.yaml <<'EOF'
description: "Tutorial: one rule and one skill"
harness: claude
rules: [core]
skills: [review-text]
mounts:
  - host: ~/src
EOF
mkdir -p ~/src/project
```

`harness: claude` makes `bree run` start Claude Code. Use `harness: pi` to
start pi instead. `mounts` lists the host paths the container sees, here
`~/src` at the same path.

Every bree command below takes the profile with `-p tutorial`. Ask bree what a
render of the profile would contain:

```
bree -p tutorial profile contents
```

You should see the rule, the skill and a settings file, each with the root it
comes from (`bree`, the base name of `~/.config/bree/`):

```
claude  ~/.claude
  rules     CLAUDE.md      core  bree
  skills    review-text          bree
  settings  settings.json        auto mode (bree)
claude: 1 rule, 1 skill, 0 commands, 1 settings file, 0 files, 0 unused
```

If you see `bree: ...` with a file name and a line instead, the profile or a
block has an error. Fix the file it names and run the command again.

## 4. Render the profile and read the result

`bree render --out DIR` writes the files the harness would read into `DIR`, laid
out like a home directory. It touches nothing else:

```
bree -p tutorial render --out /tmp/bree-tutorial
```

You should see three files written:

```
claude  /tmp/bree-tutorial/.claude/CLAUDE.md                    installed
claude  /tmp/bree-tutorial/.claude/settings.json                installed
claude  /tmp/bree-tutorial/.claude/skills/review-text/SKILL.md  installed
bree: 3 written, 0 current, 0 removed
```

Read the rendered files:

```
cat /tmp/bree-tutorial/.claude/CLAUDE.md
cat /tmp/bree-tutorial/.claude/skills/review-text/SKILL.md
cat /tmp/bree-tutorial/.claude/settings.json
```

`CLAUDE.md` holds the text of your rule. `SKILL.md` is your skill as you wrote
it. `settings.json` is a JSON object whose `autoMode.environment` describes the
container bree runs Claude Code in. Besides these three, the directory holds
bree's own bookkeeping: `.claude/.bree-manifest.json` and a copy of each file
under `.local/state/bree/rendered/`, which `bree check` uses in the next step.

## 5. Check the render

`bree check` renders the profile in memory and compares it with the files on
disk. It writes nothing. Check the directory you just rendered:

```
bree -p tutorial check --out /tmp/bree-tutorial
echo "exit status $?"
```

You should see:

```
bree: 0 of 3 files not current
exit status 0
```

Now edit a rendered file by hand, as a person might, and check again:

```
echo extra >> /tmp/bree-tutorial/.claude/CLAUDE.md
bree -p tutorial check --out /tmp/bree-tutorial
echo "exit status $?"
```

This time bree names the edited file and exits 1:

```
claude  /tmp/bree-tutorial/.claude/CLAUDE.md  modified  edited since bree rendered it
bree: 1 of 3 files not current
exit status 1
```

Edit the blocks in the root, not the rendered files: bree overwrites rendered
files on every render. Render again to get back to a current state, then check
once more:

```
bree -p tutorial render --out /tmp/bree-tutorial
bree -p tutorial check --out /tmp/bree-tutorial
echo "exit status $?"
```

The render reports `1 written, 2 current, 0 removed`. The check prints
`bree: 0 of 3 files not current` and `exit status 0`.

## 6. See what `bree run` would do

`bree run` starts the harness in a rootless podman container. `--dry-run`
prints the podman command without running it, so this step needs no podman:

```
cd ~/src/project
bree -p tutorial run --dry-run
```

You should see a `podman run` command, one option per line, that ends in
`localhost/bree-base:0.1.0` and `bree entrypoint -- claude`. It has more options
than this step names (the user namespace, the network, a volume for nested
podman). Among them are the container's home, which is the profile's state
directory (`-v /home/alice/.local/share/bree/state/tutorial:/home/alice`),
your root mounted read-only under `/run/bree/roots/0`, the `~/src` mount from
the profile, and `-w /home/alice/src/project`, the directory you ran it from.

## 7. Run the harness

This step needs rootless podman. Install it and check that it works for your
user:

```
sudo dnf install -y podman
podman info --format '{{.Host.Security.Rootless}}'
```

You should see `true`. If `podman info` fails, rootless podman does not work
for your user, and `bree run` cannot work on this machine. An error about
`/etc/subuid` or `/etc/subgid` means your user has no entries there. An error
from `newuidmap` (`Could not open proc directory` or `should have setuid or
have filecaps setuid`) means the machine allows no user namespaces, as inside
most containers. Stop here: steps 1 to 6 are the part of
bree that works without a container.

The first run builds the base image `localhost/bree-base:0.1.0` from the
Containerfile compiled into bree. That takes several minutes, pulls a Fedora
image and downloads the harnesses. To build it ahead of time and see that it
matches this bree:

```
bree -p tutorial image build
bree -p tutorial image status
```

`image build` ends with `Successfully tagged localhost/bree-base:0.1.0`, and
`image status` ends with the line `labels  match`.

Before you start the harness, look at what it will see. `run shell` starts bash
in the same container instead of the harness:

```
cd ~/src/project
bree -p tutorial run shell -- -c 'cat ~/.claude/CLAUDE.md; ls ~/.claude/skills; pwd'
```

You should see the container render the profile, then your rule, your skill and
the project directory:

```
bree: rendered profile tutorial: 3 written, 0 current, 0 removed
Answer in plain English. Run the tests before you say a change is done.
review-text
/home/alice/src/project
```

Now start Claude Code with the profile:

```
bree -p tutorial run claude
```

For pi, change `harness: claude` to `harness: pi` in the profile first, then
run `bree -p tutorial run pi`. bree renders blocks only for the harnesses the
profile lists, and a profile with only `harness: claude` lists just Claude Code,
so pi would start without your rule and skill. With `harness: pi`, pi reads the
rule from `~/.pi/agent/AGENTS.md`.
On the first run the harness asks you to log in. That is the end of what bree
does for you: the login is the harness's own, and bree keeps it in the profile's
state directory, `~/.local/share/bree/state/tutorial/`, so later runs skip it.
An agent working through this tutorial unattended stops here, because it cannot
complete the login.

Quit the harness to leave the container. Nothing it wrote outside `~/src` and
the state directory survives the run.

## When a step fails

- `bree: command not found` after step 1: `~/.cargo/bin` is not on `PATH`. Run
  the `export` line of step 1 again.
- `cargo install` fails with `linker `cc` not found`: install `gcc` with
  `sudo dnf install -y gcc` and run it again.
- A message that starts with `bree:` and names a file and a line: the YAML or
  front matter of that file is wrong. Compare it with this tutorial.
- `bree: no profile: pass -p NAME or set default-profile in ...`: a command ran
  without `-p tutorial`.
- `bree run` or `bree image build` fails before the harness starts, with an
  error from `podman`, `crun` or `newuidmap` and
  `podman exit status: 125`: rootless podman does not work in this environment,
  for example inside a container that forbids user namespaces. Report the error;
  steps 1 to 6 do not depend on it.

When you report a problem, include the step number, the command, its full
output and `bree --version`.
