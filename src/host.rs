//! The host target (docs/design.md, "Host target"): `bree render --target
//! host` and `bree check --target host` on the host's own harness
//! directories, `~/.claude`, `~/.pi/agent` and `~/.config/opencode`.
//!
//! The managed-copy rule: bree installs what is missing and updates a file
//! only while it is unchanged since bree wrote it. When a file was edited by
//! hand, or never was bree's, a render writes nothing at all. What bree wrote
//! is recorded in `~/.local/state/bree/host.json`; while that file does not
//! exist, the record is the shared config repository's `deployed.json` (see `role`). One plan
//! decides every file, and both commands read it, so they cannot disagree.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::check::{self, Ignore, Report, Row};
use crate::error::{Error, Result};
use crate::profile::Harness;
use crate::render::{
    self, BOOKKEEPING, Change, File, HarnessRender, Render, Written, align, apply, bookkeeping,
    harness_dir, io_err, is_executable, manifest_files, read_optional, rendered_copy,
    safe_relative, write_atomic,
};
use crate::role;

/// A path in a harness directory as the rows show it, with `~` for the home.
fn shown(rel: &str, path: &str) -> String {
    format!("~/{rel}/{path}")
}

// --------------------------------------------------------------- records

/// What was recorded of a file when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hash {
    /// bree's own, in `host.json`: `render::digest`.
    Sha256(String),
    /// The role's, in `deployed.json`: see `role::matches`.
    Sha1(String),
}

/// A file bree or the role wrote, by its path in the harness directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub path: String,
    pub hash: Hash,
}

impl Record {
    /// Whether the live file is unchanged since it was written. For bree's
    /// record that compares it with bree's copy of what it wrote (key order
    /// and the ignored keys do not count), or with the recorded hash when the
    /// copy is gone; for the role's record, the role's rule.
    fn unchanged(&self, home: &Path, h: Harness, disk: &[u8], ignore: &[String]) -> Result<bool> {
        match &self.hash {
            Hash::Sha1(sha1) => Ok(role::matches(&self.path, disk, sha1)),
            Hash::Sha256(sha256) => {
                let copy = read_optional(&rendered_copy(home, h, &self.path))?;
                Ok(!check::edited(
                    &self.path,
                    disk,
                    sha256,
                    copy.as_deref(),
                    ignore,
                ))
            }
        }
    }
}

fn host_json(home: &Path) -> PathBuf {
    bookkeeping(home).join("host.json")
}

/// Everything recorded about the host's harness directories.
#[derive(Debug, Default)]
pub struct Records {
    /// The manifests of `host.json` by harness name, as the file has them;
    /// none while there is no `host.json`.
    manifests: Option<BTreeMap<String, Value>>,
    files: BTreeMap<&'static str, Vec<Record>>,
}

impl Records {
    /// `host.json`; without it, the import of the role's `deployed.json`;
    /// without that, nothing. Once `host.json` exists the role's file is not
    /// read again.
    pub fn load(home: &Path) -> Result<Records> {
        let path = host_json(home);
        match read_optional(&path)? {
            Some(bytes) => Records::from_host_json(&path, &bytes),
            None => Records::import(home),
        }
    }

    fn from_host_json(path: &Path, bytes: &[u8]) -> Result<Records> {
        let bad = |what: &str| Error::new(format!("{}: {what}", path.display()));
        let doc: Value = serde_json::from_slice(bytes).map_err(|e| bad(&e.to_string()))?;
        let Some(Value::Object(harnesses)) = doc.get("harnesses") else {
            return Err(bad("no `harnesses` object"));
        };
        let mut records = Records::default();
        let mut manifests = BTreeMap::new();
        for (name, manifest) in harnesses {
            // A harness this bree does not know is kept as it is.
            if let Some(h) = Harness::from_name(name) {
                let files = manifest_files(manifest, &|what| bad(&format!("{name}: {what}")))?;
                records.files.insert(
                    h.name(),
                    files
                        .into_iter()
                        .map(|(path, sha256)| Record {
                            path,
                            hash: Hash::Sha256(sha256),
                        })
                        .collect(),
                );
            }
            manifests.insert(name.clone(), manifest.clone());
        }
        records.manifests = Some(manifests);
        Ok(records)
    }

    /// The role's `deployed.json` as records: an entry belongs to a harness
    /// when its destination starts with the harness directory, and the rest
    /// is the path in it. Entries for other directories, and paths that leave
    /// the harness directory, are skipped.
    fn import(home: &Path) -> Result<Records> {
        let mut records = Records::default();
        let Some(deployed) = role::read_deployed(home)? else {
            return Ok(records);
        };
        for h in Harness::ALL {
            let prefix = format!("{}/", harness_dir(h));
            let files = deployed
                .iter()
                .filter_map(|(dest, sha1)| {
                    let path = dest.strip_prefix(&prefix)?;
                    safe_relative(path).then(|| Record {
                        path: path.to_string(),
                        hash: Hash::Sha1(sha1.clone()),
                    })
                })
                .collect();
            records.files.insert(h.name(), files);
        }
        Ok(records)
    }

    /// What was written to the harness's directory.
    pub fn of(&self, h: Harness) -> &[Record] {
        self.files.get(h.name()).map_or(&[], Vec::as_slice)
    }

    /// The content of `host.json` after `render`: its manifests replace the
    /// recorded ones of the same harnesses, and the others stay.
    fn document(&self, render: &Render) -> Value {
        let mut manifests = self.manifests.clone().unwrap_or_default();
        for h in &render.harnesses {
            manifests.insert(h.harness.name().to_string(), render::manifest(render, h));
        }
        json!({"bree": env!("CARGO_PKG_VERSION"), "harnesses": manifests})
    }
}

// ------------------------------------------------------------------ plan

/// What a render does with one rendered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// The file is not there.
    Install,
    /// The file is what bree would write, and executable where it must be.
    Current,
    /// The file differs, and it is unchanged since it was written. A file
    /// with the right content but without the executable bit it needs counts
    /// as well: nothing of anyone's content is lost.
    Update,
    /// The file differs, and it was edited since it was written or never was
    /// bree's.
    Conflict,
}

