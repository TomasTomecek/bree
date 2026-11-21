//! `bree render --target host` and `bree check --target host` through the
//! built binary: the managed-copy rule on the home's own harness directories,
//! the state in `~/.local/state/bree/host.json` and the one-time import of
//! the shared config repository's `deployed.json`. HOME is a temporary directory, as in
//! tests/render.rs, and the profile is the fixtures' `home` (claude and pi).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A temp directory with an empty `home`, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("bree-host-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Tmp(dir)
    }

    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    /// A file under the home, as text.
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.home().join(path)).unwrap()
    }

    fn write(&self, path: &str, text: &str) {
        let p = self.home().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn exists(&self, path: &str) -> bool {
        self.home().join(path).exists()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bree(tmp: &Tmp, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bree"))
        .arg("--root")
        .arg(fixtures().join("roots/shared"))
        .args(["-p", "home"])
        .args(args)
        .current_dir(&tmp.0)
        .env_clear()
        .env("HOME", tmp.home())
        .env("USER", "alice")
        .output()
        .expect("run bree")
}

fn render(tmp: &Tmp, extra: &[&str]) -> Output {
    let mut args = vec!["render", "--target", "host"];
    args.extend_from_slice(extra);
    bree(tmp, &args)
}

fn check(tmp: &Tmp, extra: &[&str]) -> Output {
    let mut args = vec!["check", "--target", "host"];
    args.extend_from_slice(extra);
    bree(tmp, &args)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Every file under `dir` by relative path, bree's bookkeeping included.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn go(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                go(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(dir, dir, &mut out);
    out
}

/// The files of the home without bree's bookkeeping.
fn harness_files(home: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut all = tree(home);
    all.retain(|path, _| !path.starts_with(".local/"));
    all
}

/// A fixture tree's `.claude/settings.json` without the auto mode seed. The
/// trees in `tests/fixtures/render` are the container target's, and the host
/// target never writes the seed (design, "What the renderer writes").
fn without_auto_mode(mut files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let settings = files.get_mut(".claude/settings.json").unwrap();
    let mut value: Value = serde_json::from_slice(settings).unwrap();
    let keys = value.as_object_mut().unwrap();
    assert!(keys.shift_remove("skipAutoPermissionPrompt").is_some());
    assert!(keys.shift_remove("autoMode").is_some());
    let mut text = serde_json::to_string_pretty(&value).unwrap();
    text.push('\n');
    *settings = text.into_bytes();
    files
}

/// The rows of a render or check: the lines before the summary.
fn table(o: &Output) -> Vec<String> {
    let text = stdout(o);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    assert!(
        lines.last().is_some_and(|l| l.starts_with("bree: ")),
        "no summary line: {text}"
    );
    lines.pop();
    lines
}

/// The row that shows `path`, which must be there once.
fn row<'a>(rows: &'a [String], path: &str) -> &'a str {
    let found: Vec<_> = rows.iter().filter(|r| r.contains(path)).collect();
    assert_eq!(found.len(), 1, "{path} in {rows:?}");
    found[0]
}

