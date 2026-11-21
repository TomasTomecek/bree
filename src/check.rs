//! `bree check` (docs/design.md, "How `check` detects stale output"): compares
//! an in-memory render with the files on disk and with the manifest of the
//! last render, and writes nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use crate::error::Result;
use crate::profile::{Harness, PerHarness};
use crate::render::{
    self, Render, canonical, digest, harness_dir, io_err, read_optional, rendered_copy,
};
use crate::yaml;

/// JSON Pointers per harness that `capture.yaml` lists as runtime churn.
pub type Ignore = PerHarness<Vec<String>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureFile {
    #[serde(default)]
    ignore: Ignore,
}

/// The `ignore` lists of every root's `capture.yaml`, added up.
pub fn load_ignore(roots: &[PathBuf]) -> Result<Ignore> {
    let mut all = Ignore::default();
    for root in roots {
        let path = root.join("capture.yaml");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        };
        let file = path.display().to_string();
        let capture: CaptureFile = yaml::from_str(&text, &file)?;
        for h in Harness::ALL {
            for pointer in capture.ignore.get(h) {
                if !pointer.starts_with('/') {
                    return Err(yaml::Error::new(
                        &file,
                        None,
                        &format!("ignore.{}", h.name()),
                        format!("`{pointer}` is not a JSON Pointer (it starts with `/`)"),
                    )
                    .into());
                }
                all.get_mut(h).push(pointer.clone());
            }
        }
    }
    Ok(all)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Missing,
    Modified,
    Stale { added: usize, removed: usize },
    Extra,
}

impl State {
    /// `Stale` with the line counts of the change from `disk` to `rendered`.
    pub fn stale(disk: &str, rendered: &str) -> State {
        let (added, removed) = line_counts(disk, rendered);
        State::Stale { added, removed }
    }

    fn name(self) -> &'static str {
        match self {
            State::Missing => "missing",
            State::Modified => "modified",
            State::Stale { .. } => "stale",
            State::Extra => "extra",
        }
    }

    fn detail(self) -> String {
        match self {
            State::Missing => "not on disk".to_string(),
            State::Modified => "edited since bree rendered it".to_string(),
            State::Stale { added, removed } => {
                format!("render differs, +{added} -{removed} lines")
            }
            State::Extra => "no block produces it any more".to_string(),
        }
    }
}

/// A file that is not current.
#[derive(Debug)]
pub struct Row {
    pub harness: Harness,
    pub path: String,
    pub state: State,
    /// Unified diff from the disk to the render, titled with the path under
    /// the base directory.
    pub diff: String,
}