impl Op {
    /// What a write does when it goes ahead: a conflict is overwritten.
    fn change(self) -> Change {
        match self {
            Op::Install => Change::Installed,
            Op::Current => Change::Current,
            Op::Update | Op::Conflict => Change::Updated,
        }
    }
}

/// A file a render of this harness no longer produces, but recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extra {
    /// The live file is gone: the record just goes.
    Gone,
    /// The live file is unchanged since it was written: a render removes it.
    Unchanged(Vec<u8>),
    /// The live file was edited since it was written: a conflict.
    Edited(Vec<u8>),
}

#[derive(Debug)]
pub struct FilePlan<'a> {
    pub file: &'a File,
    /// The live file; none when it is not there.
    pub disk: Option<Vec<u8>>,
    pub op: Op,
}

#[derive(Debug)]
pub struct HarnessPlan<'a> {
    pub harness: &'a HarnessRender,
    /// The rendered files, in the order of the render.
    pub files: Vec<FilePlan<'a>>,
    /// The recorded files the render no longer produces, with their paths.
    pub extras: Vec<(String, Extra)>,
}

impl HarnessPlan<'_> {
    fn conflicts(&self) -> usize {
        let files = self.files.iter().filter(|f| f.op == Op::Conflict).count();
        let extras = self
            .extras
            .iter()
            .filter(|(_, e)| matches!(e, Extra::Edited(_)))
            .count();
        files + extras
    }
}

/// What a render would do on the host, decided before anything is written.
#[derive(Debug)]
pub struct Plan<'a> {
    pub harnesses: Vec<HarnessPlan<'a>>,
}

impl Plan<'_> {
    pub fn conflicts(&self) -> usize {
        self.harnesses.iter().map(HarnessPlan::conflicts).sum()
    }
}

/// The managed-copy rule for one rendered file. `last` says whether the live
/// file is unchanged since it was written, and is none when nothing recorded
/// it. A file that is what the render would write is current whatever was
/// recorded; for a `.json` file that parses, key order and spacing do not
/// count, so a harness that rewrote its settings in its own key order is left
/// as it is.
fn file_op(f: &File, disk: Option<&[u8]>, executable: bool, last: Option<bool>) -> Op {
    match disk {
        None => Op::Install,
        Some(d) if same_content(&f.path, d, &f.bytes) => {
            if f.executable && !executable {
                Op::Update
            } else {
                Op::Current
            }
        }
        Some(_) if last == Some(true) => Op::Update,
        Some(_) => Op::Conflict,
    }
}

/// Whether the live file has the rendered content: the same bytes, or for a
/// `.json` file the same value in another key order or spacing. The keys
/// `capture.yaml` ignores still count here, so a file that differs in them
/// is not left as it is.
fn same_content(path: &str, disk: &[u8], rendered: &[u8]) -> bool {
    disk == rendered || check::normalized(path, disk, &[]) == check::normalized(path, rendered, &[])
}

/// Reads the live files and plans every file of the render against `records`.
/// Writes nothing.
pub fn plan<'a>(
    render: &'a Render,
    home: &Path,
    records: &Records,
    ignore: &Ignore,
) -> Result<Plan<'a>> {
    let mut harnesses = Vec::new();
    for h in &render.harnesses {
        let dir = home.join(harness_dir(h.harness));
        let ignore = ignore.get(h.harness);
        let recorded = records.of(h.harness);
        let mut files = Vec::new();
        for f in &h.files {
            let target = dir.join(&f.path);
            let disk = read_optional(&target)?;
            // Only a file that differs from the render needs its history.
            let last = match (&disk, recorded.iter().find(|r| r.path == f.path)) {
                (Some(d), Some(r)) if !same_content(&f.path, d, &f.bytes) => {
                    Some(r.unchanged(home, h.harness, d, ignore)?)
                }
                _ => None,
            };
            let op = file_op(f, disk.as_deref(), is_executable(&target), last);
            files.push(FilePlan { file: f, disk, op });
        }
        let mut extras = Vec::new();
        for r in recorded
            .iter()
            .filter(|r| !h.files.iter().any(|f| f.path == r.path))
        {
            let extra = match read_optional(&dir.join(&r.path))? {
                None => Extra::Gone,
                Some(d) if r.unchanged(home, h.harness, &d, ignore)? => Extra::Unchanged(d),
                Some(d) => Extra::Edited(d),
            };
            extras.push((r.path.clone(), extra));
        }
        harnesses.push(HarnessPlan {
            harness: h,
            files,
            extras,
        });
    }
    Ok(Plan { harnesses })
}

// ---------------------------------------------------------------- render

/// A render that found conflicts and wrote nothing but the pending files.
#[derive(Debug)]
pub struct Conflicts {
    /// One row per conflict.
    rows: Vec<Vec<String>>,
}

impl Conflicts {
    /// The rows aligned, then the line that says what to do.
    pub fn text(&self) -> String {
        let n = self.rows.len();
        let noun = if n == 1 { "conflict" } else { "conflicts" };
        format!(
            "{}bree: {n} {noun}, nothing written; take bree's version from \
             ~/{BOOKKEEPING}/pending/ or move your change into the blocks, \
             then render again (--force overwrites)\n",
            align(&self.rows)
        )
    }
}

#[derive(Debug)]
pub enum Outcome {
    Written(Written),
    Conflicts(Conflicts),
}