fn sha1(bytes: &[u8]) -> String {
    Sha1::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const CONFLICT_TAIL: &str = "nothing written; take bree's version from \
     ~/.local/state/bree/pending/ or move your change into the blocks, \
     then render again (--force overwrites)\n";

// ------------------------------------------------------------ a clean host

#[test]
fn a_clean_render_installs_the_files_and_the_state_and_a_second_one_is_current() {
    let tmp = Tmp::new("clean");
    // Files of the harnesses that bree did not write stay as they are.
    tmp.write(".claude/.credentials.json", "secret\n");
    tmp.write(".claude.json", "{}\n");
    tmp.write(".pi/agent/sessions/s1.jsonl", "line\n");
    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}", stderr(&o));

    let want = without_auto_mode(harness_files(&fixtures().join("render/home")));
    let mut got = harness_files(&tmp.home());
    assert!(!tmp.read(".claude/settings.json").contains("autoMode"));
    for kept in [
        ".claude/.credentials.json",
        ".claude.json",
        ".pi/agent/sessions/s1.jsonl",
    ] {
        assert!(got.remove(kept).is_some(), "{kept}");
    }
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
    for (path, bytes) in &want {
        assert_eq!(
            String::from_utf8_lossy(&got[path]),
            String::from_utf8_lossy(bytes),
            "{path}"
        );
    }
    let text = stdout(&o);
    assert!(
        text.ends_with(&format!(
            "bree: {} written, 0 current, 0 removed\n",
            want.len()
        )),
        "{text}"
    );
    let rows = table(&o);
    assert!(
        row(&rows, "~/.claude/CLAUDE.md").starts_with("claude  ~/.claude/CLAUDE.md"),
        "{rows:?}"
    );
    assert!(row(&rows, "~/.claude/CLAUDE.md").ends_with("installed"));
    assert!(row(&rows, "~/.pi/agent/AGENTS.md").starts_with("pi      ~/.pi/agent/AGENTS.md"));
    let mode = fs::metadata(tmp.home().join(".claude/statusline.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);

    // The state is host.json, and the harness directories get no manifest.
    let state: Value = serde_json::from_str(&tmp.read(".local/state/bree/host.json")).unwrap();
    assert_eq!(state["bree"], env!("CARGO_PKG_VERSION"));
    let names: Vec<_> = state["harnesses"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["claude", "pi"]);
    assert_eq!(state["harnesses"]["claude"]["profile"], "home");
    assert_eq!(state["harnesses"]["pi"]["harness"], "pi");
    assert!(tmp.exists(".local/state/bree/rendered/claude/CLAUDE.md"));
    assert!(tmp.exists(".local/state/bree/rendered/pi/AGENTS.md"));
    assert!(!tmp.exists(".claude/.bree-manifest.json"));
    assert!(!tmp.exists(".pi/agent/.bree-manifest.json"));
    assert!(!tmp.exists(".local/state/bree/pending"));

    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(
        text.ends_with(&format!(
            "bree: 0 written, {} current, 0 removed\n",
            want.len()
        )),
        "{text}"
    );
    assert!(table(&o).iter().all(|r| r.ends_with("current")), "{text}");
    for kept in [".claude/.credentials.json", ".claude.json"] {
        assert!(tmp.exists(kept), "{kept}");
    }
}

// --------------------------------------------------------------- conflicts

#[test]
fn an_edited_file_refuses_the_render_and_nothing_else_is_written() {
    let tmp = Tmp::new("conflict");
    assert!(render(&tmp, &[]).status.success());
    let mut edited = tmp.read(".claude/CLAUDE.md");
    edited.push_str("added by hand\n");
    tmp.write(".claude/CLAUDE.md", &edited);
    let before = tree(&tmp.home());

    // owner=bob changes four files of each harness, CLAUDE.md among them.
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!(
            "claude  ~/.claude/CLAUDE.md  conflict  diff -u ~/.claude/CLAUDE.md \
             ~/.local/state/bree/pending/.claude/CLAUDE.md\n\
             bree: 1 conflict, {CONFLICT_TAIL}"
        )
    );

    // The only change is bree's version of the file in conflict, home-relative.
    let mut after = tree(&tmp.home());
    let pending = after
        .remove(".local/state/bree/pending/.claude/CLAUDE.md")
        .expect("pending file");
    let pending = String::from_utf8(pending).unwrap();
    assert!(pending.contains("owned by bob") && !pending.contains("added by hand"));
    assert_eq!(after, before, "no other file changed");
    assert_eq!(tmp.read(".claude/CLAUDE.md"), edited);
    assert!(
        tmp.read(".claude/settings.json")
            .contains("\"OWNER\": \"alice\"")
    );
    assert!(tmp.read(".pi/agent/AGENTS.md").contains("owned by alice"));
}

#[test]
fn force_overwrites_the_conflict_and_clears_the_pending_directory() {
    let tmp = Tmp::new("force");
    assert!(render(&tmp, &[]).status.success());
    tmp.write(".claude/CLAUDE.md", "mine\n");
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(tmp.exists(".local/state/bree/pending/.claude/CLAUDE.md"));

    let o = render(&tmp, &["--var", "owner=bob", "--force"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let rows = table(&o);
    assert!(
        row(&rows, "~/.claude/CLAUDE.md").ends_with("updated"),
        "{rows:?}"
    );
    assert!(tmp.read(".claude/CLAUDE.md").contains("owned by bob"));
    assert!(!tmp.exists(".local/state/bree/pending"));
    // The forced render is the new state: the next one is clean.
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert!(stdout(&o).contains(", 0 removed\n") && stdout(&o).contains("bree: 0 written,"));
}

#[test]
fn a_pending_directory_is_replaced_by_the_next_refusal_and_cleared_by_a_render() {
    let tmp = Tmp::new("pending");
    assert!(render(&tmp, &[]).status.success());
    tmp.write(".claude/CLAUDE.md", "mine\n");
    tmp.write(".claude/settings.json", "{\"mine\": true}\n");
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).contains("bree: 2 conflicts, "), "{}", stdout(&o));
    let pending = tree(&tmp.home().join(".local/state/bree/pending"));
    assert_eq!(
        pending.keys().collect::<Vec<_>>(),
        [".claude/CLAUDE.md", ".claude/settings.json"]
    );

    // The settings edit goes away; the next refusal has one file left.
    tmp.write(
        ".claude/settings.json",
        &tmp.read(".local/state/bree/rendered/claude/settings.json"),
    );
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert_eq!(o.status.code(), Some(1));
    let pending = tree(&tmp.home().join(".local/state/bree/pending"));
    assert_eq!(pending.keys().collect::<Vec<_>>(), [".claude/CLAUDE.md"]);

    // Taking bree's version by hand is how a conflict is resolved.
    fs::copy(
        tmp.home()
            .join(".local/state/bree/pending/.claude/CLAUDE.md"),
        tmp.home().join(".claude/CLAUDE.md"),
    )
    .unwrap();
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!tmp.exists(".local/state/bree/pending"));
}

