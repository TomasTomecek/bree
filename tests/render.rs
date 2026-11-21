//! `bree render`, `check` and `profile contents` through the built binary,
//! against the fixture roots and the expected trees in `tests/fixtures/render`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A temp directory used as HOME and as `--out`, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("bree-render-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Tmp(dir)
    }
    fn out(&self) -> PathBuf {
        self.0.join("out")
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bree(tmp: &Tmp, roots: &[&str], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bree"));
    for r in roots {
        cmd.arg("--root").arg(fixtures().join("roots").join(r));
    }
    cmd.args(args)
        .current_dir(&tmp.0)
        .env_clear()
        .env("HOME", tmp.0.join("home"))
        .env("USER", "alice");
    cmd.output().expect("run bree")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Every file under `dir` by relative path, without the manifests and
/// bree's bookkeeping.
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

/// The files as the fixtures have them: the temp HOME is written as
/// `/home/alice`. The auto mode seed of `settings.json` names the state
/// directory and the mounts, which are paths under the home, and the temp home
/// has the process id in it.
fn as_alice(tmp: &Tmp, files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let home = tmp.0.join("home");
    files
        .into_iter()
        .map(|(path, bytes)| {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.replace(home.to_str().unwrap(), "/home/alice");
            (path, text.into_bytes())
        })
        .collect()
}

fn render_out(tmp: &Tmp, profile: &str, extra: &[&str]) -> Output {
    let out = tmp.out();
    let mut args = vec!["-p", profile, "render", "--out", out.to_str().unwrap()];
    args.extend_from_slice(extra);
    bree(tmp, &["shared"], &args)
}

#[test]
fn render_equals_the_expected_trees() {
    for profile in ["home", "work"] {
        let tmp = Tmp::new(profile);
        let o = render_out(&tmp, profile, &[]);
        assert!(o.status.success(), "{}", stderr(&o));
        let got = as_alice(&tmp, tree(&tmp.out()));
        let want = tree(&fixtures().join("render").join(profile));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{profile}: file lists differ"
        );
        for (path, bytes) in &want {
            assert_eq!(
                String::from_utf8_lossy(&got[path]),
                String::from_utf8_lossy(bytes),
                "{profile}: {path}"
            );
        }
        let last = stdout(&o).lines().last().unwrap().to_string();
        assert_eq!(
            last,
            format!("bree: {} written, 0 current, 0 removed", want.len())
        );
        assert!(tmp.out().join(".claude/.bree-manifest.json").is_file());
    }
}

#[test]
fn the_private_root_adds_the_optional_blocks() {
    let tmp = Tmp::new("private");
    let out = tmp.out();
    let o = bree(
        &tmp,
        &["shared", "private"],
        &["-p", "work", "render", "--out", out.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let claude = tree(&out.join(".claude"));
    let rules = String::from_utf8_lossy(&claude["CLAUDE.md"]).into_owned();
    assert!(rules.ends_with("# Work, internal\n\nInternal endpoints live in the wiki.\n"));
    let settings = String::from_utf8_lossy(&claude["settings.json"]).into_owned();
    // Arrays replace, so the private patch's list wins; other keys stay.
    assert!(settings.contains("Bash(make deploy)") && !settings.contains("Bash(cargo test)"));
    assert!(settings.contains("\"includeCoAuthoredBy\": false") && settings.contains("INTERNAL"));
}

#[test]
fn a_block_in_two_roots_exits_2() {
    let tmp = Tmp::new("dup");
    let out = tmp.out();
    let o = bree(
        &tmp,
        &["shared", "duplicate"],
        &["-p", "work", "render", "--out", out.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).starts_with("bree: rules block `core` is defined twice:"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_second_render_reports_everything_current() {
    let tmp = Tmp::new("again");
    assert!(render_out(&tmp, "work", &[]).status.success());
    let o = render_out(&tmp, "work", &[]);
    assert!(o.status.success());
    let text = stdout(&o);
    assert!(
        text.ends_with("bree: 0 written, 20 current, 0 removed\n"),
        "{text}"
    );
    assert!(
        text.lines()
            .filter(|l| !l.starts_with("bree:"))
            .all(|l| l.ends_with("current")),
        "{text}"
    );
}

#[test]
fn a_file_no_block_produces_any_more_is_removed() {
    let tmp = Tmp::new("removed");
    let o = render_out(&tmp, "home", &["--add", "files.pi=work"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let models = tmp.out().join(".pi/agent/models.json");
    assert!(models.is_file());
    std::fs::write(tmp.out().join(".pi/agent/sessions.log"), "mine\n").unwrap();

    let o = render_out(&tmp, "home", &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(
        text.contains("models.json") && text.contains("removed"),
        "{text}"
    );
    assert!(text.ends_with(", 1 removed\n"), "{text}");
    assert!(!models.exists());
    assert!(
        !tmp.out()
            .join(".local/state/bree/rendered/pi/models.json")
            .exists(),
        "the rendered copy goes too"
    );
    assert!(
        tmp.out().join(".pi/agent/sessions.log").is_file(),
        "files bree never wrote stay"
    );
}

#[test]
fn render_without_out_writes_the_state_directory() {
    let tmp = Tmp::new("state");
    let o = bree(&tmp, &["shared"], &["-p", "home", "render"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let state = tmp.0.join("home/.local/share/bree/state/home");
    assert!(
        stdout(&o).contains(&format!("{}/.claude/CLAUDE.md", state.display())),
        "{}",
        stdout(&o)
    );
    assert_eq!(
        as_alice(&tmp, tree(&state)),
        tree(&fixtures().join("render/home"))
    );
}

#[test]
fn bad_add_and_var_exit_2() {
    let tmp = Tmp::new("badadd");
    let out = tmp.out();
    let out = out.to_str().unwrap();
    for (flag, value) in [
        ("--add", "rules"),
        ("--add", "bogus=x"),
        ("--var", "novalue"),
    ] {
        let o = bree(
            &tmp,
            &["shared"],
            &["-p", "work", "render", "--out", out, flag, value],
        );
        assert_eq!(o.status.code(), Some(2), "{flag} {value}");
        assert!(
            stderr(&o).starts_with(&format!("bree: {flag} {value}:")),
            "{}",
            stderr(&o)
        );
    }
}

fn check_out(tmp: &Tmp, profile: &str, extra: &[&str]) -> Output {
    let out = tmp.out();
    let mut args = vec!["-p", profile, "check", "--out", out.to_str().unwrap()];
    args.extend_from_slice(extra);
    bree(tmp, &["shared"], &args)
}

#[test]
fn check_right_after_render_exits_0() {
    let tmp = Tmp::new("check-ok");
    assert!(render_out(&tmp, "work", &[]).status.success());
    let o = check_out(&tmp, "work", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(stdout(&o), "bree: 0 of 20 files not current\n");
}

#[test]
fn check_reports_a_modified_file_and_exits_1() {
    let tmp = Tmp::new("check-modified");
    assert!(render_out(&tmp, "work", &[]).status.success());
    let file = tmp.out().join(".claude/CLAUDE.md");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("added by hand\n");
    std::fs::write(&file, text).unwrap();
    let o = check_out(&tmp, "work", &[]);
    assert_eq!(o.status.code(), Some(1));
    let text = stdout(&o);
    let line = text.lines().next().unwrap();
    assert!(line.starts_with("claude  "), "{text}");
    assert!(
        line.contains(".claude/CLAUDE.md")
            && line.contains("modified  edited since bree rendered it"),
        "{text}"
    );
    assert!(
        text.ends_with("bree: 1 of 20 files not current\n"),
        "{text}"
    );

    let o = check_out(&tmp, "work", &["--diff"]);
    assert!(stdout(&o).contains("-added by hand\n"), "{}", stdout(&o));
}

#[test]
fn check_reports_a_deleted_file_as_missing() {
    let tmp = Tmp::new("check-missing");
    assert!(render_out(&tmp, "work", &[]).status.success());
    std::fs::remove_file(tmp.out().join(".config/opencode/opencode.json")).unwrap();
    let o = check_out(&tmp, "work", &[]);
    assert_eq!(o.status.code(), Some(1));
    let text = stdout(&o);
    assert!(
        text.contains("opencode.json") && text.contains("missing  not on disk"),
        "{text}"
    );
}

#[test]
fn check_with_a_changed_block_says_stale() {
    let tmp = Tmp::new("check-stale");
    assert!(render_out(&tmp, "work", &[]).status.success());
    // The same tree checked with an extra rules block is stale, not modified.
    let o = check_out(&tmp, "work", &["--add", "rules=home"]);
    assert_eq!(o.status.code(), Some(1));
    let text = stdout(&o);
    assert!(text.contains("stale"), "{text}");
    assert!(text.contains("render differs, +"), "{text}");
}

#[test]
fn check_against_the_container_state_dir_reads_it_without_writing() {
    let tmp = Tmp::new("check-container");
    let o = bree(&tmp, &["shared"], &["-p", "home", "check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stdout(&o).contains(".local/share/bree/state/home/.claude/CLAUDE.md"),
        "{}",
        stdout(&o)
    );
    assert!(stdout(&o).contains("missing"));
    assert!(!tmp.0.join("home/.local").exists());
}

/// The parsed `.claude/settings.json` under a render's base directory.
fn claude_settings(base: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(base.join(".claude/settings.json")).unwrap()).unwrap()
}

#[test]
fn the_target_decides_whether_settings_carry_the_auto_mode_seed() {
    let tmp = Tmp::new("auto-mode");
    let container = tmp.out();
    let host = tmp.0.join("host-out");
    let (container_arg, host_arg) = (container.to_str().unwrap(), host.to_str().unwrap());
    // `--out` does not change the target: the default is still the container.
    let o = bree(
        &tmp,
        &["shared"],
        &["-p", "home", "render", "--out", container_arg],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let args = [
        "-p", "home", "render", "--target", "host", "--out", host_arg,
    ];
    let o = bree(&tmp, &["shared"], &args);
    assert!(o.status.success(), "{}", stderr(&o));

    let seeded = claude_settings(&container);
    assert_eq!(seeded["skipAutoPermissionPrompt"], true);
    let list = seeded["autoMode"]["environment"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{list:?}");
    assert_eq!(list[0], "$defaults");
    let entry = list[1].as_str().unwrap();
    assert!(entry.contains("for the bree profile home."), "{entry}");
    // Its paths are those of the HOME bree was given, not a fixed one.
    let src = tmp.0.join("home/src").display().to_string();
    let mounted = format!("The host path {src} is mounted at {src}, read-write.");
    assert!(entry.contains(&mounted), "{entry}");

    // The host target's file is the container's without the two seeded keys.
    let mut plain = seeded.clone();
    let keys = plain.as_object_mut().unwrap();
    assert!(keys.shift_remove("skipAutoPermissionPrompt").is_some());
    assert!(keys.shift_remove("autoMode").is_some());
    assert_eq!(claude_settings(&host), plain);

    // `check` asks the same target as `render`, with or without `--out`.
    let check = |target: &str, out: &str| {
        let args = ["-p", "home", "check", "--target", target, "--out", out];
        bree(&tmp, &["shared"], &args)
    };
    for (target, out) in [("container", container_arg), ("host", host_arg)] {
        let o = check(target, out);
        assert_eq!(o.status.code(), Some(0), "{target}: {}", stdout(&o));
        assert_eq!(stdout(&o), "bree: 0 of 15 files not current\n");
    }
    for (target, out) in [("host", container_arg), ("container", host_arg)] {
        let o = check(target, out);
        assert_eq!(o.status.code(), Some(1), "{target}: {}", stdout(&o));
        let text = stdout(&o);
        assert!(
            text.lines()
                .next()
                .unwrap()
                .contains(".claude/settings.json")
                && text.contains("stale"),
            "{text}"
        );
        assert!(
            text.ends_with("bree: 1 of 15 files not current\n"),
            "{text}"
        );
    }
}

#[test]
fn profile_contents_lists_the_unused_command() {
    let tmp = Tmp::new("contents");
    let o = bree(
        &tmp,
        &["shared"],
        &["-p", "work", "profile", "contents", "--harness", "claude"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(text.starts_with("claude  ~/.claude\n"), "{text}");
    assert!(
        text.contains("  unused    commands/release-notes  "),
        "{text}"
    );
    assert!(text.contains("  settings  settings.json  "), "{text}");
    // The seeds of the container target come first, then the blocks.
    assert!(
        text.contains("  auto mode (bree) < tiers.yaml < work\n"),
        "{text}"
    );
    assert!(text.ends_with("3 unused\n"), "{text}");
    assert!(
        !text.contains("pi  "),
        "--harness limits the output: {text}"
    );
    assert!(!tmp.0.join("home/.local").exists(), "nothing is written");

    let o = bree(
        &tmp,
        &["shared"],
        &[
            "-p",
            "work",
            "profile",
            "contents",
            "--json",
            "--add",
            "commands=release-notes",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["harnesses"].as_array().unwrap().len(), 3);
    assert_eq!(
        v["harnesses"][0]["settings"][0],
        serde_json::json!({"file": "settings.json", "block": "auto mode (bree)", "root": "shared"})
    );
    let unused = &v["harnesses"][0]["unused"];
    assert!(
        !unused.to_string().contains("release-notes"),
        "--add uses the block: {unused}"
    );
}