/// Replaces `pending/` with bree's version of each file in conflict, under
/// the same path as the live file has in the home, and returns the rows that
/// say what to diff.
fn save_pending(home: &Path, plan: &Plan) -> Result<Conflicts> {
    remove_pending(home)?;
    let pending = bookkeeping(home).join("pending");
    let mut rows = Vec::new();
    for hp in &plan.harnesses {
        let h = hp.harness.harness;
        let rel = harness_dir(h);
        let row =
            |live: String, what: String| vec![h.name().to_string(), live, "conflict".into(), what];
        for fp in hp.files.iter().filter(|f| f.op == Op::Conflict) {
            let path = &fp.file.path;
            write_atomic(
                &pending.join(rel).join(path),
                &fp.file.bytes,
                fp.file.executable,
            )?;
            let live = shown(rel, path);
            let saved = format!("~/{BOOKKEEPING}/pending/{rel}/{path}");
            rows.push(row(live.clone(), format!("diff -u {live} {saved}")));
        }
        for (path, extra) in &hp.extras {
            if matches!(extra, Extra::Edited(_)) {
                rows.push(row(
                    shown(rel, path),
                    "edited, and no block produces it any more".to_string(),
                ));
            }
        }
    }
    Ok(Conflicts { rows })
}

fn remove_pending(home: &Path) -> Result<()> {
    let pending = bookkeeping(home).join("pending");
    match fs::remove_dir_all(&pending) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err(&pending)(e)),
        _ => Ok(()),
    }
}

/// `bree render --target host`: plans every file, and writes only when the
/// plan has no conflict or `force` says to overwrite them. A refused render
/// leaves the harness directories, `host.json` and bree's copies as they
/// were. `only` says the render was limited with `--harness`, which only
/// changes the advice of the import guard's error.
pub fn render(
    r: &Render,
    home: &Path,
    ignore: &Ignore,
    force: bool,
    only: bool,
) -> Result<Outcome> {
    let records = Records::load(home)?;
    // The first render writes host.json, after which deployed.json is never
    // read again: a harness the render leaves out, by `--harness` or because
    // the profile does not have it, would lose its import, and every file the
    // role wrote there would later count as one bree never wrote.
    if records.manifests.is_none() {
        let left_out: Vec<&str> = Harness::ALL
            .into_iter()
            .filter(|&h| !records.of(h).is_empty())
            .filter(|&h| !r.harnesses.iter().any(|hr| hr.harness == h))
            .map(Harness::name)
            .collect();
        if !left_out.is_empty() {
            let fix = if only {
                "drop --harness, and the profile's harnesses must list each of them"
            } else {
                "add each of them to the profile's harnesses"
            };
            return Err(Error::new(format!(
                "render: the first render on the host imports {} and must include \
                 every harness it records; this render leaves out {} ({fix})",
                role::DEPLOYED_SHOWN,
                left_out.join(", ")
            )));
        }
    }
    let plan = plan(r, home, &records, ignore)?;
    if plan.conflicts() > 0 && !force {
        return save_pending(home, &plan).map(Outcome::Conflicts);
    }
    let mut w = Written::default();
    for hp in &plan.harnesses {
        let rel = harness_dir(hp.harness.harness);
        let changes: Vec<Change> = hp.files.iter().map(|f| f.op.change()).collect();
        // A record whose file is gone leaves the state without a row.
        let dropped: Vec<&str> = hp.extras.iter().map(|(path, _)| path.as_str()).collect();
        apply(home, hp.harness, &changes, &dropped)?;
        let files: Vec<(&str, Change)> = hp
            .files
            .iter()
            .map(|f| f.file.path.as_str())
            .zip(changes)
            .collect();
        let removed: Vec<&str> = hp
            .extras
            .iter()
            .filter(|(_, extra)| *extra != Extra::Gone)
            .map(|(path, _)| path.as_str())
            .collect();
        w.add(
            hp.harness.harness,
            |path| shown(rel, path),
            &files,
            &removed,
        );
    }
    let mut text = serde_json::to_string_pretty(&records.document(r)).expect("serializes");
    text.push('\n');
    write_atomic(&host_json(home), text.as_bytes(), false)?;
    remove_pending(home)?;
    Ok(Outcome::Written(w))
}

// ----------------------------------------------------------------- check

/// `bree check --target host`: what the plan says, as the rows of `check`:
/// a file that is not there is `missing`, one a render would update is
/// `stale`, one it would refuse is `modified`, and a recorded one no block
/// produces any more is `extra`. Writes nothing.
pub fn check(r: &Render, home: &Path, ignore: &Ignore) -> Result<Report> {
    let records = Records::load(home)?;
    let plan = plan(r, home, &records, ignore)?;
    Ok(report(&plan, ignore))
}