#[test]
fn a_file_bree_never_wrote_that_differs_is_a_conflict_and_one_that_matches_is_not() {
    let tmp = Tmp::new("unrecorded");
    tmp.write(".claude/CLAUDE.md", "mine\n");
    let same =
        fs::read_to_string(fixtures().join("render/home/.pi/agent/APPEND_SYSTEM.md")).unwrap();
    tmp.write(".pi/agent/APPEND_SYSTEM.md", &same);

    let o = render(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!(
            "claude  ~/.claude/CLAUDE.md  conflict  diff -u ~/.claude/CLAUDE.md \
             ~/.local/state/bree/pending/.claude/CLAUDE.md\n\
             bree: 1 conflict, {CONFLICT_TAIL}"
        )
    );
    // Nothing else was installed, and there is no state yet.
    assert_eq!(
        harness_files(&tmp.home()).keys().collect::<Vec<_>>(),
        [".claude/CLAUDE.md", ".pi/agent/APPEND_SYSTEM.md"]
    );
    assert!(!tmp.exists(".local/state/bree/host.json"));
    assert_eq!(tmp.read(".claude/CLAUDE.md"), "mine\n");

    let o = render(&tmp, &["--force"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let rows = table(&o);
    assert!(row(&rows, "~/.claude/CLAUDE.md").ends_with("updated"));
    assert!(row(&rows, "~/.pi/agent/APPEND_SYSTEM.md").ends_with("current"));
}

// ------------------------------------------------------------ extra files

#[test]
fn a_file_no_block_produces_any_more_is_removed_unless_it_was_edited() {
    let tmp = Tmp::new("extra");
    let o = render(&tmp, &["--add", "files.pi=work"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(tmp.exists(".pi/agent/models.json"));
    tmp.write(".pi/agent/sessions.log", "mine\n");

    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let rows = table(&o);
    let r = row(&rows, "~/.pi/agent/models.json");
    assert!(r.starts_with("pi      ") && r.ends_with("removed"), "{r}");
    assert!(stdout(&o).ends_with(", 1 removed\n"), "{}", stdout(&o));
    assert!(!tmp.exists(".pi/agent/models.json"));
    assert!(!tmp.exists(".local/state/bree/rendered/pi/models.json"));
    assert_eq!(tmp.read(".pi/agent/sessions.log"), "mine\n");
    let state: Value = serde_json::from_str(&tmp.read(".local/state/bree/host.json")).unwrap();
    assert!(!state.to_string().contains("models.json"));

    // An edited one is a conflict, and --force removes it.
    assert!(render(&tmp, &["--add", "files.pi=work"]).status.success());
    tmp.write(".pi/agent/models.json", "{\"mine\": true}\n");
    let o = render(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!(
            "pi  ~/.pi/agent/models.json  conflict  edited, and no block produces it any more\n\
             bree: 1 conflict, {CONFLICT_TAIL}"
        )
    );
    assert_eq!(tmp.read(".pi/agent/models.json"), "{\"mine\": true}\n");
    assert!(
        !tmp.exists(".local/state/bree/pending/.pi"),
        "bree has no version to save"
    );

    let o = render(&tmp, &["--force"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(row(&table(&o), "~/.pi/agent/models.json").ends_with("removed"));
    assert!(!tmp.exists(".pi/agent/models.json"));
    assert_eq!(tmp.read(".pi/agent/sessions.log"), "mine\n");
}

#[test]
fn a_recorded_file_that_is_already_gone_leaves_the_state_without_a_row() {
    let tmp = Tmp::new("gone");
    assert!(render(&tmp, &["--add", "files.pi=work"]).status.success());
    fs::remove_file(tmp.home().join(".pi/agent/models.json")).unwrap();
    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!stdout(&o).contains("models.json"), "{}", stdout(&o));
    assert!(stdout(&o).ends_with(", 0 removed\n"));
    assert!(!tmp.exists(".local/state/bree/rendered/pi/models.json"));
    let state: Value = serde_json::from_str(&tmp.read(".local/state/bree/host.json")).unwrap();
    assert!(!state.to_string().contains("models.json"));
}

#[test]
fn rendering_one_harness_keeps_the_state_of_the_other() {
    let tmp = Tmp::new("harness");
    assert!(render(&tmp, &[]).status.success());
    let before: Value = serde_json::from_str(&tmp.read(".local/state/bree/host.json")).unwrap();

    let o = render(&tmp, &["--harness", "claude", "--var", "owner=bob"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        table(&o).iter().all(|r| r.starts_with("claude  ")),
        "{}",
        stdout(&o)
    );
    assert!(tmp.read(".pi/agent/AGENTS.md").contains("owned by alice"));
    let after: Value = serde_json::from_str(&tmp.read(".local/state/bree/host.json")).unwrap();
    assert_eq!(after["harnesses"]["pi"], before["harnesses"]["pi"]);
    assert_ne!(after["harnesses"]["claude"], before["harnesses"]["claude"]);

    // The other harness still updates cleanly from its own state.
    let o = render(&tmp, &["--var", "owner=bob"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(tmp.read(".pi/agent/AGENTS.md").contains("owned by bob"));
}

// ------------------------------------------------------ the role's import

const ROLE_SETTINGS: &str =
    "{\n  \"model\": \"sonnet\",\n  \"env\": {\"NOTE\": \"caf\u{e9}\"}\n}\n";

/// A home where the role has run: it wrote CLAUDE.md, settings.json (in its
/// own formatting, with a non-ASCII value) and pi's AGENTS.md, and recorded
/// the sha1 of each. Returns the text of its `deployed.json`.
fn role_home(tmp: &Tmp) -> String {
    tmp.write(".claude/CLAUDE.md", "role rules\n");
    tmp.write(".claude/settings.json", ROLE_SETTINGS);
    tmp.write(".pi/agent/AGENTS.md", "role pi rules\n");
    // What Python's json.dumps(value, sort_keys=True) makes of the settings.
    let canonical = "{\"env\": {\"NOTE\": \"caf\\u00e9\"}, \"model\": \"sonnet\"}";
    let deployed = json!({
        ".claude/CLAUDE.md": sha1(b"role rules\n"),
        ".claude/settings.json": sha1(canonical.as_bytes()),
        ".pi/agent/AGENTS.md": sha1(b"role pi rules\n"),
        ".config/other/file": sha1(b"not bree's directory\n"),
        ".claude/../outside": sha1(b"not inside the harness directory\n"),
    })
    .to_string();
    tmp.write(".local/state/ai-role/deployed.json", &deployed);
    deployed
}

#[test]
fn the_roles_files_are_updated_on_the_first_render_without_a_conflict() {
    let tmp = Tmp::new("import");
    let deployed = role_home(&tmp);
    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}{}", stdout(&o), stderr(&o));
    let rows = table(&o);
    for path in [
        "~/.claude/CLAUDE.md",
        "~/.claude/settings.json",
        "~/.pi/agent/AGENTS.md",
    ] {
        assert!(row(&rows, path).ends_with("updated"), "{rows:?}");
    }
    assert!(row(&rows, "~/.claude/statusline.sh").ends_with("installed"));
    assert!(tmp.read(".claude/CLAUDE.md").contains("owned by alice"));

    // The role's file is still there, unchanged, and bree has its own state.
    assert_eq!(tmp.read(".local/state/ai-role/deployed.json"), deployed);
    assert!(tmp.exists(".local/state/bree/host.json"));
    assert!(!tmp.exists(".claude/.bree-manifest.json"));
    let o = render(&tmp, &[]);
    assert!(
        stdout(&o).starts_with("claude  ~/.claude/CLAUDE.md"),
        "{}",
        stdout(&o)
    );
    assert!(table(&o).iter().all(|r| r.ends_with("current")));
}

#[test]
fn a_role_file_edited_after_the_role_wrote_it_is_a_conflict() {
    let tmp = Tmp::new("import-edited");
    let deployed = role_home(&tmp);
    tmp.write(".claude/CLAUDE.md", "role rules\nand my own line\n");
    // A JSON file whose value changed does not match its canonical hash.
    tmp.write(".claude/settings.json", "{\"model\": \"opus\"}\n");
    let before = tree(&tmp.home());

    let o = render(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let text = stdout(&o);
    assert!(
        text.contains("bree: 2 conflicts, nothing written;"),
        "{text}"
    );
    for path in ["~/.claude/CLAUDE.md", "~/.claude/settings.json"] {
        assert!(
            row(&table(&o), path).contains("conflict  diff -u"),
            "{text}"
        );
    }
    // The role's third file is not in the conflicts.
    assert!(!text.contains("AGENTS.md"), "{text}");
    let mut after = tree(&tmp.home());
    after.retain(|path, _| !path.starts_with(".local/state/bree/pending/"));
    assert_eq!(after, before, "only the pending files were written");
    assert_eq!(tmp.read(".local/state/ai-role/deployed.json"), deployed);
    assert!(!tmp.exists(".local/state/bree/host.json"));
}

#[test]
fn deployed_json_is_never_read_again_once_host_json_exists() {
    let tmp = Tmp::new("import-once");
    role_home(&tmp);
    assert!(render(&tmp, &[]).status.success());

    // Valid, but every hash is wrong: nothing reads it, so nothing conflicts.
    let wrong = json!({
        ".claude/CLAUDE.md": "0".repeat(40),
        ".claude/settings.json": "0".repeat(40),
        ".pi/agent/AGENTS.md": "0".repeat(40),
    })
    .to_string();
    tmp.write(".local/state/ai-role/deployed.json", &wrong);
    let o = render(&tmp, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("bree: 0 written, "), "{}", stdout(&o));
    assert!(table(&o).iter().all(|r| r.ends_with("current")));
    assert_eq!(check(&tmp, &[]).status.code(), Some(0));

    // Not even valid JSON: still not read, and still not touched.
    tmp.write(".local/state/ai-role/deployed.json", "not json at all");
    assert!(render(&tmp, &[]).status.success());
    assert_eq!(check(&tmp, &[]).status.code(), Some(0));
    assert_eq!(
        tmp.read(".local/state/ai-role/deployed.json"),
        "not json at all"
    );
}

#[test]
fn a_first_render_of_one_harness_on_a_role_home_exits_2() {
    let tmp = Tmp::new("import-partial");
    role_home(&tmp);
    let before = tree(&tmp.home());
    let o = render(&tmp, &["--harness", "claude"]);
    assert_eq!(o.status.code(), Some(2), "{}", stdout(&o));
    assert!(stderr(&o).contains("leaves out pi"), "{}", stderr(&o));
    assert_eq!(tree(&tmp.home()), before, "nothing was written");
    // Every harness at once goes through; after that `--harness` is fine.
    assert!(render(&tmp, &[]).status.success());
    assert!(render(&tmp, &["--harness", "claude"]).status.success());
}

#[test]
fn a_first_render_without_a_harness_the_role_recorded_exits_2() {
    let tmp = Tmp::new("import-unrendered");
    role_home(&tmp);
    // The role also wrote opencode's settings, and the profile has no opencode.
    tmp.write(".config/opencode/opencode.json", "{}\n");
    let deployed = json!({
        ".claude/CLAUDE.md": sha1(b"role rules\n"),
        ".pi/agent/AGENTS.md": sha1(b"role pi rules\n"),
        ".config/opencode/opencode.json": sha1(b"{}"),
    })
    .to_string();
    tmp.write(".local/state/ai-role/deployed.json", &deployed);
    let before = tree(&tmp.home());
    let o = render(&tmp, &[]);
    assert_eq!(o.status.code(), Some(2), "{}", stdout(&o));
    assert!(
        stderr(&o).contains("leaves out opencode (add each of them to the profile's harnesses)"),
        "{}",
        stderr(&o)
    );
    assert_eq!(tree(&tmp.home()), before, "nothing was written");
    // check writes nothing anyway, so it still reports.
    assert_ne!(check(&tmp, &[]).status.code(), Some(2));
}

#[test]
fn a_deployed_json_that_is_not_an_object_of_strings_exits_2_naming_the_file() {
    let tmp = Tmp::new("import-bad");
    let path = tmp.home().join(".local/state/ai-role/deployed.json");
    for text in ["{not json", "[\"a\"]", "{\".claude/CLAUDE.md\": 3}"] {
        tmp.write(".local/state/ai-role/deployed.json", text);
        for o in [render(&tmp, &[]), check(&tmp, &[])] {
            assert_eq!(o.status.code(), Some(2), "{text}: {}", stdout(&o));
            assert!(
                stderr(&o).starts_with(&format!("bree: {}: ", path.display())),
                "{text}: {}",
                stderr(&o)
            );
            assert_eq!(stdout(&o), "");
        }
        assert_eq!(tmp.read(".local/state/ai-role/deployed.json"), text);
        assert!(!tmp.exists(".claude"), "nothing was installed");
        assert!(!tmp.exists(".local/state/bree"));
    }
}

#[test]
fn a_host_json_that_does_not_parse_exits_2_naming_the_file() {
    let tmp = Tmp::new("host-json-bad");
    tmp.write(".local/state/bree/host.json", "{nope");
    let o = render(&tmp, &[]);
    assert_eq!(o.status.code(), Some(2));
    let path = tmp.home().join(".local/state/bree/host.json");
    assert!(
        stderr(&o).starts_with(&format!("bree: {}: ", path.display())),
        "{}",
        stderr(&o)
    );
    assert!(!tmp.exists(".claude"));
}

// ------------------------------------------------------------------- check

#[test]
fn check_is_clean_after_a_render_and_reports_an_edited_file_as_modified() {
    let tmp = Tmp::new("check");
    let n = harness_files(&fixtures().join("render/home")).len();
    assert!(render(&tmp, &[]).status.success());
    let o = check(&tmp, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(stdout(&o), format!("bree: 0 of {n} files not current\n"));

    tmp.write(".claude/CLAUDE.md", "mine\n");
    let o = check(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1));
    let rows = table(&o);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].starts_with("claude  ~/.claude/CLAUDE.md")
            && rows[0].ends_with("modified  edited since bree rendered it"),
        "{rows:?}"
    );
    assert!(
        stdout(&o).ends_with(&format!("bree: 1 of {n} files not current\n")),
        "{}",
        stdout(&o)
    );

    let o = check(&tmp, &["--diff"]);
    assert!(
        stdout(&o).contains("--- a/.claude/CLAUDE.md\n+++ b/.claude/CLAUDE.md\n"),
        "{}",
        stdout(&o)
    );
    assert!(stdout(&o).contains("-mine\n"), "{}", stdout(&o));
}

#[test]
fn check_reports_stale_missing_and_extra_files() {
    let tmp = Tmp::new("check-states");
    assert!(render(&tmp, &["--add", "files.pi=work"]).status.success());
    fs::remove_file(tmp.home().join(".claude/commands/review-text.md")).unwrap();
    let o = check(&tmp, &["--var", "owner=bob"]);
    assert_eq!(o.status.code(), Some(1));
    let rows = table(&o);
    // The padding depends on the widest state of the table.
    let squash = |r: &str| r.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        squash(row(&rows, "~/.pi/agent/models.json")),
        "pi ~/.pi/agent/models.json extra no block produces it any more"
    );
    assert_eq!(
        squash(row(&rows, "~/.claude/commands/review-text.md")),
        "claude ~/.claude/commands/review-text.md missing not on disk"
    );
    assert_eq!(
        squash(row(&rows, "~/.claude/CLAUDE.md")),
        "claude ~/.claude/CLAUDE.md stale render differs, +1 -1 lines"
    );
}

#[test]
fn check_writes_nothing_and_never_creates_the_bookkeeping_directory() {
    let tmp = Tmp::new("check-readonly");
    // On a home that was never rendered, every file is missing.
    let o = check(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        table(&o).iter().all(|r| r.contains("missing")),
        "{}",
        stdout(&o)
    );
    assert!(tree(&tmp.home()).is_empty(), "nothing was created");

    // Nor does it change the state of a rendered one, however much differs.
    assert!(render(&tmp, &["--add", "files.pi=work"]).status.success());
    tmp.write(".claude/CLAUDE.md", "mine\n");
    fs::remove_file(tmp.home().join(".pi/agent/APPEND_SYSTEM.md")).unwrap();
    let before = tree(&tmp.home());
    for extra in [&["--var", "owner=bob"][..], &["--diff"][..]] {
        let o = check(&tmp, extra);
        assert_eq!(o.status.code(), Some(1));
        assert_eq!(
            tree(&tmp.home()),
            before,
            "home, host.json and copies as they were"
        );
    }
}

#[test]
fn check_and_render_agree_on_a_conflict() {
    let tmp = Tmp::new("check-agree");
    assert!(render(&tmp, &[]).status.success());
    tmp.write(".claude/settings.json", "{\"model\": \"mine\"}\n");
    let o = check(&tmp, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(row(&table(&o), "~/.claude/settings.json").contains("modified"));
    assert_eq!(render(&tmp, &[]).status.code(), Some(1));
    tmp.write(
        ".claude/settings.json",
        &tmp.read(".local/state/bree/rendered/claude/settings.json"),
    );
    assert_eq!(check(&tmp, &[]).status.code(), Some(0));
    assert_eq!(render(&tmp, &[]).status.code(), Some(0));
}

// ---------------------------------------------- --out and --force elsewhere

#[test]
fn host_target_with_out_keeps_the_plain_directory_semantics() {
    let tmp = Tmp::new("out");
    let out = tmp.0.join("out");
    let out = out.to_str().unwrap();
    let o = bree(&tmp, &["render", "--target", "host", "--out", out]);
    assert!(o.status.success(), "{}", stderr(&o));
    // Container semantics: a manifest per harness directory, no host.json.
    assert!(tmp.0.join("out/.claude/.bree-manifest.json").is_file());
    assert!(tmp.0.join("out/.pi/agent/.bree-manifest.json").is_file());
    assert!(!tmp.0.join("out/.local/state/bree/host.json").exists());
    assert!(!tmp.home().join(".local").exists());
    assert!(
        stdout(&o).contains(&format!("{out}/.claude/CLAUDE.md")),
        "{}",
        stdout(&o)
    );

    // An edit is overwritten, not a conflict, with or without --force.
    fs::write(tmp.0.join("out/.claude/CLAUDE.md"), "mine\n").unwrap();
    let o = bree(
        &tmp,
        &["render", "--target", "host", "--out", out, "--force"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(row(&table(&o), "/.claude/CLAUDE.md").ends_with("updated"));
    fs::write(tmp.0.join("out/.claude/CLAUDE.md"), "mine\n").unwrap();
    let o = bree(&tmp, &["render", "--target", "host", "--out", out]);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = bree(&tmp, &["check", "--target", "host", "--out", out]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
}

#[test]
fn force_on_the_container_target_changes_nothing() {
    let tmp = Tmp::new("force-container");
    let o = bree(&tmp, &["render"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let state = tmp.home().join(".local/share/bree/state/home");
    fs::write(state.join(".claude/CLAUDE.md"), "edited in a session\n").unwrap();
    let plain = bree(&tmp, &["render"]);
    fs::write(state.join(".claude/CLAUDE.md"), "edited in a session\n").unwrap();
    let forced = bree(&tmp, &["render", "--force"]);
    assert!(plain.status.success() && forced.status.success());
    assert_eq!(stdout(&plain), stdout(&forced));
    assert!(
        tmp.home()
            .join(".local/share/bree/state/home/.claude/.bree-manifest.json")
            .is_file()
    );
    assert!(
        !tmp.exists(".local/state/bree"),
        "the host's bookkeeping is not touched"
    );
}