impl Row {
    /// A row that shows the path `shown` and diffs `disk` to `rendered` under
    /// the title `title` (the path under the base directory).
    pub fn new(
        harness: Harness,
        shown: String,
        title: &str,
        state: State,
        disk: &str,
        rendered: &str,
    ) -> Row {
        Row {
            harness,
            path: shown,
            state,
            diff: unified(title, disk, rendered),
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub rows: Vec<Row>,
    /// Rendered files plus the extra ones.
    pub total: usize,
}

/// The file's content for comparing: a `.json` file that parses has the
/// ignored keys removed and its keys sorted, so key order does not count.
pub fn normalized(path: &str, bytes: &[u8], ignore: &[String]) -> Vec<u8> {
    let parsed = path
        .ends_with(".json")
        .then(|| serde_json::from_slice::<Value>(bytes).ok())
        .flatten();
    match parsed {
        Some(mut v) => {
            for pointer in ignore {
                remove_pointer(&mut v, pointer);
            }
            canonical(&v).to_string().into_bytes()
        }
        None => bytes.to_vec(),
    }
}

fn remove_pointer(v: &mut Value, pointer: &str) {
    let Some((parent, last)) = pointer.rsplit_once('/') else {
        return;
    };
    let key = last.replace("~1", "/").replace("~0", "~");
    match v.pointer_mut(parent) {
        Some(Value::Object(m)) => {
            m.shift_remove(&key);
        }
        Some(Value::Array(a)) => {
            if let Ok(i) = key.parse::<usize>() {
                if i < a.len() {
                    a.remove(i);
                }
            }
        }
        _ => {}
    }
}

/// Whether the live file differs from what an earlier render wrote: from
/// bree's copy of it when there is one, else from the recorded sha256. Key
/// order and the ignored keys do not count.
pub fn edited(
    path: &str,
    disk: &[u8],
    sha256: &str,
    copy: Option<&[u8]>,
    ignore: &[String],
) -> bool {
    match copy {
        Some(copy) => normalized(path, disk, ignore) != normalized(path, copy, ignore),
        None => digest(path, disk) != sha256,
    }
}

fn line_counts(disk: &str, rendered: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(disk, rendered);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    (count(ChangeTag::Insert), count(ChangeTag::Delete))
}

fn unified(shown: &str, disk: &str, rendered: &str) -> String {
    TextDiff::from_lines(disk, rendered)
        .unified_diff()
        .header(&format!("a/{shown}"), &format!("b/{shown}"))
        .to_string()
}

/// Compares the render with the files under `base`; the paths in the report
/// start with `base` as given.
pub fn check(render: &Render, base: &Path, ignore: &Ignore) -> Result<Report> {
    let mut rows = Vec::new();
    let mut total = 0;
    for h in &render.harnesses {
        let rel = harness_dir(h.harness);
        let dir = base.join(rel);
        let ignore = ignore.get(h.harness);
        let old = render::read_manifest(&dir)?.unwrap_or_default();
        let shown_path = |path: &str| base.join(rel).join(path).display().to_string();
        let mut row = |path: &str, state: State, disk: &str, rendered: &str| {
            let title = format!("{rel}/{path}");
            rows.push(Row::new(
                h.harness,
                shown_path(path),
                &title,
                state,
                disk,
                rendered,
            ));
        };
        for f in &h.files {
            total += 1;
            let rendered = String::from_utf8_lossy(&f.bytes);
            let Some(disk) = read_optional(&dir.join(&f.path))? else {
                row(&f.path, State::Missing, "", &rendered);
                continue;
            };
            let disk_text = String::from_utf8_lossy(&disk);
            let last = old.iter().find(|(p, _)| p == &f.path);
            let modified = match last {
                None => false,
                Some((_, sha)) => {
                    let copy = read_optional(&rendered_copy(base, h.harness, &f.path))?;
                    edited(&f.path, &disk, sha, copy.as_deref(), ignore)
                }
            };
            if modified {
                row(&f.path, State::Modified, &disk_text, &rendered);
            } else if normalized(&f.path, &disk, ignore) != normalized(&f.path, &f.bytes, ignore) {
                row(
                    &f.path,
                    State::stale(&disk_text, &rendered),
                    &disk_text,
                    &rendered,
                );
            }
        }
        for (path, _) in &old {
            if h.files.iter().any(|f| &f.path == path) {
                continue;
            }
            if let Some(disk) = read_optional(&dir.join(path))? {
                total += 1;
                row(path, State::Extra, &String::from_utf8_lossy(&disk), "");
            }
        }
    }
    Ok(Report { rows, total })
}

/// The report as text: one aligned line per file, each followed by its diff
/// when asked, then the summary.
pub fn format(report: &Report, diff: bool) -> String {
    let lines: Vec<Vec<String>> = report
        .rows
        .iter()
        .map(|r| {
            vec![
                r.harness.name().to_string(),
                r.path.clone(),
                r.state.name().to_string(),
                r.state.detail(),
            ]
        })
        .collect();
    let aligned = render::align(&lines);
    let mut out = String::new();
    for (line, row) in aligned.lines().zip(&report.rows) {
        out.push_str(line);
        out.push('\n');
        if diff {
            out.push_str(&row.diff);
        }
    }
    out.push_str(&format!(
        "bree: {} of {} files not current\n",
        report.rows.len(),
        report.total
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::{TempDir, env};
    use crate::render::{Options, write};

    /// A root with a rules block, a settings block and a command, a profile
    /// using them, and the render written under `out`.
    struct Fixture {
        t: TempDir,
        root: PathBuf,
        profile: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let t = TempDir::new(tag);
            let root = t.0.join("root");
            for (p, text) in [
                ("rules/a.md", "one\ntwo\n"),
                ("settings/claude/s.json", r#"{"model":"opus","keep":1}"#),
                ("commands/c.md", "cmd\n"),
            ] {
                let path = root.join(p);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
            let profile = t.0.join("p.yaml");
            fs::write(
                &profile,
                "harness: claude\nrules: [a]\ncommands: all\nsettings:\n  claude: [s]\n",
            )
            .unwrap();
            Fixture { t, root, profile }
        }

        fn render(&self, adds: &[&str]) -> Render {
            let p = crate::profile::load(&self.profile, "p", &env()).unwrap();
            let adds: Vec<String> = adds.iter().map(|s| s.to_string()).collect();
            let opts = Options {
                harnesses: &[],
                adds: &adds,
                vars: &[],
                container: false,
            };
            render::render(&p, &self.profile, std::slice::from_ref(&self.root), &opts).unwrap()
        }

        fn out(&self) -> PathBuf {
            self.t.0.join("out")
        }

        fn claude(&self, path: &str) -> PathBuf {
            self.out().join(".claude").join(path)
        }

        fn written(&self) -> Report {
            write(&self.render(&[]), &self.out(), &self.out()).unwrap();
            self.check(&Ignore::default())
        }

        fn check(&self, ignore: &Ignore) -> Report {
            check(&self.render(&[]), &self.out(), ignore).unwrap()
        }
    }

    fn states(r: &Report) -> Vec<(String, &'static str)> {
        r.rows
            .iter()
            .map(|r| {
                let p = r.path.rsplit_once("/.claude/").unwrap().1.to_string();
                (p, r.state.name())
            })
            .collect()
    }

    #[test]
    fn a_fresh_render_is_current() {
        let f = Fixture::new("current");
        let r = f.written();
        assert!(r.rows.is_empty());
        assert_eq!(r.total, 3);
        assert_eq!(format(&r, false), "bree: 0 of 3 files not current\n");
    }

    #[test]
    fn nothing_on_disk_is_all_missing() {
        let f = Fixture::new("missing");
        let r = f.check(&Ignore::default());
        assert_eq!(
            states(&r),
            [
                ("CLAUDE.md".to_string(), "missing"),
                ("commands/c.md".to_string(), "missing"),
                ("settings.json".to_string(), "missing"),
            ]
        );
        assert_eq!(r.rows[0].state.detail(), "not on disk");
    }

    #[test]
    fn a_changed_block_makes_the_file_stale_with_line_counts() {
        let f = Fixture::new("stale");
        f.written();
        fs::write(f.root.join("rules/a.md"), "one\n2\nthree\nfour\n").unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("CLAUDE.md".to_string(), "stale")]);
        assert_eq!(
            r.rows[0].state,
            State::Stale {
                added: 3,
                removed: 1
            }
        );
        let text = format(&r, false);
        assert!(
            text.contains("stale  render differs, +3 -1 lines"),
            "{text}"
        );
        assert!(text.ends_with("bree: 1 of 3 files not current\n"), "{text}");
    }

    #[test]
    fn an_edited_file_is_modified_even_when_a_block_changed_too() {
        let f = Fixture::new("modified");
        f.written();
        fs::write(f.claude("CLAUDE.md"), "one\nedited\n").unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("CLAUDE.md".to_string(), "modified")]);
        fs::write(f.root.join("rules/a.md"), "changed\n").unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("CLAUDE.md".to_string(), "modified")]);
        assert!(format(&r, false).contains("edited since bree rendered it"));
    }

    #[test]
    fn modified_falls_back_to_the_manifest_hash_without_the_copy() {
        let f = Fixture::new("hash");
        f.written();
        fs::remove_dir_all(f.out().join(".local")).unwrap();
        assert!(f.check(&Ignore::default()).rows.is_empty());
        fs::write(f.claude("commands/c.md"), "other\n").unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("commands/c.md".to_string(), "modified")]);
    }

    #[test]
    fn a_deleted_file_is_missing() {
        let f = Fixture::new("deleted");
        f.written();
        fs::remove_file(f.claude("commands/c.md")).unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("commands/c.md".to_string(), "missing")]);
    }

    #[test]
    fn a_file_no_block_produces_any_more_is_extra_until_it_is_gone() {
        let f = Fixture::new("extra");
        f.written();
        fs::remove_file(f.root.join("commands/c.md")).unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("commands/c.md".to_string(), "extra")]);
        assert_eq!(r.total, 3, "two rendered files and the extra one");
        assert!(format(&r, false).contains("no block produces it any more"));
        fs::remove_file(f.claude("commands/c.md")).unwrap();
        assert!(f.check(&Ignore::default()).rows.is_empty());
    }

    #[test]
    fn the_container_seed_is_part_of_the_render_so_a_changed_profile_makes_settings_stale() {
        let f = Fixture::new("auto-mode");
        let render = |f: &Fixture| {
            let p = crate::profile::load(&f.profile, "p", &env()).unwrap();
            let opts = Options {
                harnesses: &[],
                adds: &[],
                vars: &[],
                container: true,
            };
            render::render(&p, &f.profile, std::slice::from_ref(&f.root), &opts).unwrap()
        };
        write(&render(&f), &f.out(), &f.out()).unwrap();
        let settings = fs::read_to_string(f.claude("settings.json")).unwrap();
        assert!(settings.contains("\"autoMode\""), "{settings}");
        assert!(
            check(&render(&f), &f.out(), &Ignore::default())
                .unwrap()
                .rows
                .is_empty()
        );
        // The description of the container comes from the profile: another
        // network is another description, so the file is stale, like after a
        // change of a block.
        fs::write(
            &f.profile,
            "harness: claude\nrules: [a]\ncommands: all\nsettings:\n  claude: [s]\ncontainer:\n  network: none\n",
        )
        .unwrap();
        let r = check(&render(&f), &f.out(), &Ignore::default()).unwrap();
        assert_eq!(states(&r), [("settings.json".to_string(), "stale")]);
        assert_eq!(
            r.rows[0].state,
            State::Stale {
                added: 1,
                removed: 1
            }
        );
    }

    #[test]
    fn a_harness_rewriting_key_order_is_still_current() {
        let f = Fixture::new("reorder");
        f.written();
        fs::write(
            f.claude("settings.json"),
            "{\"keep\":1,\n  \"model\":\"opus\"}",
        )
        .unwrap();
        assert!(f.check(&Ignore::default()).rows.is_empty());
        fs::write(f.claude("settings.json"), r#"{"keep":2,"model":"opus"}"#).unwrap();
        let r = f.check(&Ignore::default());
        assert_eq!(states(&r), [("settings.json".to_string(), "modified")]);
    }

    #[test]
    fn ignored_pointers_make_runtime_keys_current() {
        let f = Fixture::new("ignore");
        f.written();
        fs::write(
            f.claude("settings.json"),
            r#"{"model":"opus","keep":1,"feedback":{"seen":true}}"#,
        )
        .unwrap();
        assert_eq!(f.check(&Ignore::default()).rows.len(), 1);
        let mut ignore = Ignore::default();
        ignore.claude.push("/feedback".to_string());
        assert!(f.check(&ignore).rows.is_empty());
        // The ignored key does not hide a change of another one.
        fs::write(f.claude("settings.json"), r#"{"model":"x","feedback":1}"#).unwrap();
        assert_eq!(f.check(&ignore).rows.len(), 1);
    }

    #[test]
    fn diff_shows_the_disk_to_render_change() {
        let f = Fixture::new("diff");
        f.written();
        fs::write(f.root.join("rules/a.md"), "one\nTWO\n").unwrap();
        fs::remove_file(f.claude("commands/c.md")).unwrap();
        let text = format(&f.check(&Ignore::default()), true);
        let head = "--- a/.claude/CLAUDE.md\n+++ b/.claude/CLAUDE.md\n";
        assert!(text.contains(head), "{text}");
        assert!(text.contains("-two\n+TWO\n"), "{text}");
        assert!(
            text.contains("+cmd\n"),
            "a missing file diffs from empty: {text}"
        );
    }

    #[test]
    fn capture_yaml_lists_add_up_and_are_strict() {
        let t = TempDir::new("capture");
        let (a, b) = (t.0.join("a"), t.0.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(
            a.join("capture.yaml"),
            "ignore:\n  claude: [/x, /y/z]\n  pi: [/p]\n",
        )
        .unwrap();
        fs::write(b.join("capture.yaml"), "ignore:\n  claude: [/w]\n").unwrap();
        let i = load_ignore(&[a.clone(), b.clone(), t.0.join("none")]).unwrap();
        assert_eq!(i.claude, ["/x", "/y/z", "/w"]);
        assert_eq!(i.pi, ["/p"]);
        assert!(i.opencode.is_empty());
        fs::write(b.join("capture.yaml"), "ignore:\n  vim: [/w]\n").unwrap();
        assert!(
            load_ignore(std::slice::from_ref(&b))
                .unwrap_err()
                .0
                .contains("unknown field `vim`")
        );
        fs::write(b.join("capture.yaml"), "ignoree: {}\n").unwrap();
        assert!(load_ignore(std::slice::from_ref(&b)).is_err());
        fs::write(b.join("capture.yaml"), "ignore:\n  claude: [x]\n").unwrap();
        assert!(
            load_ignore(&[b])
                .unwrap_err()
                .0
                .contains("not a JSON Pointer")
        );
    }

    #[test]
    fn pointers_are_unescaped_and_reach_arrays() {
        let mut v: Value =
            serde_json::from_str(r#"{"a/b":1,"c~d":2,"l":[1,2,3],"n":{"x":1}}"#).unwrap();
        for p in ["/a~1b", "/c~0d", "/l/1", "/n/x", "/missing/deep", "/l/9"] {
            remove_pointer(&mut v, p);
        }
        assert_eq!(v.to_string(), r#"{"l":[1,3],"n":{}}"#);
    }
}