fn report(plan: &Plan, ignore: &Ignore) -> Report {
    let mut rows = Vec::new();
    let mut total = 0;
    for hp in &plan.harnesses {
        let h = hp.harness.harness;
        let rel = harness_dir(h);
        let ignore = ignore.get(h);
        let mut row = |path: &str, state: check::State, disk: &str, rendered: &str| {
            let title = format!("{rel}/{path}");
            rows.push(Row::new(h, shown(rel, path), &title, state, disk, rendered));
        };
        for fp in &hp.files {
            total += 1;
            let f = fp.file;
            let rendered = String::from_utf8_lossy(&f.bytes);
            let disk = fp.disk.as_deref().map(String::from_utf8_lossy);
            let disk_text = disk.as_deref().unwrap_or_default();
            match fp.op {
                Op::Install => row(&f.path, check::State::Missing, "", &rendered),
                Op::Current => {}
                // As `check` always has: a file that differs from the render
                // only in key order or in ignored keys is not stale.
                Op::Update => {
                    let on_disk = fp.disk.as_deref().unwrap_or_default();
                    if check::normalized(&f.path, on_disk, ignore)
                        != check::normalized(&f.path, &f.bytes, ignore)
                    {
                        row(
                            &f.path,
                            check::State::stale(disk_text, &rendered),
                            disk_text,
                            &rendered,
                        );
                    }
                }
                Op::Conflict => row(&f.path, check::State::Modified, disk_text, &rendered),
            }
        }
        for (path, extra) in &hp.extras {
            if let Extra::Unchanged(disk) | Extra::Edited(disk) = extra {
                total += 1;
                row(
                    path,
                    check::State::Extra,
                    &String::from_utf8_lossy(disk),
                    "",
                );
            }
        }
    }
    Report { rows, total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::TempDir;
    use crate::render::{Kind, Root, Source};

    fn file(path: &str, text: &str) -> File {
        File {
            path: path.to_string(),
            bytes: text.as_bytes().to_vec(),
            kind: Kind::Files,
            sources: vec![Source {
                block: "b".to_string(),
                root: "r".to_string(),
                lines: None,
            }],
            executable: false,
        }
    }

    /// A render of these files, which the renderer keeps sorted by path.
    fn render_of(harnesses: Vec<(Harness, Vec<File>)>) -> Render {
        Render {
            profile: "p".to_string(),
            roots: vec![Root {
                name: "r".to_string(),
                path: PathBuf::from("/r"),
            }],
            harnesses: harnesses
                .into_iter()
                .map(|(harness, mut files)| {
                    files.sort_by(|a, b| a.path.cmp(&b.path));
                    HarnessRender {
                        harness,
                        files,
                        used: Vec::new(),
                    }
                })
                .collect(),
            library: Vec::new(),
        }
    }

    fn claude(files: Vec<File>) -> Render {
        render_of(vec![(Harness::Claude, files)])
    }

    fn put(home: &Path, path: &str, text: &str) {
        let p = home.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(home: &Path, path: &str) -> String {
        fs::read_to_string(home.join(path)).unwrap()
    }

    /// Every file under `dir` by relative path, bookkeeping included.
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

    fn no_ignore() -> Ignore {
        Ignore::default()
    }

    /// A render that must go through.
    fn written(r: &Render, home: &Path) -> Written {
        match render(r, home, &no_ignore(), false, false).unwrap() {
            Outcome::Written(w) => w,
            Outcome::Conflicts(c) => panic!("conflicts:\n{}", c.text()),
        }
    }

    fn ops(r: &Render, home: &Path, ignore: &Ignore) -> Vec<(String, Op)> {
        let records = Records::load(home).unwrap();
        let p = plan(r, home, &records, ignore).unwrap();
        p.harnesses
            .iter()
            .flat_map(|h| h.files.iter().map(|f| (f.file.path.clone(), f.op)))
            .collect()
    }

    fn states(rows: &[Vec<String>]) -> Vec<(String, String)> {
        rows.iter().map(|r| (r[1].clone(), r[2].clone())).collect()
    }

    // ------------------------------------------------------ classification

    #[test]
    fn file_op_follows_the_managed_copy_rule() {
        let f = file("a", "new\n");
        let x = File {
            executable: true,
            ..file("a", "new\n")
        };
        for last in [None, Some(true), Some(false)] {
            assert_eq!(file_op(&f, None, false, last), Op::Install);
            // What the render would write is current, whatever was recorded.
            assert_eq!(file_op(&f, Some(b"new\n"), false, last), Op::Current);
            assert_eq!(file_op(&x, Some(b"new\n"), true, last), Op::Current);
            // Right content without the executable bit: nothing to lose.
            assert_eq!(file_op(&x, Some(b"new\n"), false, last), Op::Update);
        }
        assert_eq!(file_op(&f, Some(b"old\n"), false, Some(true)), Op::Update);
        assert_eq!(file_op(&f, Some(b"old\n"), true, Some(true)), Op::Update);
        assert_eq!(
            file_op(&f, Some(b"old\n"), false, Some(false)),
            Op::Conflict
        );
        assert_eq!(file_op(&f, Some(b"old\n"), false, None), Op::Conflict);
        let j = file("s.json", "{\"a\": 1, \"b\": 2}\n");
        assert_eq!(
            file_op(&j, Some(b"{\"b\":2,\"a\":1}"), false, None),
            Op::Current
        );
        assert_eq!(
            file_op(&j, Some(b"{\"b\":3,\"a\":1}"), false, None),
            Op::Conflict
        );
    }

    #[test]
    fn recorded_files_are_updated_while_unchanged_and_conflict_once_edited() {
        let t = TempDir::new("host-plan");
        let v1 = claude(vec![
            file("a.md", "one\n"),
            file("b.md", "two\n"),
            file("c.md", "three\n"),
        ]);
        let w = written(&v1, &t.0);
        assert_eq!((w.written, w.current, w.removed), (3, 0, 0));
        let v2 = claude(vec![
            file("a.md", "one!\n"),
            file("b.md", "two!\n"),
            file("c.md", "three\n"),
        ]);
        put(&t.0, ".claude/b.md", "mine\n");
        assert_eq!(
            ops(&v2, &t.0, &no_ignore()),
            [
                ("a.md".to_string(), Op::Update),
                ("b.md".to_string(), Op::Conflict),
                ("c.md".to_string(), Op::Current),
            ]
        );
        // An edit that lands on what the render says is not a conflict.
        put(&t.0, ".claude/b.md", "two!\n");
        assert_eq!(ops(&v2, &t.0, &no_ignore())[1].1, Op::Current);
    }

    #[test]
    fn the_recorded_hash_decides_when_bree_s_copy_is_gone() {
        let t = TempDir::new("host-hash");
        let v1 = claude(vec![file("a.md", "one\n"), file("b.md", "two\n")]);
        written(&v1, &t.0);
        fs::remove_dir_all(t.0.join(".local/state/bree/rendered")).unwrap();
        put(&t.0, ".claude/b.md", "mine\n");
        let v2 = claude(vec![file("a.md", "one!\n"), file("b.md", "two!\n")]);
        assert_eq!(
            ops(&v2, &t.0, &no_ignore()),
            [
                ("a.md".to_string(), Op::Update),
                ("b.md".to_string(), Op::Conflict)
            ]
        );
    }

    #[test]
    fn key_order_and_ignored_keys_are_not_edits() {
        let t = TempDir::new("host-json");
        let v1 = claude(vec![file("settings.json", "{\"a\": 1, \"b\": 2}\n")]);
        written(&v1, &t.0);
        let v2 = claude(vec![file("settings.json", "{\"a\": 5, \"b\": 2}\n")]);
        let update = [("settings.json".to_string(), Op::Update)];
        let conflict = [("settings.json".to_string(), Op::Conflict)];
        put(
            &t.0,
            ".claude/settings.json",
            "{\n \"b\": 2,\n \"a\": 1\n}\n",
        );
        assert_eq!(ops(&v2, &t.0, &no_ignore()), update);
        put(
            &t.0,
            ".claude/settings.json",
            "{\"a\": 1, \"b\": 2, \"seen\": true}",
        );
        assert_eq!(ops(&v2, &t.0, &no_ignore()), conflict);
        let mut ignore = Ignore::default();
        ignore.claude.push("/seen".to_string());
        assert_eq!(ops(&v2, &t.0, &ignore), update);
        // The ignored key does not hide a real edit.
        put(&t.0, ".claude/settings.json", "{\"a\": 9, \"seen\": true}");
        assert_eq!(ops(&v2, &t.0, &ignore), conflict);
    }

    #[test]
    fn a_file_bree_never_wrote_conflicts_unless_it_is_what_the_render_says() {
        let t = TempDir::new("host-unrecorded");
        put(&t.0, ".claude/a.md", "mine\n");
        put(&t.0, ".claude/b.md", "same\n");
        let r = claude(vec![
            file("a.md", "one\n"),
            file("b.md", "same\n"),
            file("c.md", "new\n"),
        ]);
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [
                ("a.md".to_string(), Op::Conflict),
                ("b.md".to_string(), Op::Current),
                ("c.md".to_string(), Op::Install),
            ]
        );
    }

    #[test]
    fn json_with_the_rendered_values_in_another_key_order_is_current() {
        // A settings.json bree never recorded, with the rendered values in
        // Claude Code's key order, is no conflict, and a render leaves it as
        // it is.
        let t = TempDir::new("host-json-order");
        let live = "{\n  \"b\": [1, {\"y\": 2, \"x\": 1}],\n  \"a\": \"z\"\n}\n";
        put(&t.0, ".claude/settings.json", live);
        let r = claude(vec![
            file(
                "settings.json",
                "{\"a\": \"z\", \"b\": [1, {\"x\": 1, \"y\": 2}]}\n",
            ),
            file("x.md", "{\"a\": 1}\n"),
        ]);
        put(&t.0, ".claude/x.md", "{\"a\":1}\n");
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [
                ("settings.json".to_string(), Op::Current),
                // Only .json files compare by value.
                ("x.md".to_string(), Op::Conflict),
            ]
        );
        put(&t.0, ".claude/x.md", "{\"a\": 1}\n");
        let w = written(&r, &t.0);
        assert_eq!((w.written, w.current), (0, 2));
        assert_eq!(read(&t.0, ".claude/settings.json"), live);
        // Array order and a key the render does not have still count.
        put(
            &t.0,
            ".claude/settings.json",
            "{\"a\": \"z\", \"b\": [{\"x\": 1, \"y\": 2}, 1]}",
        );
        let r2 = claude(vec![file(
            "settings.json",
            "{\"a\": \"z\", \"b\": [1, {\"x\": 1, \"y\": 2}], \"c\": 1}\n",
        )]);
        assert_eq!(ops(&r2, &t.0, &no_ignore())[0].1, Op::Conflict);
        put(
            &t.0,
            ".claude/settings.json",
            "{\"b\": [1, {\"x\": 1, \"y\": 2}], \"a\": \"z\", \"seen\": 1}",
        );
        // An ignored key is not the rendered content: the file is recorded
        // by now, so it is an update, never left as it is.
        let mut ignore = Ignore::default();
        ignore.claude.push("/seen".to_string());
        assert_eq!(ops(&r, &t.0, &ignore)[0].1, Op::Update);
    }

    #[test]
    fn a_file_missing_its_executable_bit_is_updated() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("host-exec");
        put(&t.0, ".claude/run.sh", "echo\n");
        let r = claude(vec![File {
            executable: true,
            ..file("run.sh", "echo\n")
        }]);
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [("run.sh".to_string(), Op::Update)]
        );
        let w = written(&r, &t.0);
        assert_eq!(w.rows[0][2], "updated");
        let mode = fs::metadata(t.0.join(".claude/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(written(&r, &t.0).current, 1);
    }

    #[test]
    fn recorded_files_the_render_drops_are_gone_removable_or_edited() {
        let t = TempDir::new("host-extra");
        let v1 = claude(vec![
            file("keep.md", "k\n"),
            file("gone.md", "g\n"),
            file("same.md", "s\n"),
            file("edited.md", "e\n"),
        ]);
        written(&v1, &t.0);
        fs::remove_file(t.0.join(".claude/gone.md")).unwrap();
        put(&t.0, ".claude/edited.md", "mine\n");
        let v2 = claude(vec![file("keep.md", "k\n")]);
        let records = Records::load(&t.0).unwrap();
        let p = plan(&v2, &t.0, &records, &no_ignore()).unwrap();
        assert_eq!(
            p.harnesses[0].extras,
            [
                ("edited.md".to_string(), Extra::Edited(b"mine\n".to_vec())),
                ("gone.md".to_string(), Extra::Gone),
                ("same.md".to_string(), Extra::Unchanged(b"s\n".to_vec())),
            ]
        );
        assert_eq!(p.conflicts(), 1);
    }

    // -------------------------------------------------------------- records

    #[test]
    fn deployed_json_entries_belong_to_the_harness_whose_directory_starts_them() {
        let t = TempDir::new("host-import");
        put(
            &t.0,
            ".local/state/ai-role/deployed.json",
            r#"{
                ".claude/CLAUDE.md": "c1",
                ".claude/skills/x/SKILL.md": "c2",
                ".claude/../escape": "bad",
                ".claude/": "bad",
                ".claudeX/y": "other",
                ".pi/agent/AGENTS.md": "p1",
                ".config/opencode/opencode.json": "o1",
                ".config/other/z": "z",
                ".bashrc": "b"
            }"#,
        );
        let records = Records::load(&t.0).unwrap();
        let of = |h| {
            records
                .of(h)
                .iter()
                .map(|r| (r.path.as_str(), r.hash.clone()))
                .collect::<Vec<_>>()
        };
        let sha1 = |s: &str| Hash::Sha1(s.to_string());
        assert_eq!(
            of(Harness::Claude),
            [("CLAUDE.md", sha1("c1")), ("skills/x/SKILL.md", sha1("c2"))]
        );
        assert_eq!(of(Harness::Pi), [("AGENTS.md", sha1("p1"))]);
        assert_eq!(of(Harness::Opencode), [("opencode.json", sha1("o1"))]);
    }

    #[test]
    fn deployed_json_is_read_only_while_there_is_no_host_json() {
        let t = TempDir::new("host-once");
        let deployed = ".local/state/ai-role/deployed.json";
        put(&t.0, deployed, "[1]");
        let e = Records::load(&t.0).unwrap_err();
        assert!(
            e.0.starts_with(&format!("{}: ", t.0.join(deployed).display())),
            "{e}"
        );

        put(&t.0, deployed, r#"{".claude/a.md": "x"}"#);
        let r = claude(vec![file("a.md", "one\n")]);
        written(&r, &t.0);
        // host.json exists now, so a deployed.json that would fail is not read.
        put(&t.0, deployed, "not json at all");
        let records = Records::load(&t.0).unwrap();
        assert_eq!(records.of(Harness::Claude).len(), 1);
        assert!(matches!(
            records.of(Harness::Claude)[0].hash,
            Hash::Sha256(_)
        ));
        assert_eq!(read(&t.0, deployed), "not json at all");
    }

    #[test]
    fn a_host_json_that_does_not_parse_is_an_error_naming_it() {
        let t = TempDir::new("host-bad");
        let path = t.0.join(".local/state/bree/host.json");
        for (text, what) in [
            ("{nope", "at line 1 column"),
            ("[]", "no `harnesses` object"),
            (
                r#"{"harnesses": {"claude": {}}}"#,
                "claude: no `files` list",
            ),
            (
                r#"{"harnesses": {"pi": {"files": [{"path": "../x", "sha256": "0"}]}}}"#,
                "pi: unsafe path `../x`",
            ),
        ] {
            put(&t.0, ".local/state/bree/host.json", text);
            let e = Records::load(&t.0).unwrap_err();
            assert!(
                e.0.starts_with(&format!("{}: ", path.display())) && e.0.contains(what),
                "{text}: {e}"
            );
        }
    }

    #[test]
    fn host_json_is_sorted_by_harness_and_a_partial_render_keeps_the_others() {
        let t = TempDir::new("host-doc");
        let both = render_of(vec![
            (Harness::Pi, vec![file("AGENTS.md", "pi\n")]),
            (Harness::Claude, vec![file("CLAUDE.md", "c\n")]),
        ]);
        written(&both, &t.0);
        let doc = |t: &TempDir| -> Value {
            serde_json::from_str(&read(&t.0, ".local/state/bree/host.json")).unwrap()
        };
        let first = doc(&t);
        assert_eq!(first["bree"], env!("CARGO_PKG_VERSION"));
        let names: Vec<_> = first["harnesses"].as_object().unwrap().keys().collect();
        assert_eq!(names, ["claude", "pi"]);
        assert_eq!(first["harnesses"]["pi"]["files"][0]["path"], "AGENTS.md");
        assert!(read(&t.0, ".local/state/bree/host.json").ends_with("}\n"));

        let only = claude(vec![file("CLAUDE.md", "c2\n")]);
        written(&only, &t.0);
        let second = doc(&t);
        assert_eq!(second["harnesses"]["pi"], first["harnesses"]["pi"]);
        assert_ne!(second["harnesses"]["claude"], first["harnesses"]["claude"]);
        assert_eq!(read(&t.0, ".pi/agent/AGENTS.md"), "pi\n");
    }

    #[test]
    fn a_harness_this_bree_does_not_know_stays_in_host_json() {
        let t = TempDir::new("host-unknown");
        put(
            &t.0,
            ".local/state/bree/host.json",
            r#"{"bree": "9", "harnesses": {"vim": {"future": true}}}"#,
        );
        written(&claude(vec![file("a.md", "one\n")]), &t.0);
        let doc: Value = serde_json::from_str(&read(&t.0, ".local/state/bree/host.json")).unwrap();
        assert_eq!(doc["harnesses"]["vim"], json!({"future": true}));
        assert_eq!(doc["bree"], env!("CARGO_PKG_VERSION"));
    }

    // --------------------------------------------------------------- render

    #[test]
    fn the_role_rule_decides_for_imported_files() {
        let t = TempDir::new("host-role");
        let json_live = "{\n  \"b\": 1,\n  \"a\": \"caf\u{e9}\"\n}\n";
        put(&t.0, ".claude/CLAUDE.md", "role rules\n");
        put(&t.0, ".claude/settings.json", json_live);
        put(&t.0, ".claude/other.md", "role other\n");
        let python_form = "{\"a\": \"caf\\u00e9\", \"b\": 1}";
        let deployed = json!({
            ".claude/CLAUDE.md": role::sha1_hex(b"role rules\n"),
            ".claude/settings.json": role::sha1_hex(python_form.as_bytes()),
            ".claude/other.md": role::sha1_hex(b"role other\n"),
        });
        put(
            &t.0,
            ".local/state/ai-role/deployed.json",
            &deployed.to_string(),
        );
        let r = claude(vec![
            file("CLAUDE.md", "bree rules\n"),
            file("other.md", "bree other\n"),
            file("settings.json", "{\"b\": 2}\n"),
        ]);
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [
                ("CLAUDE.md".to_string(), Op::Update),
                ("other.md".to_string(), Op::Update),
                ("settings.json".to_string(), Op::Update),
            ]
        );
        // The role's raw hash no longer matches an edited file, and a JSON
        // file edited in a value does not match the canonical one either.
        put(&t.0, ".claude/CLAUDE.md", "role rules\nmine\n");
        put(&t.0, ".claude/settings.json", "{\"b\": 3, \"a\": \"cafe\"}");
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [
                ("CLAUDE.md".to_string(), Op::Conflict),
                ("other.md".to_string(), Op::Update),
                ("settings.json".to_string(), Op::Conflict),
            ]
        );
        // A refused import render left the role's file alone.
        assert_eq!(
            read(&t.0, ".local/state/ai-role/deployed.json"),
            deployed.to_string()
        );
    }

    #[test]
    fn a_refused_render_writes_nothing_but_the_pending_files() {
        let t = TempDir::new("host-refuse");
        let v1 = claude(vec![file("a.md", "one\n"), file("b.md", "two\n")]);
        written(&v1, &t.0);
        put(&t.0, ".claude/b.md", "mine\n");
        let before = tree(&t.0);

        let v2 = claude(vec![file("a.md", "one!\n"), file("b.md", "two!\n")]);
        let Outcome::Conflicts(c) = render(&v2, &t.0, &no_ignore(), false, false).unwrap() else {
            panic!("expected conflicts");
        };
        assert_eq!(
            c.text(),
            "claude  ~/.claude/b.md  conflict  diff -u ~/.claude/b.md \
             ~/.local/state/bree/pending/.claude/b.md\n\
             bree: 1 conflict, nothing written; take bree's version from \
             ~/.local/state/bree/pending/ or move your change into the blocks, \
             then render again (--force overwrites)\n"
        );
        let mut after = tree(&t.0);
        let pending = after.remove(".local/state/bree/pending/.claude/b.md");
        assert_eq!(pending.as_deref(), Some(&b"two!\n"[..]));
        assert_eq!(after, before, "nothing else changed");

        // The next refused render starts the pending directory over.
        put(&t.0, ".claude/b.md", "two!\n");
        put(&t.0, ".claude/a.md", "mine too\n");
        let Outcome::Conflicts(c) = render(&v2, &t.0, &no_ignore(), false, false).unwrap() else {
            panic!("expected conflicts");
        };
        assert_eq!(
            states(&c.rows),
            [("~/.claude/a.md".to_string(), "conflict".to_string())]
        );
        let pending: Vec<_> = tree(&t.0.join(".local/state/bree/pending"))
            .into_keys()
            .collect();
        assert_eq!(pending, [".claude/a.md"]);
    }

    #[test]
    fn conflicts_are_counted_and_an_edited_extra_is_one() {
        let t = TempDir::new("host-extra-conflict");
        let v1 = claude(vec![file("a.md", "one\n"), file("old.md", "old\n")]);
        written(&v1, &t.0);
        put(&t.0, ".claude/a.md", "mine\n");
        put(&t.0, ".claude/old.md", "mine\n");
        let v2 = claude(vec![file("a.md", "one!\n")]);
        let Outcome::Conflicts(c) = render(&v2, &t.0, &no_ignore(), false, false).unwrap() else {
            panic!("expected conflicts");
        };
        let text = c.text();
        assert!(
            text.contains(
                "claude  ~/.claude/old.md  conflict  edited, and no block produces it any more\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("bree: 2 conflicts, nothing written;"),
            "{text}"
        );
        assert_eq!(read(&t.0, ".claude/old.md"), "mine\n");
        // Only a rendered file has a version of bree's to save.
        let pending: Vec<_> = tree(&t.0.join(".local/state/bree/pending"))
            .into_keys()
            .collect();
        assert_eq!(pending, [".claude/a.md"]);
    }

    #[test]
    fn force_overwrites_conflicts_removes_edited_extras_and_clears_pending() {
        let t = TempDir::new("host-force");
        let v1 = claude(vec![file("a.md", "one\n"), file("old.md", "old\n")]);
        written(&v1, &t.0);
        put(&t.0, ".claude/a.md", "mine\n");
        put(&t.0, ".claude/old.md", "mine\n");
        let v2 = claude(vec![file("a.md", "one!\n")]);
        assert!(matches!(
            render(&v2, &t.0, &no_ignore(), false, false).unwrap(),
            Outcome::Conflicts(_)
        ));
        assert!(t.0.join(".local/state/bree/pending").is_dir());

        let Outcome::Written(w) = render(&v2, &t.0, &no_ignore(), true, false).unwrap() else {
            panic!("expected a write");
        };
        assert_eq!(
            states(&w.rows),
            [
                ("~/.claude/a.md".to_string(), "updated".to_string()),
                ("~/.claude/old.md".to_string(), "removed".to_string()),
            ]
        );
        assert_eq!((w.written, w.current, w.removed), (1, 0, 1));
        assert_eq!(read(&t.0, ".claude/a.md"), "one!\n");
        assert!(!t.0.join(".claude/old.md").exists());
        assert!(!t.0.join(".local/state/bree/pending").exists());
        assert!(
            !t.0.join(".local/state/bree/rendered/claude/old.md")
                .exists()
        );
    }

    #[test]
    fn a_successful_render_clears_an_old_pending_directory() {
        let t = TempDir::new("host-pending");
        put(&t.0, ".local/state/bree/pending/.claude/stale.md", "old\n");
        written(&claude(vec![file("a.md", "one\n")]), &t.0);
        assert!(!t.0.join(".local/state/bree/pending").exists());
    }

    #[test]
    fn a_record_whose_file_is_gone_leaves_without_a_row() {
        let t = TempDir::new("host-forget");
        let v1 = claude(vec![
            file("keep.md", "k\n"),
            file("gone.md", "g\n"),
            file("same.md", "s\n"),
        ]);
        written(&v1, &t.0);
        fs::remove_file(t.0.join(".claude/gone.md")).unwrap();
        let w = written(&claude(vec![file("keep.md", "k\n")]), &t.0);
        assert_eq!(
            states(&w.rows),
            [
                ("~/.claude/keep.md".to_string(), "current".to_string()),
                ("~/.claude/same.md".to_string(), "removed".to_string()),
            ]
        );
        assert_eq!((w.written, w.current, w.removed), (0, 1, 1));
        let copies: Vec<_> = tree(&t.0.join(".local/state/bree/rendered"))
            .into_keys()
            .collect();
        assert_eq!(copies, ["claude/keep.md"]);
        let doc: Value = serde_json::from_str(&read(&t.0, ".local/state/bree/host.json")).unwrap();
        assert_eq!(
            doc["harnesses"]["claude"]["files"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn nothing_but_the_rendered_and_the_recorded_files_is_touched() {
        let t = TempDir::new("host-untouched");
        put(&t.0, ".claude/.credentials.json", "secret\n");
        put(&t.0, ".claude.json", "{}\n");
        put(&t.0, ".claude/projects/x/session.jsonl", "line\n");
        written(&claude(vec![file("a.md", "one\n")]), &t.0);
        written(&claude(vec![file("b.md", "two\n")]), &t.0);
        assert_eq!(read(&t.0, ".claude/.credentials.json"), "secret\n");
        assert_eq!(read(&t.0, ".claude.json"), "{}\n");
        assert_eq!(read(&t.0, ".claude/projects/x/session.jsonl"), "line\n");
        assert!(!t.0.join(".claude/a.md").exists());
        // No manifest in the harness directory, the state is host.json.
        assert!(!t.0.join(".claude/.bree-manifest.json").exists());
    }

    // ---------------------------------------------------------------- check

    #[test]
    fn check_maps_the_plan_to_the_states_of_check() {
        let t = TempDir::new("host-check");
        let v1 = claude(vec![
            file("current.md", "c\n"),
            file("missing.md", "m\n"),
            file("modified.md", "d\n"),
            file("stale.md", "s\n"),
            file("old.md", "o\n"),
            file("gone.md", "g\n"),
        ]);
        written(&v1, &t.0);
        fs::remove_file(t.0.join(".claude/missing.md")).unwrap();
        fs::remove_file(t.0.join(".claude/gone.md")).unwrap();
        put(&t.0, ".claude/modified.md", "mine\n");
        let v2 = claude(vec![
            file("current.md", "c\n"),
            file("missing.md", "m\n"),
            file("modified.md", "d!\n"),
            file("stale.md", "s\nmore\n"),
            file("new.md", "n\n"),
            file("unrecorded.md", "u\n"),
        ]);
        put(&t.0, ".claude/unrecorded.md", "mine\n");
        let before = tree(&t.0);
        let report = check(&v2, &t.0, &no_ignore()).unwrap();
        assert_eq!(tree(&t.0), before, "check writes nothing");
        let text = check::format(&report, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "claude  ~/.claude/missing.md     missing   not on disk",
                "claude  ~/.claude/modified.md    modified  edited since bree rendered it",
                "claude  ~/.claude/new.md         missing   not on disk",
                "claude  ~/.claude/stale.md       stale     render differs, +1 -0 lines",
                "claude  ~/.claude/unrecorded.md  modified  edited since bree rendered it",
                "claude  ~/.claude/old.md         extra     no block produces it any more",
                "bree: 6 of 7 files not current",
            ]
        );
        assert_eq!(report.total, 7);
    }

    #[test]
    fn check_with_diff_titles_the_files_like_the_container_check() {
        let t = TempDir::new("host-check-diff");
        written(&claude(vec![file("a.md", "one\ntwo\n")]), &t.0);
        let v2 = claude(vec![file("a.md", "one\nTWO\n")]);
        let text = check::format(&check(&v2, &t.0, &no_ignore()).unwrap(), true);
        assert!(
            text.contains("--- a/.claude/a.md\n+++ b/.claude/a.md\n"),
            "{text}"
        );
        assert!(text.contains("-two\n+TWO\n"), "{text}");
    }

    #[test]
    fn a_file_that_differs_only_in_key_order_is_not_stale() {
        let t = TempDir::new("host-check-order");
        written(
            &claude(vec![file("settings.json", "{\"a\": 1, \"b\": 2}\n")]),
            &t.0,
        );
        put(&t.0, ".claude/settings.json", "{\"b\": 2,\n \"a\": 1}");
        let r = claude(vec![file("settings.json", "{\"a\": 1, \"b\": 2}\n")]);
        assert!(check(&r, &t.0, &no_ignore()).unwrap().rows.is_empty());
        // Nor does the render rewrite it in its own key order.
        assert_eq!(
            ops(&r, &t.0, &no_ignore()),
            [("settings.json".to_string(), Op::Current)]
        );
    }

    #[test]
    fn check_on_an_empty_home_reports_everything_missing_and_creates_nothing() {
        let t = TempDir::new("host-check-empty");
        let r = claude(vec![file("a.md", "one\n"), file("b.md", "two\n")]);
        let report = check(&r, &t.0, &no_ignore()).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert!(report.rows.iter().all(|r| r.state == check::State::Missing));
        assert!(tree(&t.0).is_empty());
        assert!(!t.0.join(".local").exists());
    }

    #[test]
    fn check_and_render_agree_on_what_is_to_do() {
        let t = TempDir::new("host-agree");
        let v1 = claude(vec![file("a.md", "one\n"), file("b.md", "two\n")]);
        written(&v1, &t.0);
        let v2 = claude(vec![file("a.md", "one!\n"), file("b.md", "two\n")]);
        put(&t.0, ".claude/a.md", "mine\n");
        // check says modified, and the render refuses.
        let report = check(&v2, &t.0, &no_ignore()).unwrap();
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].state, check::State::Modified);
        assert!(matches!(
            render(&v2, &t.0, &no_ignore(), false, false).unwrap(),
            Outcome::Conflicts(_)
        ));
        // Once the edit is gone, check is clean-ish (stale) and the render goes through.
        put(&t.0, ".claude/a.md", "one\n");
        let report = check(&v2, &t.0, &no_ignore()).unwrap();
        assert_eq!(report.rows.len(), 1);
        assert!(matches!(report.rows[0].state, check::State::Stale { .. }));
        written(&v2, &t.0);
        assert!(check(&v2, &t.0, &no_ignore()).unwrap().rows.is_empty());
    }
}
