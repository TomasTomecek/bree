//! `bree entrypoint` through the built binary: it renders the profile named by
//! `BREE_PROFILE` into HOME, then execs the command. The image CI job does the
//! same in the base image with the roots mounted under `/run/bree/roots/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A temp directory used as HOME, removed on drop.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        let dir =
            std::env::temp_dir().join(format!("bree-entrypoint-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home(dir)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entrypoint(home: &Home, profile: Option<&str>, command: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bree"));
    cmd.arg("--root")
        .arg(fixtures().join("roots/shared"))
        .arg("entrypoint");
    if !command.is_empty() {
        cmd.arg("--").args(command);
    }
    cmd.current_dir(&home.0)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home.0)
        .env("USER", "alice");
    if let Some(p) = profile {
        cmd.env("BREE_PROFILE", p);
    }
    cmd.output().expect("run bree")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The files as the fixtures have them: the temp HOME is written as
/// `/home/alice`, since the auto mode seed of `settings.json` names the state
/// directory and the mounts under the home, and the temp home has the process
/// id in it.
fn as_alice(home: &Home, files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    files
        .into_iter()
        .map(|(path, bytes)| {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.replace(home.0.to_str().unwrap(), "/home/alice");
            (path, text.into_bytes())
        })
        .collect()
}

/// Every file under `dir` by relative path, without the manifests and
/// bree's bookkeeping, as in tests/render.rs.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn go(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if rel == ".local" || rel.ends_with(".bree-manifest.json") {
                continue;
            }
            if p.is_dir() {
                go(base, &p, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(dir, dir, &mut out);
    out
}

#[test]
fn renders_the_profile_into_home_then_execs_the_command() {
    let home = Home::new("exec");
    let o = entrypoint(
        &home,
        Some("home"),
        &[
            "sh",
            "-c",
            "echo \"config=$CLAUDE_CONFIG_DIR\"; head -n 1 \"$HOME/.claude/CLAUDE.md\"",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("bree: rendered profile home: "),
        "{}",
        stderr(&o)
    );
    let out = stdout(&o);
    let config = format!("config={}/.claude\n", home.0.display());
    assert!(out.starts_with(&config), "{out}");
    let expected = fixtures().join("render/home");
    let first = std::fs::read_to_string(expected.join(".claude/CLAUDE.md")).unwrap();
    assert_eq!(
        &out[config.len()..],
        format!("{}\n", first.lines().next().unwrap())
    );
    // The container target: `settings.json` has the auto mode seed.
    assert_eq!(as_alice(&home, tree(&home.0)), tree(&expected));
    assert!(home.0.join(".claude/.bree-manifest.json").is_file());
    assert!(home.0.join(".pi/agent/.bree-manifest.json").is_file());
}

#[test]
fn renders_again_at_every_start_and_overwrites_changes() {
    let home = Home::new("again");
    assert!(entrypoint(&home, Some("home"), &[]).status.success());
    let rules = home.0.join(".claude/CLAUDE.md");
    std::fs::write(&rules, "changed in a session\n").unwrap();
    let stray = home.0.join(".claude/sessions.json");
    std::fs::write(&stray, "{}\n").unwrap();
    let o = entrypoint(&home, Some("home"), &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains(": 1 written, "), "{}", stderr(&o));
    let expected = fixtures().join("render/home/.claude/CLAUDE.md");
    assert_eq!(
        std::fs::read(&rules).unwrap(),
        std::fs::read(expected).unwrap()
    );
    // Files the harness keeps there and bree did not write stay.
    assert_eq!(std::fs::read_to_string(&stray).unwrap(), "{}\n");
}

#[test]
fn keeps_claude_config_dir_when_set() {
    let home = Home::new("config-dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bree"));
    let o = cmd
        .arg("--root")
        .arg(fixtures().join("roots/shared"))
        .args(["entrypoint", "--", "sh", "-c", "echo $CLAUDE_CONFIG_DIR"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home.0)
        .env("USER", "alice")
        .env("BREE_PROFILE", "home")
        .env("CLAUDE_CONFIG_DIR", "/elsewhere")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "/elsewhere\n");
}

#[test]
fn without_bree_profile_it_fails_and_starts_nothing() {
    let home = Home::new("no-profile");
    let o = entrypoint(&home, None, &["sh", "-c", "echo started"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("BREE_PROFILE is not set"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), "");
}

#[test]
fn a_render_error_starts_nothing() {
    let home = Home::new("missing-profile");
    let o = entrypoint(&home, Some("nosuch"), &["sh", "-c", "echo started"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("profile `nosuch` not found"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), "");
}
