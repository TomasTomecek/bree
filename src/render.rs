//! The renderer (docs/design.md, "Building blocks" and "What the renderer
//! writes"): finds the blocks in the roots, composes the ones a profile lists
//! into the files of each harness, in memory, and writes them with a manifest.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::automode;
use crate::error::{Error, Result};
use crate::profile::{Blocks, Harness, Profile};
use crate::tiers::{self, Pool};
use crate::yaml;

pub const MANIFEST: &str = ".bree-manifest.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Rules,
    Skills,
    Commands,
    Settings,
    Files,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Rules => "rules",
            Kind::Skills => "skills",
            Kind::Commands => "commands",
            Kind::Settings => "settings",
            Kind::Files => "files",
        }
    }
}

/// One block found in a root: `path` is the file, or the directory for skills
/// and files bundles.
#[derive(Debug, Clone)]
pub struct Block {
    pub kind: Kind,
    pub harness: Option<Harness>,
    pub name: String,
    pub root: usize,
    pub path: PathBuf,
}

/// Where a rendered file comes from; `lines` is the 1-based inclusive range of
/// a rules block in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub block: String,
    pub root: String,
    pub lines: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub bytes: Vec<u8>,
    pub kind: Kind,
    pub sources: Vec<Source>,
    pub executable: bool,
}

/// A block a harness's render uses, in the order of its list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Used {
    pub kind: Kind,
    pub block: String,
    pub root: String,
}

#[derive(Debug)]
pub struct HarnessRender {
    pub harness: Harness,
    /// Sorted by path.
    pub files: Vec<File>,
    pub used: Vec<Used>,
}

#[derive(Debug)]
pub struct Render {
    pub profile: String,
    pub roots: Vec<Root>,
    pub harnesses: Vec<HarnessRender>,
    /// Every block the roots hold, for `profile contents` to find the unused.
    pub library: Vec<Block>,
}

pub struct Options<'a> {
    /// Empty means the profile's harnesses.
    pub harnesses: &'a [Harness],
    pub adds: &'a [String],
    pub vars: &'a [String],
    /// The render is for the container target: Claude Code's settings get
    /// bree's auto mode seed (design, "What the renderer writes"). `false` for
    /// the host target, with or without `--out`.
    pub container: bool,
}

/// The harness directory relative to the base directory.
pub fn harness_dir(h: Harness) -> &'static str {
    match h {
        Harness::Claude => ".claude",
        Harness::Pi => ".pi/agent",
        Harness::Opencode => ".config/opencode",
    }
}

fn rules_file(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "CLAUDE.md",
        _ => "AGENTS.md",
    }
}

fn commands_dir(h: Harness) -> &'static str {
    match h {
        Harness::Claude => "commands",
        Harness::Pi => "prompts",
        Harness::Opencode => "command",
    }
}

fn settings_file(h: Harness) -> &'static str {
    match h {
        Harness::Opencode => "opencode.json",
        _ => "settings.json",
    }
}

/// bree's bookkeeping directory, relative to the base directory
/// (docs/design.md, "The manifest").
pub const BOOKKEEPING: &str = ".local/state/bree";

pub fn bookkeeping(base: &Path) -> PathBuf {
    base.join(BOOKKEEPING)
}

/// Where bree keeps its copy of what it wrote, under the base directory.
pub fn rendered_copy(base: &Path, h: Harness, path: &str) -> PathBuf {
    bookkeeping(base).join("rendered").join(h.name()).join(path)
}

pub fn io_err(path: &Path) -> impl Fn(io::Error) -> Error {
    let p = path.display().to_string();
    move |e| Error::new(format!("{p}: {e}"))
}

/// The file's bytes, or none when it does not exist.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path)(e)),
    }
}

// ------------------------------------------------------------- discovery

/// A block list's name in the profile and after `--add`.
fn list_name(kind: Kind, harness: Option<Harness>) -> String {
    match harness {
        Some(h) => format!("{}.{}", kind.name(), h.name()),
        None => kind.name().to_string(),
    }
}

/// Sorted `(name, path)` of a directory's entries; none when it is missing.
fn list_dir(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(io_err(dir))?;
        if let Ok(name) = entry.file_name().into_string() {
            out.push((name, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

fn discover(roots: &[Root]) -> Result<Vec<Block>> {
    let mut blocks = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let mut add = |kind, harness, name: &str, path: PathBuf| {
            blocks.push(Block {
                kind,
                harness,
                name: name.to_string(),
                root: i,
                path,
            });
        };
        for (file, path) in list_dir(&root.path.join("rules"))? {
            if let (Some(name), true) = (file.strip_suffix(".md"), path.is_file()) {
                add(Kind::Rules, None, name, path);
            }
        }
        for (name, path) in list_dir(&root.path.join("skills"))? {
            if path.is_dir() {
                if !path.join("SKILL.md").is_file() {
                    return Err(Error::new(format!(
                        "skill {} has no SKILL.md",
                        path.display()
                    )));
                }
                add(Kind::Skills, None, &name, path);
            }
        }
        for (file, path) in list_dir(&root.path.join("commands"))? {
            if let (Some(name), true) = (file.strip_suffix(".md"), path.is_file()) {
                add(Kind::Commands, None, name, path);
            }
        }
        for h in Harness::ALL {
            let dir = root.path.join("settings").join(h.name());
            for (file, path) in list_dir(&dir)? {
                let name = file
                    .strip_suffix(".jsonc")
                    .or_else(|| file.strip_suffix(".json"));
                if let (Some(name), true) = (name, path.is_file()) {
                    add(Kind::Settings, Some(h), name, path);
                }
            }
            for (name, path) in list_dir(&root.path.join("files").join(h.name()))? {
                if path.is_dir() {
                    add(Kind::Files, Some(h), &name, path);
                }
            }
        }
    }
    let mut seen: HashMap<(String, String), &Block> = HashMap::new();
    for b in &blocks {
        let key = (list_name(b.kind, b.harness), b.name.clone());
        if let Some(first) = seen.insert(key, b) {
            return Err(Error::new(format!(
                "{} block `{}` is defined twice: {} and {}",
                list_name(b.kind, b.harness),
                b.name,
                first.path.display(),
                b.path.display()
            )));
        }
    }
    Ok(blocks)
}

// ----------------------------------------------------------- composition

struct Ctx<'a> {
    blocks: &'a [Block],
    roots: &'a [Root],
    optional: &'a [String],
    profile_file: String,
    /// The root that holds the profile: the root of the auto mode seed.
    profile_root: String,
    pool: Option<&'a Pool>,
    container: bool,
}

impl<'a> Ctx<'a> {
    fn find(&self, kind: Kind, harness: Option<Harness>, name: &str) -> Option<&'a Block> {
        self.blocks
            .iter()
            .find(|b| b.kind == kind && b.harness == harness && b.name == name)
    }

    /// Every block of a kind, by name.
    fn all(&self, kind: Kind) -> Vec<&'a Block> {
        let mut v: Vec<&Block> = self.blocks.iter().filter(|b| b.kind == kind).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// The blocks an entry list names for `target`: `HARNESS:NAME` entries
    /// of other harnesses are skipped, and so are optional names no root has.
    fn resolve(
        &self,
        kind: Kind,
        harness: Option<Harness>,
        entries: &[String],
        target: Harness,
    ) -> Result<Vec<&'a Block>> {
        let list = list_name(kind, harness);
        let mut out: Vec<&Block> = Vec::new();
        for entry in entries {
            let name = match entry.split_once(':') {
                Some((h, name)) if harness.is_none() => {
                    if Harness::from_name(h) != Some(target) {
                        continue;
                    }
                    name
                }
                _ => entry.as_str(),
            };
            match self.find(kind, harness, name) {
                Some(b) => out.push(b),
                None if self.optional.iter().any(|o| o == name) => {}
                None => {
                    return Err(yaml::Error::new(
                        &self.profile_file,
                        None,
                        &list,
                        format!("block `{name}` not found in the roots"),
                    )
                    .into());
                }
            }
        }
        if matches!(kind, Kind::Skills | Kind::Commands) {
            let mut seen = Vec::new();
            out.retain(|b| {
                let new = !seen.contains(&&b.path);
                seen.push(&b.path);
                new
            });
        }
        Ok(out)
    }

    fn source(&self, b: &Block) -> Source {
        Source {
            block: b.name.clone(),
            root: self.roots[b.root].name.clone(),
            lines: None,
        }
    }
}

fn parse_add(add: &str) -> Result<(&str, &str)> {
    add.split_once('=')
        .filter(|(_, name)| !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| Error::new(format!("--add {add}: expected KIND=NAME")))
}

fn apply_adds(p: &mut Profile, adds: &[String]) -> Result<()> {
    for add in adds {
        let (kind, name) = parse_add(add)?;
        let name = name.to_string();
        let push = |b: &mut Blocks| {
            if let Blocks::List(v) = b {
                v.push(name.clone());
            }
        };
        match kind.split_once('.') {
            None => match kind {
                "rules" => p.rules.push(name),
                "skills" => push(&mut p.skills),
                "commands" => push(&mut p.commands),
                _ => return Err(bad_kind(add)),
            },
            Some((k, h)) => {
                let h = Harness::from_name(h).ok_or_else(|| bad_kind(add))?;
                match k {
                    "settings" => p.settings.get_mut(h).push(name),
                    "files" => p.files.get_mut(h).push(name),
                    _ => return Err(bad_kind(add)),
                }
            }
        }
    }
    Ok(())
}

fn bad_kind(add: &str) -> Error {
    Error::new(format!(
        "--add {add}: KIND must be rules, skills, commands, settings.<harness> or files.<harness>"
    ))
}

fn valid_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn merge_vars(profile_vars: &[(String, String)], args: &[String]) -> Result<Vec<(String, String)>> {
    let mut vars = profile_vars.to_vec();
    for arg in args {
        let (name, value) = arg
            .split_once('=')
            .filter(|(n, _)| valid_var_name(n))
            .ok_or_else(|| Error::new(format!("--var {arg}: expected NAME=VALUE")))?;
        match vars.iter_mut().find(|(n, _)| n == name) {
            Some(v) => v.1 = value.to_string(),
            None => vars.push((name.to_string(), value.to_string())),
        }
    }
    Ok(vars)
}

fn select_harnesses(p: &Profile, wanted: &[Harness]) -> Result<Vec<Harness>> {
    for h in wanted {
        if !p.harnesses.contains(h) {
            return Err(Error::new(format!(
                "harness `{}` is not in the profile's harnesses ({})",
                h.name(),
                p.harnesses
                    .iter()
                    .map(|h| h.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(p.harnesses
        .iter()
        .copied()
        .filter(|h| wanted.is_empty() || wanted.contains(h))
        .collect())
}

/// Renders the profile's blocks for each selected harness, in memory.
pub fn render(
    profile: &Profile,
    profile_file: &Path,
    root_paths: &[PathBuf],
    opts: &Options,
) -> Result<Render> {
    let roots: Vec<Root> = root_paths
        .iter()
        .map(|p| Root {
            name: p
                .file_name()
                .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into()),
            path: p.clone(),
        })
        .collect();
    let blocks = discover(&roots)?;
    let mut profile = profile.clone();
    apply_adds(&mut profile, opts.adds)?;
    let vars = merge_vars(&profile.vars, opts.vars)?;
    let tiers = tiers::load(&roots)?;
    let pool = match &profile.pool {
        None => None,
        Some(name) => Some(tiers.pool(name).ok_or_else(|| {
            let have: Vec<_> = tiers.pools.iter().map(|p| p.name.as_str()).collect();
            let have = if have.is_empty() {
                "(no tiers.yaml in the roots)".to_string()
            } else {
                format!("(pools: {})", have.join(", "))
            };
            Error::new(format!(
                "profile `{}`: pool `{name}` is not in the tiers manifest {have}",
                profile.name
            ))
        })?),
    };
    let ctx = Ctx {
        blocks: &blocks,
        roots: &roots,
        optional: &profile.optional,
        profile_file: profile_file.display().to_string(),
        profile_root: profile_root(&roots, profile_file),
        pool,
        container: opts.container,
    };
    let mut harnesses = Vec::new();
    for h in select_harnesses(&profile, opts.harnesses)? {
        harnesses.push(render_harness(&ctx, &profile, &vars, h)?);
    }
    Ok(Render {
        profile: profile.name.clone(),
        roots,
        harnesses,
        library: blocks,
    })
}

/// The name of the root that holds the profile file: the deepest root it is
/// under. A profile outside every root (only a test has one) counts for the
/// first root.
fn profile_root(roots: &[Root], profile_file: &Path) -> String {
    roots
        .iter()
        .filter(|r| profile_file.starts_with(&r.path))
        .max_by_key(|r| r.path.components().count())
        .or(roots.first())
        .map_or_else(String::new, |r| r.name.clone())
}

type Vars = [(String, String)];

/// What a placeholder can see: the variables, the profile's pool and the
/// harness being rendered.
struct Tpl<'a> {
    vars: &'a Vars,
    pool: Option<&'a Pool>,
    harness: Harness,
}

fn render_harness(ctx: &Ctx, p: &Profile, vars: &Vars, h: Harness) -> Result<HarnessRender> {
    let tpl = &Tpl {
        vars,
        pool: ctx.pool,
        harness: h,
    };
    let rules = ctx.resolve(Kind::Rules, None, &p.rules, h)?;
    let skills = match &p.skills {
        Blocks::All => ctx.all(Kind::Skills),
        Blocks::List(v) => ctx.resolve(Kind::Skills, None, v, h)?,
    };
    let commands = match &p.commands {
        Blocks::All => ctx.all(Kind::Commands),
        Blocks::List(v) => ctx.resolve(Kind::Commands, None, v, h)?,
    };
    let settings = ctx.resolve(Kind::Settings, Some(h), p.settings.get(h), h)?;
    let bundles = ctx.resolve(Kind::Files, Some(h), p.files.get(h), h)?;

    let mut files = Files(BTreeMap::new());
    if let Some(f) = render_rules(ctx, &rules, tpl, h)? {
        files.put(h, f)?;
    }
    for b in &skills {
        for f in skill_files(ctx, b, tpl)? {
            files.put(h, f)?;
        }
    }
    for b in &commands {
        let path = format!("{}/{}.md", commands_dir(h), b.name);
        files.put(h, text_file(ctx, b, path, Kind::Commands, tpl)?)?;
    }
    if let Some(f) = render_settings(ctx, p, &settings, tpl, h)? {
        files.put(h, f)?;
    }
    for b in &bundles {
        for f in bundle_files(ctx, b)? {
            files.put(h, f)?;
        }
    }
    let used = [
        (Kind::Rules, &rules),
        (Kind::Skills, &skills),
        (Kind::Commands, &commands),
        (Kind::Settings, &settings),
        (Kind::Files, &bundles),
    ]
    .into_iter()
    .flat_map(|(kind, list)| {
        list.iter().map(move |b| Used {
            kind,
            block: b.name.clone(),
            root: ctx.roots[b.root].name.clone(),
        })
    })
    .collect();
    Ok(HarnessRender {
        harness: h,
        files: files.0.into_values().collect(),
        used,
    })
}

/// The files of one harness by path; a path produced twice is an error.
struct Files(BTreeMap<String, File>);

impl Files {
    fn put(&mut self, h: Harness, f: File) -> Result<()> {
        let describe = |f: &File| {
            let s = &f.sources[0];
            format!("{} block `{}` ({})", f.kind.name(), s.block, s.root)
        };
        if let Some(old) = self.0.get(&f.path) {
            return Err(Error::new(format!(
                "{}: {} is produced twice, by {} and {}",
                h.name(),
                f.path,
                describe(old),
                describe(&f)
            )));
        }
        if f.path == MANIFEST {
            return Err(Error::new(format!(
                "{}: {} would replace bree's manifest",
                h.name(),
                describe(&f)
            )));
        }
        self.0.insert(f.path.clone(), f);
        Ok(())
    }
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(io_err(path))?;
    String::from_utf8(bytes).map_err(|_| Error::new(format!("{}: not valid UTF-8", path.display())))
}

fn render_rules(ctx: &Ctx, blocks: &[&Block], tpl: &Tpl, h: Harness) -> Result<Option<File>> {
    if blocks.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    let mut sources = Vec::new();
    let mut line = 1;
    for (i, b) in blocks.iter().enumerate() {
        let body = template_file(&read_text(&b.path)?, tpl, &b.path.display().to_string())?;
        let body = body.trim_end_matches('\n');
        let count = body.matches('\n').count() + 1;
        if i > 0 {
            text.push_str("\n\n");
            line += 1;
        }
        text.push_str(body);
        let mut source = ctx.source(b);
        source.lines = Some((line, line + count - 1));
        sources.push(source);
        line += count;
    }
    text.push('\n');
    Ok(Some(File {
        path: rules_file(h).to_string(),
        bytes: text.into_bytes(),
        kind: Kind::Rules,
        sources,
        executable: false,
    }))
}

/// A command: one templated text file.
fn text_file(ctx: &Ctx, b: &Block, path: String, kind: Kind, tpl: &Tpl) -> Result<File> {
    let text = template_file(&read_text(&b.path)?, tpl, &b.path.display().to_string())?;
    Ok(File {
        path,
        bytes: text.into_bytes(),
        kind,
        sources: vec![ctx.source(b)],
        executable: false,
    })
}

/// Every regular file under `dir`, as (relative path, full path), sorted.
fn walk(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn go(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for (name, path) in list_dir(dir)? {
            let rel = format!("{prefix}{name}");
            if path.is_dir() {
                go(&path, &format!("{rel}/"), out)?;
            } else if path.is_file() {
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(dir, "", &mut out)?;
    Ok(out)
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// A skill's files; the ones that are valid UTF-8 are templated.
fn skill_files(ctx: &Ctx, b: &Block, tpl: &Tpl) -> Result<Vec<File>> {
    let mut out = Vec::new();
    for (rel, path) in walk(&b.path)? {
        let mut bytes = fs::read(&path).map_err(io_err(&path))?;
        if let Ok(text) = std::str::from_utf8(&bytes) {
            bytes = template_file(text, tpl, &path.display().to_string())?.into_bytes();
        }
        out.push(File {
            path: format!("skills/{}/{rel}", b.name),
            bytes,
            kind: Kind::Skills,
            sources: vec![ctx.source(b)],
            executable: is_executable(&path),
        });
    }
    Ok(out)
}

/// A files bundle: every file as it is, never templated.
fn bundle_files(ctx: &Ctx, b: &Block) -> Result<Vec<File>> {
    let mut out = Vec::new();
    for (rel, path) in walk(&b.path)? {
        out.push(File {
            path: rel,
            bytes: fs::read(&path).map_err(io_err(&path))?,
            kind: Kind::Files,
            sources: vec![ctx.source(b)],
            executable: is_executable(&path),
        });
    }
    Ok(out)
}

/// A settings file: the seeds the renderer sets itself, then the profile's
/// blocks patched over them in order. Both the seeds and the blocks are
/// sources of the file, in that order.
fn render_settings(
    ctx: &Ctx,
    p: &Profile,
    blocks: &[&Block],
    tpl: &Tpl,
    h: Harness,
) -> Result<Option<File>> {
    // The description of the container, for Claude Code on the container
    // target.
    let entry = (ctx.container && h == Harness::Claude).then(|| automode::entry(p));
    let auto_mode = entry.as_deref().map(|entry| {
        let source = Source {
            block: automode::SOURCE.to_string(),
            root: ctx.profile_root.clone(),
            lines: None,
        };
        (automode::seed(entry), vec![source])
    });
    let seeds: Vec<(Value, Vec<Source>)> =
        auto_mode.into_iter().chain(tier_env(tpl.pool, h)).collect();
    if blocks.is_empty() && seeds.is_empty() {
        return Ok(None);
    }
    let mut merged = Value::Object(Map::new());
    let mut sources = Vec::new();
    for (seed, from) in seeds {
        merge_patch(&mut merged, seed);
        sources.extend(from);
    }
    for b in blocks {
        let file = b.path.display().to_string();
        let mut text = read_text(&b.path)?;
        if file.ends_with(".jsonc") {
            text = strip_comments(&text).map_err(|e| Error::new(format!("{file}: {e}")))?;
            text = strip_trailing_commas(&text);
        }
        let mut patch: Value =
            serde_json::from_str(&text).map_err(|e| Error::new(format!("{file}: {e}")))?;
        if !patch.is_object() {
            return Err(Error::new(format!(
                "{file}: a settings block must be a JSON object"
            )));
        }
        substitute(&mut patch, tpl, &file, "")?;
        merge_patch(&mut merged, patch);
    }
    if let Some(entry) = &entry {
        automode::keep_entry(&mut merged, entry);
    }
    sources.extend(blocks.iter().map(|b| ctx.source(b)));
    let mut text = serde_json::to_string_pretty(&merged).expect("a Value serializes");
    text.push('\n');
    Ok(Some(File {
        path: settings_file(h).to_string(),
        bytes: text.into_bytes(),
        kind: Kind::Settings,
        sources,
        executable: false,
    }))
}

/// The settings Claude Code starts from on a pool other than `claude`: its
/// model aliases point at the pool's `big`, `medium` and `small` tiers. The
/// source is the pool's `tiers.yaml`.
fn tier_env(pool: Option<&Pool>, h: Harness) -> Option<(Value, Vec<Source>)> {
    let pool = pool.filter(|p| h == Harness::Claude && p.name != "claude")?;
    let env: Map<String, Value> = [
        ("big", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
        ("medium", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
        ("small", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
    ]
    .into_iter()
    .filter_map(|(tier, var)| Some((var.to_string(), pool.resolve(tier)?.id(h).into())))
    .collect();
    let source = Source {
        block: "tiers.yaml".to_string(),
        root: pool.root.clone(),
        lines: None,
    };
    (!env.is_empty()).then(|| (json!({ "env": env }), vec![source]))
}

/// Templates the string values (not the keys) of a parsed settings block.
fn substitute(v: &mut Value, tpl: &Tpl, file: &str, pointer: &str) -> Result<()> {
    match v {
        Value::String(s) => {
            *s = template(s, tpl)
                .map_err(|e| Error::new(format!("{file}: {pointer}: {}", e.message)))?;
        }
        Value::Array(a) => {
            for (i, x) in a.iter_mut().enumerate() {
                substitute(x, tpl, file, &format!("{pointer}/{i}"))?;
            }
        }
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                let k = k.replace('~', "~0").replace('/', "~1");
                substitute(x, tpl, file, &format!("{pointer}/{k}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// JSON Merge Patch (RFC 7396).
pub fn merge_patch(target: &mut Value, patch: Value) {
    let Value::Object(patch) = patch else {
        *target = patch;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(map) = target else {
        unreachable!("target was just made an object")
    };
    for (k, v) in patch {
        if v.is_null() {
            map.shift_remove(&k);
        } else {
            merge_patch(map.entry(k).or_insert(Value::Null), v);
        }
    }
}

/// Removes `//` and `/* */` comments outside strings, for `.jsonc` input.
pub fn strip_comments(text: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(s) = chars.next() {
                    out.push(s);
                    if s == '\\' {
                        out.extend(chars.next());
                    } else if s == '"' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => while chars.next_if(|&n| n != '\n').is_some() {},
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                loop {
                    match chars.next() {
                        Some('/') if prev == '*' => break,
                        Some(n) => prev = n,
                        None => return Err("unterminated /* comment".to_string()),
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// Removes a comma before a closing `}` or `]` outside strings, for `.jsonc`
/// input after `strip_comments`. opencode reads its config with trailing
/// commas allowed, and configs vendored for it have them.
pub fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(s) = chars.next() {
                    out.push(s);
                    if s == '\\' {
                        out.extend(chars.next());
                    } else if s == '"' {
                        break;
                    }
                }
            }
            ',' => {
                let mut ws = String::new();
                while let Some(w) = chars.next_if(|n| n.is_whitespace()) {
                    ws.push(w);
                }
                if !matches!(chars.peek(), Some('}') | Some(']')) {
                    out.push(c);
                }
                out.push_str(&ws);
            }
            _ => out.push(c),
        }
    }
    out
}

// ------------------------------------------------------------- templates

struct TemplateError {
    line: usize,
    message: String,
}

fn template_file(text: &str, tpl: &Tpl, file: &str) -> Result<String> {
    template(text, tpl).map_err(|e| Error::new(format!("{file}:{}: {}", e.line, e.message)))
}

fn template(text: &str, tpl: &Tpl) -> std::result::Result<String, TemplateError> {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        expand_line(line, tpl, &mut out).map_err(|message| TemplateError {
            line: i + 1,
            message,
        })?;
    }
    Ok(out)
}

fn expand_line(line: &str, tpl: &Tpl, out: &mut String) -> std::result::Result<(), String> {
    let mut rest = line;
    while let Some(i) = rest.find("{{") {
        let (before, after) = (&rest[..i], &rest[i + 2..]);
        if let Some(escaped) = before.strip_suffix('\\') {
            out.push_str(escaped);
            out.push_str("{{");
            rest = after;
            continue;
        }
        out.push_str(before);
        let end = after
            .find("}}")
            .ok_or("`{{` without a closing `}}` on the same line")?;
        out.push_str(&placeholder(&after[..end], tpl)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(())
}

fn placeholder<'a>(content: &str, tpl: &'a Tpl) -> std::result::Result<Cow<'a, str>, String> {
    if let Some(name) = content.strip_prefix("var.") {
        if valid_var_name(name) {
            return match tpl.vars.iter().find(|(n, _)| n == name) {
                Some((_, value)) => Ok(value.into()),
                None => Err(format!("unknown variable `{name}`")),
            };
        }
    }
    if let Some(rest) = content.strip_prefix("tier.") {
        let (name, display) = rest
            .strip_suffix(".name")
            .map_or((rest, false), |n| (n, true));
        if tiers::valid_name(name) {
            let pool = tpl.pool.ok_or_else(|| {
                format!("`{{{{{content}}}}}` needs a pool, and the profile names none")
            })?;
            let tier = pool.resolve(name).ok_or_else(|| {
                format!(
                    "unknown tier `{name}` in pool `{}` (it has {})",
                    pool.name,
                    pool.tier_names().join(", ")
                )
            })?;
            return Ok(if display {
                tier.display().into()
            } else {
                tier.id(tpl.harness).into()
            });
        }
    }
    Err(format!("{{{{{content}}}}}: unknown placeholder"))
}

// -------------------------------------------------------------- manifest

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The value with every object's keys sorted.
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// sha256 of the bytes; a `.json` file that parses is hashed in canonical
/// form, so a harness rewriting its key order does not change it.
pub fn digest(path: &str, bytes: &[u8]) -> String {
    let parsed = path
        .ends_with(".json")
        .then(|| serde_json::from_slice::<Value>(bytes).ok())
        .flatten();
    match parsed {
        Some(v) => hex(&Sha256::digest(canonical(&v).to_string().as_bytes())),
        None => hex(&Sha256::digest(bytes)),
    }
}

/// The manifest of one harness's files: what `.bree-manifest.json` holds, and
/// what `host.json` holds for each harness.
pub fn manifest(render: &Render, h: &HarnessRender) -> Value {
    let roots: Vec<Value> = render
        .roots
        .iter()
        .map(|r| json!({"name": r.name, "path": r.path.display().to_string()}))
        .collect();
    let files: Vec<Value> = h
        .files
        .iter()
        .map(|f| {
            let sources: Vec<Value> = f
                .sources
                .iter()
                .map(|s| {
                    let mut v = json!({"block": s.block, "root": s.root});
                    if let Some((a, b)) = s.lines {
                        v["lines"] = json!([a, b]);
                    }
                    v
                })
                .collect();
            json!({
                "path": f.path,
                "sha256": digest(&f.path, &f.bytes),
                "kind": f.kind.name(),
                "sources": sources,
            })
        })
        .collect();
    json!({
        "bree": env!("CARGO_PKG_VERSION"),
        "profile": render.profile,
        "harness": h.harness.name(),
        "roots": roots,
        "files": files,
    })
}

/// A path of an old manifest or state file, which only counts when it stays
/// inside the harness directory.
pub fn safe_relative(path: &str) -> bool {
    Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        && !path.is_empty()
}

/// `(path, sha256)` of the `files` of one manifest. `bad` words an error with
/// the place the manifest was read from.
pub fn manifest_files(v: &Value, bad: &dyn Fn(&str) -> Error) -> Result<Vec<(String, String)>> {
    let files = v["files"]
        .as_array()
        .ok_or_else(|| bad("no `files` list"))?;
    let mut out = Vec::new();
    for f in files {
        let (Some(p), Some(sha)) = (f["path"].as_str(), f["sha256"].as_str()) else {
            return Err(bad("a file entry needs `path` and `sha256`"));
        };
        if !safe_relative(p) {
            return Err(bad(&format!("unsafe path `{p}`")));
        }
        out.push((p.to_string(), sha.to_string()));
    }
    Ok(out)
}

/// `(path, sha256)` of the files an earlier render wrote, or none without a
/// manifest.
pub fn read_manifest(dir: &Path) -> Result<Option<Vec<(String, String)>>> {
    let path = dir.join(MANIFEST);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(&path)(e)),
    };
    let bad = |what: &str| Error::new(format!("{}: {what}", path.display()));
    let v: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
    manifest_files(&v, &bad).map(Some)
}

// --------------------------------------------------------------- writing

/// Rows of cells as text, every column but the last padded to the widest cell
/// plus two spaces.
pub fn align(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter(|r| c + 1 < r.len())
                .map(|r| r[c].chars().count() + 2)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            out.push_str(cell);
            if c + 1 < row.len() {
                let pad = widths[c] - cell.chars().count();
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push('\n');
    }
    out
}

/// What a write does to one rendered file; the name is the state in its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Installed,
    Updated,
    Current,
}

impl Change {
    pub fn name(self) -> &'static str {
        match self {
            Change::Installed => "installed",
            Change::Updated => "updated",
            Change::Current => "current",
        }
    }
}

/// What a write did, for the caller to print.
#[derive(Debug, Default)]
pub struct Written {
    pub rows: Vec<Vec<String>>,
    pub written: usize,
    pub current: usize,
    pub removed: usize,
}

impl Written {
    /// Adds the rows of one harness: its files in order, then the files that
    /// were removed. `shown` turns a path in the harness directory into the
    /// path the row shows.
    pub fn add(
        &mut self,
        h: Harness,
        shown: impl Fn(&str) -> String,
        files: &[(&str, Change)],
        removed: &[&str],
    ) {
        let row =
            |path: &str, state: &str| vec![h.name().to_string(), shown(path), state.to_string()];
        for (path, change) in files {
            self.rows.push(row(path, change.name()));
            if *change == Change::Current {
                self.current += 1;
            } else {
                self.written += 1;
            }
        }
        for path in removed {
            self.rows.push(row(path, "removed"));
            self.removed += 1;
        }
    }

    /// The rows aligned, then the summary line, as `bree render` prints them.
    pub fn text(&self) -> String {
        format!(
            "{}bree: {} written, {} current, {} removed\n",
            align(&self.rows),
            self.written,
            self.current,
            self.removed
        )
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().expect("a file has a parent");
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let tmp = dir.join(format!(".bree-tmp-{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(io_err(&tmp))?;
    fs::rename(&tmp, path).map_err(io_err(path))
}

/// Removes empty directories from `dir` up to, not including, `stop`.
fn prune_empty(mut dir: &Path, stop: &Path) {
    while dir != stop && fs::remove_dir(dir).is_ok() {
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
}

/// Carries out what was decided for one harness under `base`: writes the
/// files that are not current and bree's copy of every file, then removes the
/// files `removals` names (a path in the harness directory) with their copies,
/// and the directories that stay empty.
pub fn apply(base: &Path, h: &HarnessRender, changes: &[Change], removals: &[&str]) -> Result<()> {
    let dir = base.join(harness_dir(h.harness));
    for (f, change) in h.files.iter().zip(changes) {
        if *change != Change::Current {
            write_atomic(&dir.join(&f.path), &f.bytes, f.executable)?;
        }
        write_atomic(&rendered_copy(base, h.harness, &f.path), &f.bytes, false)?;
    }
    let copies = bookkeeping(base).join("rendered").join(h.harness.name());
    for path in removals {
        let copy = rendered_copy(base, h.harness, path);
        for target in [dir.join(path), copy.clone()] {
            match fs::remove_file(&target) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(io_err(&target)(e));
                }
                _ => {}
            }
        }
        prune_empty(dir.join(path).parent().unwrap(), &dir);
        prune_empty(copy.parent().unwrap(), &copies);
    }
    Ok(())
}

/// Writes the render under `base` (the harness directories, the manifests and
/// bree's copies) and removes files an earlier render wrote that this one
/// does not. `shown` is the base as the user typed it. This is the container
/// and `--out` semantics: whatever is on the disk is overwritten.
pub fn write(render: &Render, base: &Path, shown: &Path) -> Result<Written> {
    let mut w = Written::default();
    for h in &render.harnesses {
        let rel = harness_dir(h.harness);
        let dir = base.join(rel);
        let old = read_manifest(&dir)?.unwrap_or_default();
        let mut changes = Vec::new();
        for f in &h.files {
            let target = dir.join(&f.path);
            changes.push(match fs::read(&target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Change::Installed,
                Err(e) => return Err(io_err(&target)(e)),
                Ok(disk) if disk != f.bytes || (f.executable && !is_executable(&target)) => {
                    Change::Updated
                }
                Ok(_) => Change::Current,
            });
        }
        let removals: Vec<&str> = old
            .iter()
            .map(|(path, _)| path.as_str())
            .filter(|path| !h.files.iter().any(|f| &f.path == path))
            .collect();
        apply(base, h, &changes, &removals)?;
        let mut bytes = serde_json::to_string_pretty(&manifest(render, h)).expect("serializes");
        bytes.push('\n');
        write_atomic(&dir.join(MANIFEST), bytes.as_bytes(), false)?;

        let files: Vec<(&str, Change)> = h
            .files
            .iter()
            .map(|f| f.path.as_str())
            .zip(changes)
            .collect();
        w.add(
            h.harness,
            |path| shown.join(rel).join(path).display().to_string(),
            &files,
            &removals,
        );
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::{TempDir, env};

    fn put(root: &Path, path: &str, text: &str) {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// A root named `name` under `t` with the given files.
    fn root(t: &TempDir, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let r = t.0.join(name);
        for (p, text) in files {
            put(&r, p, text);
        }
        r
    }

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// Renders for the host target, which the tests of blocks and merges use:
    /// the auto mode seed has its own tests, which render as the container
    /// with `try_render_as`.
    fn try_render(
        t: &TempDir,
        roots: &[PathBuf],
        profile: &str,
        adds: &[&str],
        extra: &[&str],
    ) -> Result<Render> {
        try_render_as(t, roots, profile, adds, extra, false)
    }

    fn try_render_as(
        t: &TempDir,
        roots: &[PathBuf],
        profile: &str,
        adds: &[&str],
        extra: &[&str],
        container: bool,
    ) -> Result<Render> {
        let file = t.0.join("profile.yaml");
        fs::write(&file, profile).unwrap();
        let p = crate::profile::load(&file, "test", &env()).unwrap();
        let adds: Vec<String> = adds.iter().map(|s| s.to_string()).collect();
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let opts = Options {
            harnesses: &[],
            adds: &adds,
            vars: &extra,
            container,
        };
        render(&p, &file, roots, &opts)
    }

    fn do_render(t: &TempDir, roots: &[PathBuf], profile: &str) -> Render {
        try_render(t, roots, profile, &[], &[]).unwrap()
    }

    fn file<'a>(r: &'a Render, h: Harness, path: &str) -> &'a File {
        let hr = r.harnesses.iter().find(|x| x.harness == h).unwrap();
        hr.files.iter().find(|f| f.path == path).unwrap_or_else(|| {
            panic!(
                "no {path} in {:?}",
                hr.files.iter().map(|f| &f.path).collect::<Vec<_>>()
            )
        })
    }

    fn text(r: &Render, h: Harness, path: &str) -> String {
        String::from_utf8(file(r, h, path).bytes.clone()).unwrap()
    }

    fn paths(r: &Render, h: Harness) -> Vec<String> {
        let hr = r.harnesses.iter().find(|x| x.harness == h).unwrap();
        hr.files.iter().map(|f| f.path.clone()).collect()
    }

    const BASIC: &[(&str, &str)] = &[
        ("rules/a.md", "alpha\n\n\n"),
        ("rules/b.md", "beta1\nbeta2\n"),
        ("rules/c.md", "gamma"),
    ];

    #[test]
    fn rules_are_concatenated_with_line_ranges() {
        let t = TempDir::new("rules");
        let r = root(&t, "r", BASIC);
        let out = do_render(&t, &[r], "harness: claude\nrules: [a, b, c]\n");
        assert_eq!(
            text(&out, Harness::Claude, "CLAUDE.md"),
            "alpha\n\nbeta1\nbeta2\n\ngamma\n"
        );
        let lines: Vec<_> = file(&out, Harness::Claude, "CLAUDE.md")
            .sources
            .iter()
            .map(|s| (s.block.as_str(), s.lines.unwrap()))
            .collect();
        assert_eq!(lines, [("a", (1, 1)), ("b", (3, 4)), ("c", (6, 6))]);
    }

    #[test]
    fn rules_of_pi_and_opencode_go_to_agents_md_and_no_rules_no_file() {
        let t = TempDir::new("agents");
        let r = root(&t, "r", BASIC);
        let out = do_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nharnesses: [pi, opencode]\nrules: [a]\n",
        );
        assert_eq!(text(&out, Harness::Pi, "AGENTS.md"), "alpha\n");
        assert_eq!(text(&out, Harness::Opencode, "AGENTS.md"), "alpha\n");
        let out = do_render(&t, &[r], "harness: pi\n");
        assert!(paths(&out, Harness::Pi).is_empty());
    }

    #[test]
    fn harness_entries_apply_to_that_harness_only() {
        let t = TempDir::new("prefix");
        let r = root(&t, "r", BASIC);
        let out = do_render(
            &t,
            &[r],
            "harness: claude\nharnesses: [claude, pi]\nrules: [a, 'claude:b', 'pi:c']\n",
        );
        assert_eq!(
            text(&out, Harness::Claude, "CLAUDE.md"),
            "alpha\n\nbeta1\nbeta2\n"
        );
        assert_eq!(text(&out, Harness::Pi, "AGENTS.md"), "alpha\n\ngamma\n");
    }

    #[test]
    fn all_takes_every_block_sorted_and_add_to_all_is_a_no_op() {
        let t = TempDir::new("all");
        let r = root(
            &t,
            "r",
            &[
                ("skills/zed/SKILL.md", "z\n"),
                ("skills/abc/SKILL.md", "a\n"),
                ("commands/x.md", "x\n"),
            ],
        );
        let out = try_render(
            &t,
            &[r],
            "harness: claude\nskills: all\ncommands: all\n",
            &["skills=abc", "commands=x"],
            &[],
        )
        .unwrap();
        assert_eq!(
            paths(&out, Harness::Claude),
            [
                "commands/x.md",
                "skills/abc/SKILL.md",
                "skills/zed/SKILL.md"
            ]
        );
        let used: Vec<_> = out.harnesses[0]
            .used
            .iter()
            .map(|u| u.block.as_str())
            .collect();
        assert_eq!(used, ["abc", "zed", "x"]);
    }

    #[test]
    fn optional_blocks_are_skipped_when_absent_and_used_when_present() {
        let t = TempDir::new("optional");
        let public = root(&t, "public", BASIC);
        let private = root(&t, "private", &[("rules/secret.md", "internal\n")]);
        let profile = "harness: claude\nrules: [a, secret]\noptional: [secret]\n";
        let out = do_render(&t, std::slice::from_ref(&public), profile);
        assert_eq!(text(&out, Harness::Claude, "CLAUDE.md"), "alpha\n");
        let out = do_render(&t, &[public, private], profile);
        assert_eq!(
            text(&out, Harness::Claude, "CLAUDE.md"),
            "alpha\n\ninternal\n"
        );
    }

    #[test]
    fn missing_block_names_the_profile_and_the_list() {
        let t = TempDir::new("missing");
        let r = root(&t, "r", BASIC);
        let e = try_render(
            &t,
            &[r],
            "harness: claude\nsettings:\n  claude: [nope]\n",
            &[],
            &[],
        )
        .unwrap_err();
        let want = format!(
            "{}: settings.claude: block `nope` not found",
            t.0.join("profile.yaml").display()
        );
        assert!(e.0.starts_with(&want), "{e}");
    }

    #[test]
    fn a_block_in_two_roots_is_an_error_naming_both() {
        let t = TempDir::new("twice");
        let a = root(&t, "a", BASIC);
        let b = root(&t, "b", &[("rules/a.md", "other\n")]);
        let e = try_render(&t, &[a.clone(), b.clone()], "harness: claude\n", &[], &[]).unwrap_err();
        assert_eq!(
            e.0,
            format!(
                "rules block `a` is defined twice: {} and {}",
                a.join("rules/a.md").display(),
                b.join("rules/a.md").display()
            )
        );
    }

    #[test]
    fn json_and_jsonc_for_one_name_is_defined_twice() {
        let t = TempDir::new("jsonc-twice");
        let r = root(
            &t,
            "r",
            &[("settings/pi/x.json", "{}"), ("settings/pi/x.jsonc", "{}")],
        );
        let e = try_render(&t, &[r], "harness: pi\n", &[], &[]).unwrap_err();
        assert!(
            e.0.starts_with("settings.pi block `x` is defined twice"),
            "{e}"
        );
    }

    #[test]
    fn a_skill_without_skill_md_is_an_error() {
        let t = TempDir::new("noskill");
        let r = root(&t, "r", &[("skills/s/notes.md", "x")]);
        let e = try_render(&t, &[r], "harness: pi\n", &[], &[]).unwrap_err();
        assert!(e.0.ends_with("skills/s has no SKILL.md"), "{e}");
    }

    fn patch(target: &str, patch: &str) -> Value {
        let mut t: Value = serde_json::from_str(target).unwrap();
        merge_patch(&mut t, serde_json::from_str(patch).unwrap());
        t
    }

    #[test]
    fn merge_patch_follows_rfc_7396() {
        let j = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            patch(r#"{"a":{"b":1,"c":2},"d":1}"#, r#"{"a":{"b":9,"e":3}}"#),
            j(r#"{"a":{"b":9,"c":2,"e":3},"d":1}"#)
        );
        assert_eq!(patch(r#"{"a":[1,2,3]}"#, r#"{"a":[4]}"#), j(r#"{"a":[4]}"#));
        assert_eq!(patch(r#"{"a":1,"b":2}"#, r#"{"a":null}"#), j(r#"{"b":2}"#));
        assert_eq!(
            patch(r#"{"a":1}"#, r#"{"a":{"b":null,"c":1}}"#),
            j(r#"{"a":{"c":1}}"#)
        );
        assert_eq!(patch(r#"{"a":{"b":1}}"#, r#"{"a":"x"}"#), j(r#"{"a":"x"}"#));
        assert_eq!(patch(r#"{"a":1}"#, "[1]"), j("[1]"));
    }

    #[test]
    fn settings_are_merged_in_order_and_non_objects_are_refused() {
        let t = TempDir::new("settings");
        let r = root(
            &t,
            "r",
            &[
                ("settings/claude/a.json", r#"{"x":{"y":1},"drop":true}"#),
                (
                    "settings/claude/b.jsonc",
                    "{\n // c\n \"x\": {\"z\": 2}, \"drop\": null /* c */\n}",
                ),
                ("settings/claude/list.json", "[1]"),
            ],
        );
        let out = do_render(
            &t,
            std::slice::from_ref(&r),
            "harness: claude\nsettings:\n  claude: [a, b]\n",
        );
        assert_eq!(
            text(&out, Harness::Claude, "settings.json"),
            "{\n  \"x\": {\n    \"y\": 1,\n    \"z\": 2\n  }\n}\n"
        );
        let e = try_render(
            &t,
            &[r],
            "harness: claude\nsettings:\n  claude: [list]\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert!(
            e.0.ends_with("a settings block must be a JSON object"),
            "{e}"
        );
    }

    #[test]
    fn opencode_settings_go_to_opencode_json() {
        let t = TempDir::new("oc");
        let r = root(&t, "r", &[("settings/opencode/a.json", r#"{"a":1}"#)]);
        let out = do_render(&t, &[r], "harness: opencode\nsettings:\n  opencode: [a]\n");
        assert_eq!(
            text(&out, Harness::Opencode, "opencode.json"),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn jsonc_comments_are_stripped_outside_strings_only() {
        let s =
            strip_comments("{\"a\": \"x // y /* z */\", // end\n \"b\": 1 /* c \n d */}").unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["a"], "x // y /* z */");
        assert_eq!(v["b"], 1);
        let s = strip_comments(r#"{"a": "q\"// not a comment"}"#).unwrap();
        assert_eq!(s, r#"{"a": "q\"// not a comment"}"#);
        assert!(strip_comments("{} /* open").is_err());
    }

    #[test]
    fn jsonc_trailing_commas_are_dropped_outside_strings_only() {
        let jsonc = "{\"a\": {\"b\": 1, // last\n },\n \"c\": [1, 2 , ],\n \"d\": \",}\",\n}";
        let s = strip_trailing_commas(&strip_comments(jsonc).unwrap());
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["a"]["b"], 1);
        assert_eq!(v["c"], serde_json::json!([1, 2]));
        assert_eq!(v["d"], ",}");
        // Line numbers in later parse errors stay those of the source.
        assert_eq!(s.lines().count(), jsonc.lines().count());
        // A comma that is not trailing is kept, so a missing value still fails.
        assert!(serde_json::from_str::<Value>(&strip_trailing_commas("{\"a\":1,,}")).is_err());
    }

    fn tpl(text: &str, v: &[(&str, &str)]) -> std::result::Result<String, String> {
        let vars = vars(v);
        let tpl = Tpl {
            vars: &vars,
            pool: None,
            harness: Harness::Claude,
        };
        template(text, &tpl).map_err(|e| format!("{}: {}", e.line, e.message))
    }

    #[test]
    fn templates_substitute_variables_and_escape() {
        assert_eq!(
            tpl("a {{var.x}} b {{var.y-1}}\n", &[("x", "1"), ("y-1", "2")]).unwrap(),
            "a 1 b 2\n"
        );
        assert_eq!(
            tpl("\\{{var.x}} and {{var.x}}", &[("x", "1")]).unwrap(),
            "{{var.x}} and 1"
        );
        assert_eq!(tpl("no placeholders", &[]).unwrap(), "no placeholders");
    }

    #[test]
    fn template_errors_have_the_line() {
        assert_eq!(
            tpl("a\n{{var.nope}}\n", &[]).unwrap_err(),
            "2: unknown variable `nope`"
        );
        assert_eq!(
            tpl("{{tier.big}}", &[]).unwrap_err(),
            "1: `{{tier.big}}` needs a pool, and the profile names none"
        );
        assert_eq!(
            tpl("{{foo}}", &[]).unwrap_err(),
            "1: {{foo}}: unknown placeholder"
        );
        assert_eq!(
            tpl("{{ var.x }}", &[("x", "1")]).unwrap_err(),
            "1: {{ var.x }}: unknown placeholder"
        );
        assert_eq!(
            tpl("ok\nbad {{var.x\nmore }}", &[("x", "1")]).unwrap_err(),
            "2: `{{` without a closing `}}` on the same line"
        );
    }

    #[test]
    fn var_flag_beats_the_profile_and_reaches_every_kind() {
        let t = TempDir::new("vars");
        let r = root(
            &t,
            "r",
            &[
                ("rules/v.md", "who {{var.who}}\n"),
                ("commands/c.md", "cmd {{var.who}}\n"),
                ("skills/s/SKILL.md", "skill {{var.who}}\n"),
                ("skills/s/data.bin", "\u{ff}"),
                (
                    "settings/claude/s.json",
                    r#"{"k":"{{var.who}}","{{var.who}}":1}"#,
                ),
                ("files/claude/f/raw.txt", "{{var.who}}\n"),
            ],
        );
        let profile = "harness: claude\nrules: [v]\nskills: [s]\ncommands: [c]\nsettings:\n  claude: [s]\nfiles:\n  claude: [f]\nvars:\n  who: profile\n";
        let out = try_render(&t, &[r], profile, &[], &["who=flag"]).unwrap();
        let c = Harness::Claude;
        assert_eq!(text(&out, c, "CLAUDE.md"), "who flag\n");
        assert_eq!(text(&out, c, "commands/c.md"), "cmd flag\n");
        assert_eq!(text(&out, c, "skills/s/SKILL.md"), "skill flag\n");
        assert_eq!(text(&out, c, "raw.txt"), "{{var.who}}\n");
        let s: Value = serde_json::from_str(&text(&out, c, "settings.json")).unwrap();
        assert_eq!(s["k"], "flag");
        assert_eq!(s["{{var.who}}"], 1, "keys are not templated");
        assert_eq!(
            file(&out, c, "skills/s/data.bin").bytes,
            "\u{ff}".as_bytes()
        );
    }

    #[test]
    fn unknown_variable_in_settings_names_the_pointer() {
        let t = TempDir::new("varptr");
        let r = root(
            &t,
            "r",
            &[("settings/pi/s.json", r#"{"a":["x",{"b/c":"{{var.q}}"}]}"#)],
        );
        let e = try_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nsettings:\n  pi: [s]\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(
            e.0,
            format!(
                "{}: /a/1/b~1c: unknown variable `q`",
                r.join("settings/pi/s.json").display()
            )
        );
    }

    #[test]
    fn bundle_files_conflict_with_each_other_and_with_rendered_files() {
        let t = TempDir::new("conflict");
        let r = root(
            &t,
            "r",
            &[
                ("files/pi/one/x.md", "1"),
                ("files/pi/two/x.md", "2"),
                ("files/pi/cfg/settings.json", "{}"),
                ("settings/pi/s.json", "{}"),
                ("commands/c.md", "c"),
                ("files/pi/cmd/prompts/c.md", "c"),
            ],
        );
        let e = try_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nfiles:\n  pi: [one, two]\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(
            e.0,
            "pi: x.md is produced twice, by files block `one` (r) and files block `two` (r)"
        );
        let e = try_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nsettings:\n  pi: [s]\nfiles:\n  pi: [cfg]\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert!(
            e.0.starts_with("pi: settings.json is produced twice"),
            "{e}"
        );
        let e = try_render(
            &t,
            &[r],
            "harness: pi\ncommands: [c]\nfiles:\n  pi: [cmd]\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.0.starts_with("pi: prompts/c.md is produced twice"), "{e}");
    }

    #[test]
    fn add_appends_to_each_kind_and_rejects_bad_ones() {
        let t = TempDir::new("add");
        let r = root(
            &t,
            "r",
            &[
                ("rules/a.md", "a\n"),
                ("skills/s/SKILL.md", "s\n"),
                ("commands/c.md", "c\n"),
                ("settings/pi/p.json", "{}"),
                ("files/pi/f/f.txt", "f"),
            ],
        );
        let adds = [
            "rules=a",
            "skills=s",
            "commands=c",
            "settings.pi=p",
            "files.pi=f",
        ];
        let out = try_render(&t, std::slice::from_ref(&r), "harness: pi\n", &adds, &[]).unwrap();
        assert_eq!(
            paths(&out, Harness::Pi),
            [
                "AGENTS.md",
                "f.txt",
                "prompts/c.md",
                "settings.json",
                "skills/s/SKILL.md"
            ]
        );
        for bad in [
            "rules",
            "rules=",
            "bogus=a",
            "settings.vim=a",
            "files=a",
            "rules.pi=a",
            "rules=a/b",
        ] {
            let e =
                try_render(&t, std::slice::from_ref(&r), "harness: pi\n", &[bad], &[]).unwrap_err();
            assert!(e.0.starts_with(&format!("--add {bad}:")), "{e}");
        }
    }

    #[test]
    fn a_harness_outside_the_profile_is_an_error() {
        let t = TempDir::new("harness");
        let r = root(&t, "r", BASIC);
        let file = t.0.join("p.yaml");
        fs::write(&file, "harness: claude\nharnesses: [claude, pi]\n").unwrap();
        let p = crate::profile::load(&file, "t", &env()).unwrap();
        let mut o = Options {
            harnesses: &[Harness::Opencode],
            adds: &[],
            vars: &[],
            container: false,
        };
        let e = render(&p, &file, std::slice::from_ref(&r), &o).unwrap_err();
        assert_eq!(
            e.0,
            "harness `opencode` is not in the profile's harnesses (claude, pi)"
        );
        o.harnesses = &[Harness::Pi];
        let out = render(&p, &file, &[r], &o).unwrap();
        assert_eq!(out.harnesses.len(), 1);
    }

    #[test]
    fn manifest_records_sources_lines_and_hashes() {
        let t = TempDir::new("manifest");
        let r = root(
            &t,
            "r",
            &[
                ("rules/a.md", "a\n"),
                ("rules/b.md", "b1\nb2\n"),
                ("settings/claude/s.json", r#"{"b":1,"a":{"d":1,"c":2}}"#),
            ],
        );
        let out = do_render(
            &t,
            &[r],
            "harness: claude\nrules: [a, b]\nsettings:\n  claude: [s]\n",
        );
        let m = manifest(&out, &out.harnesses[0]);
        assert_eq!(m["bree"], env!("CARGO_PKG_VERSION"));
        assert_eq!(m["profile"], "test");
        assert_eq!(m["harness"], "claude");
        assert_eq!(m["roots"][0]["name"], "r");
        let files = m["files"].as_array().unwrap();
        assert_eq!(files[0]["path"], "CLAUDE.md");
        assert_eq!(files[0]["kind"], "rules");
        assert_eq!(files[0]["sha256"], hex(&Sha256::digest(b"a\n\nb1\nb2\n")));
        assert_eq!(
            files[0]["sources"][1],
            json!({"block": "b", "root": "r", "lines": [3, 4]})
        );
        assert_eq!(files[1]["path"], "settings.json");
        assert_eq!(files[1]["sources"][0], json!({"block": "s", "root": "r"}));
        assert_eq!(
            files[1]["sha256"],
            hex(&Sha256::digest(br#"{"a":{"c":2,"d":1},"b":1}"#))
        );
    }

    #[test]
    fn json_digest_ignores_key_order_and_spacing_only_for_json() {
        assert_eq!(
            digest("x.json", br#"{"a":1,"b":{"c":2,"d":3}}"#),
            digest(
                "x.json",
                b"{\n \"b\": {\"d\": 3, \"c\": 2},\n \"a\": 1\n}\n"
            )
        );
        assert_ne!(
            digest("x.json", b"{\"a\":1}"),
            digest("x.json", b"{\"a\":2}")
        );
        assert_ne!(digest("x.md", b"{\"a\":1}"), digest("x.md", b"{ \"a\":1}"));
        assert_ne!(
            digest("x.json", b"not json"),
            digest("x.json", b"not  json")
        );
    }

    #[test]
    fn write_installs_updates_removes_and_prunes() {
        let t = TempDir::new("write");
        let r = root(
            &t,
            "r",
            &[
                ("rules/a.md", "a\n"),
                ("skills/s/SKILL.md", "s\n"),
                ("skills/s/sub/x.txt", "x\n"),
            ],
        );
        let first = do_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nrules: [a]\nskills: [s]\n",
        );
        let base = t.0.join("out");
        let w = write(&first, &base, Path::new("OUT")).unwrap();
        assert_eq!((w.written, w.current, w.removed), (3, 0, 0));
        assert_eq!(w.rows[0], ["pi", "OUT/.pi/agent/AGENTS.md", "installed"]);
        let dir = base.join(".pi/agent");
        assert!(dir.join("skills/s/sub/x.txt").is_file());
        assert!(rendered_copy(&base, Harness::Pi, "skills/s/sub/x.txt").is_file());
        assert!(dir.join(MANIFEST).is_file());

        fs::write(dir.join("AGENTS.md"), "edited\n").unwrap();
        fs::write(dir.join("unrelated.txt"), "mine\n").unwrap();
        let second = do_render(&t, &[r], "harness: pi\nrules: [a]\n");
        let w = write(&second, &base, Path::new("OUT")).unwrap();
        assert_eq!((w.written, w.current, w.removed), (1, 0, 2));
        let states: Vec<_> = w.rows.iter().map(|r| r[2].as_str()).collect();
        assert_eq!(states, ["updated", "removed", "removed"]);
        assert!(!dir.join("skills").exists(), "empty directories are pruned");
        assert!(!rendered_copy(&base, Harness::Pi, "skills").exists());
        assert!(dir.join("unrelated.txt").is_file());
        assert_eq!(fs::read_to_string(dir.join("AGENTS.md")).unwrap(), "a\n");
        let w = write(&second, &base, Path::new("OUT")).unwrap();
        assert_eq!((w.written, w.current, w.removed), (0, 1, 0));
    }

    #[test]
    fn a_manifest_path_outside_the_harness_directory_is_refused() {
        let t = TempDir::new("unsafe");
        put(
            &t.0,
            MANIFEST,
            r#"{"files":[{"path":"../x","sha256":"0"}]}"#,
        );
        let e = read_manifest(&t.0).unwrap_err();
        assert!(e.0.ends_with("unsafe path `../x`"), "{e}");
    }

    #[test]
    fn align_pads_every_column_but_the_last() {
        let rows = vec![
            vec!["a".to_string(), "long".to_string(), "x".to_string()],
            vec!["bbb".to_string(), "s".to_string(), "y".to_string()],
        ];
        assert_eq!(align(&rows), "a    long  x\nbbb  s     y\n");
    }

    const TIERS: &str = "pools:
  claude:
    big: {model: opus-1, name: Opus}
    small: {model: haiku-1}
  other:
    big: {model: glm, name: GLM, opencode: prov/glm}
    medium: {fallback: big}
";

    /// A root with the pools above and a rule, a command and a settings block
    /// that use tiers.
    fn tier_root(t: &TempDir) -> PathBuf {
        root(
            t,
            "r",
            &[
                ("tiers.yaml", TIERS),
                ("rules/a.md", "ask {{tier.big.name}} ({{tier.big}})\n"),
                ("settings/opencode/s.json", r#"{"model":"{{tier.big}}"}"#),
                (
                    "settings/claude/e.json",
                    r#"{"env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"mine"},"x":1}"#,
                ),
                ("settings/claude/x.json", r#"{"x":1}"#),
            ],
        )
    }

    fn rules_error(t: &TempDir, rule: &str, profile: &str) -> String {
        let r = root(t, "e", &[("tiers.yaml", TIERS), ("rules/a.md", rule)]);
        try_render(t, &[r], profile, &[], &[]).unwrap_err().0
    }

    #[test]
    fn tier_placeholders_follow_the_profiles_pool_and_harness() {
        let t = TempDir::new("tier");
        let r = tier_root(&t);
        let p = "harness: claude\nharnesses: [claude, opencode]\nrules: [a]\nsettings:\n  opencode: [s]\n";
        let out = do_render(&t, std::slice::from_ref(&r), &format!("{p}pool: claude\n"));
        assert_eq!(
            text(&out, Harness::Claude, "CLAUDE.md"),
            "ask Opus (opus-1)\n"
        );
        let out = do_render(&t, &[r], &format!("{p}pool: other\n"));
        assert_eq!(text(&out, Harness::Claude, "CLAUDE.md"), "ask GLM (glm)\n");
        assert_eq!(
            text(&out, Harness::Opencode, "AGENTS.md"),
            "ask GLM (prov/glm)\n"
        );
        assert_eq!(
            text(&out, Harness::Opencode, "opencode.json"),
            "{\n  \"model\": \"prov/glm\"\n}\n"
        );
    }

    #[test]
    fn tier_errors_have_the_place() {
        let t = TempDir::new("tiererr");
        let e = rules_error(
            &t,
            "a\n{{tier.huge}}\n",
            "harness: claude\nrules: [a]\npool: other\n",
        );
        let want = "{}:2: unknown tier `huge` in pool `other` (it has big, medium)";
        assert_eq!(
            e,
            want.replace("{}", &t.0.join("e/rules/a.md").display().to_string())
        );
        let e = rules_error(&t, "{{tier.big}}\n", "harness: claude\nrules: [a]\n");
        assert!(
            e.ends_with(":1: `{{tier.big}}` needs a pool, and the profile names none"),
            "{e}"
        );
        for bad in ["{{tier.}}", "{{tier.big.foo}}"] {
            let e = rules_error(&t, bad, "harness: claude\nrules: [a]\npool: other\n");
            assert!(e.ends_with("unknown placeholder"), "{e}");
        }
        let r = root(
            &t,
            "s",
            &[
                ("tiers.yaml", TIERS),
                ("settings/pi/s.json", r#"{"a":["{{tier.nope}}"]}"#),
            ],
        );
        let e = try_render(
            &t,
            std::slice::from_ref(&r),
            "harness: pi\nsettings:\n  pi: [s]\npool: other\n",
            &[],
            &[],
        )
        .unwrap_err();
        let file = r.join("settings/pi/s.json").display().to_string();
        assert_eq!(
            e.0,
            format!("{file}: /a/0: unknown tier `nope` in pool `other` (it has big, medium)")
        );
    }

    #[test]
    fn the_profiles_pool_must_be_in_the_manifest() {
        let t = TempDir::new("nopool");
        let r = root(&t, "r", &[("tiers.yaml", TIERS)]);
        let e = try_render(&t, &[r], "harness: claude\npool: x\n", &[], &[]).unwrap_err();
        assert_eq!(
            e.0,
            "profile `test`: pool `x` is not in the tiers manifest (pools: claude, other)"
        );
        let r = root(&t, "bare", &[("rules/a.md", "a")]);
        let e = try_render(&t, &[r], "harness: claude\npool: x\n", &[], &[]).unwrap_err();
        assert_eq!(
            e.0,
            "profile `test`: pool `x` is not in the tiers manifest (no tiers.yaml in the roots)"
        );
    }

    #[test]
    fn claude_gets_the_pools_models_as_env_unless_the_pool_is_claude() {
        let t = TempDir::new("tierenv");
        let r = tier_root(&t);
        let p = |pool: &str, blocks: &str| {
            format!("harness: claude\npool: {pool}\nsettings:\n  claude: [{blocks}]\n")
        };
        let out = do_render(&t, std::slice::from_ref(&r), &p("claude", "x"));
        assert_eq!(
            text(&out, Harness::Claude, "settings.json"),
            "{\n  \"x\": 1\n}\n"
        );
        // Only the tiers the pool defines, fallbacks resolved; blocks win.
        let out = do_render(&t, std::slice::from_ref(&r), &p("other", "e, x"));
        let v: Value = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        assert_eq!(
            v["env"],
            json!({"ANTHROPIC_DEFAULT_OPUS_MODEL": "mine", "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm"})
        );
        let srcs: Vec<_> = file(&out, Harness::Claude, "settings.json")
            .sources
            .iter()
            .map(|s| (s.block.as_str(), s.root.as_str()))
            .collect();
        assert_eq!(srcs, [("tiers.yaml", "r"), ("e", "r"), ("x", "r")]);
        // No settings blocks: the file holds the env alone.
        let out = do_render(&t, &[r], "harness: claude\npool: other\n");
        let v: Value = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        assert_eq!(
            v,
            json!({"env": {"ANTHROPIC_DEFAULT_OPUS_MODEL": "glm", "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm"}})
        );
        let s = &file(&out, Harness::Claude, "settings.json").sources;
        assert_eq!(
            (s.len(), s[0].block.as_str(), s[0].lines),
            (1, "tiers.yaml", None)
        );
    }

    // ------------------------------------------------- the auto mode seed

    /// Blocks that set the keys the auto mode seed meets, and one for pi and
    /// one for opencode.
    fn auto_root(t: &TempDir) -> PathBuf {
        root(
            t,
            "r",
            &[
                (
                    "settings/claude/own-list.json",
                    r#"{"autoMode":{"environment":["Org: Acme","$defaults"]}}"#,
                ),
                (
                    "settings/claude/list-no-defaults.json",
                    r#"{"autoMode":{"environment":["only mine"]}}"#,
                ),
                (
                    "settings/claude/env-null.json",
                    r#"{"autoMode":{"environment":null}}"#,
                ),
                ("settings/claude/auto-null.json", r#"{"autoMode":null}"#),
                (
                    "settings/claude/env-text.json",
                    r#"{"autoMode":{"environment":"just text"}}"#,
                ),
                (
                    "settings/claude/other-lists.json",
                    r#"{"autoMode":{"allow":["a"],"soft_deny":["d"]},"x":1}"#,
                ),
                (
                    "settings/claude/prompt-false.json",
                    r#"{"skipAutoPermissionPrompt":false}"#,
                ),
                (
                    "settings/claude/prompt-null.json",
                    r#"{"skipAutoPermissionPrompt":null}"#,
                ),
                ("settings/pi/p.json", r#"{"a":1}"#),
                ("settings/opencode/o.json", r#"{"b":2}"#),
            ],
        )
    }

    /// The profile of `claude_container` with these settings blocks.
    fn claude_profile(blocks: &[&str]) -> String {
        format!(
            "harness: claude\nsettings:\n  claude: [{}]\n",
            blocks.join(", ")
        )
    }

    /// Renders the profile as the container, and gives the parsed
    /// `settings.json` of Claude Code with the entry bree wrote for it.
    fn claude_container(t: &TempDir, r: &Path, profile: &str) -> (Value, String) {
        let out = try_render_as(t, &[r.to_path_buf()], profile, &[], &[], true).unwrap();
        let p = crate::profile::load(&t.0.join("profile.yaml"), "test", &env()).unwrap();
        let v = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        (v, automode::entry(&p))
    }

    #[test]
    fn the_container_seeds_claude_with_the_opt_in_and_the_entry() {
        let t = TempDir::new("auto-seed");
        let r = auto_root(&t);
        let one = std::slice::from_ref(&r);
        let out = try_render_as(&t, one, "harness: claude\n", &[], &[], true).unwrap();
        // No settings block: the file holds the seed alone.
        assert_eq!(paths(&out, Harness::Claude), ["settings.json"]);
        let p = crate::profile::load(&t.0.join("profile.yaml"), "test", &env()).unwrap();
        let entry = automode::entry(&p);
        assert!(entry.contains("for the bree profile test."), "{entry}");
        let want = format!(
            "{{\n  \"skipAutoPermissionPrompt\": true,\n  \"autoMode\": {{\n    \"environment\": [\n      \"$defaults\",\n      {}\n    ]\n  }}\n}}\n",
            Value::String(entry)
        );
        assert_eq!(text(&out, Harness::Claude, "settings.json"), want);
        // Not a block: the source is named for the seed, in the root that holds
        // the profile (this one is outside every root, so the first).
        let f = file(&out, Harness::Claude, "settings.json");
        assert_eq!(
            f.sources,
            [Source {
                block: "auto mode (bree)".to_string(),
                root: "r".to_string(),
                lines: None
            }]
        );
        let m = manifest(&out, &out.harnesses[0]);
        assert_eq!(
            m["files"][0]["sources"],
            json!([{"block": "auto mode (bree)", "root": "r"}])
        );
        // The same profile gives the same bytes every time.
        let again = try_render_as(&t, &[r], "harness: claude\n", &[], &[], true).unwrap();
        assert_eq!(
            file(&again, Harness::Claude, "settings.json").bytes,
            f.bytes
        );
    }

    #[test]
    fn a_block_with_its_own_list_keeps_it_and_gets_the_entry_after_it() {
        let t = TempDir::new("auto-list");
        let r = auto_root(&t);
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["own-list"]));
        assert_eq!(
            v["autoMode"]["environment"],
            json!(["Org: Acme", "$defaults", entry])
        );
        assert_eq!(v["skipAutoPermissionPrompt"], true);
        // Without `$defaults` in it, the list still gets the entry.
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["list-no-defaults"]));
        assert_eq!(v["autoMode"]["environment"], json!(["only mine", entry]));
        // A block that already holds the entry does not get it twice.
        let (_, entry) = claude_container(&t, &r, &claude_profile(&[]));
        let mine = json!({"autoMode": {"environment": ["mine", entry]}}).to_string();
        let r2 = root(&t, "r2", &[("settings/claude/has-it.json", mine.as_str())]);
        let (v, entry) = claude_container(&t, &r2, &claude_profile(&["has-it"]));
        assert_eq!(v["autoMode"]["environment"], json!(["mine", entry]));
        // The blocks come after the seed in the sources.
        let out = try_render_as(&t, &[r], &claude_profile(&["own-list"]), &[], &[], true).unwrap();
        let blocks: Vec<_> = file(&out, Harness::Claude, "settings.json")
            .sources
            .iter()
            .map(|s| s.block.as_str())
            .collect();
        assert_eq!(blocks, ["auto mode (bree)", "own-list"]);
    }

    #[test]
    fn a_block_that_removes_or_replaces_the_list_opts_out() {
        let t = TempDir::new("auto-optout");
        let r = auto_root(&t);
        // The key removed: no entry, and no list either.
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["env-null"]));
        assert_eq!(v["autoMode"], json!({}));
        assert_eq!(v["skipAutoPermissionPrompt"], true);
        assert!(!v.to_string().contains(&entry), "{v}");
        // The whole object removed.
        let (v, _) = claude_container(&t, &r, &claude_profile(&["auto-null"]));
        assert!(v.get("autoMode").is_none(), "{v}");
        // Something else than an array stays as it is.
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["env-text"]));
        assert_eq!(v["autoMode"]["environment"], "just text");
        assert!(!v.to_string().contains(&entry), "{v}");
    }

    #[test]
    fn the_other_auto_mode_lists_are_never_touched() {
        let t = TempDir::new("auto-lists");
        let r = auto_root(&t);
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["other-lists"]));
        assert_eq!(
            v["autoMode"],
            json!({
                "environment": ["$defaults", entry],
                "allow": ["a"],
                "soft_deny": ["d"],
            })
        );
        // No list of its own besides the seed's, in the seed's place.
        let (v, _) = claude_container(&t, &r, &claude_profile(&[]));
        assert_eq!(v["autoMode"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn a_block_can_turn_the_opt_in_off_or_remove_it() {
        let t = TempDir::new("auto-prompt");
        let r = auto_root(&t);
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["prompt-false"]));
        assert_eq!(v["skipAutoPermissionPrompt"], false);
        assert_eq!(v["autoMode"]["environment"], json!(["$defaults", entry]));
        let (v, entry) = claude_container(&t, &r, &claude_profile(&["prompt-null"]));
        assert!(v.get("skipAutoPermissionPrompt").is_none(), "{v}");
        assert_eq!(v["autoMode"]["environment"], json!(["$defaults", entry]));
    }

    #[test]
    fn the_host_target_gets_no_seed() {
        let t = TempDir::new("auto-host");
        let r = auto_root(&t);
        // Nothing to write without a block.
        let one = std::slice::from_ref(&r);
        let out = try_render_as(&t, one, "harness: claude\n", &[], &[], false).unwrap();
        assert!(paths(&out, Harness::Claude).is_empty());
        // With a block, the file holds the block's keys and no others.
        let out =
            try_render_as(&t, &[r], &claude_profile(&["other-lists"]), &[], &[], false).unwrap();
        let v: Value = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        assert_eq!(
            v,
            json!({"autoMode": {"allow": ["a"], "soft_deny": ["d"]}, "x": 1})
        );
        let f = file(&out, Harness::Claude, "settings.json");
        let blocks: Vec<_> = f.sources.iter().map(|s| s.block.as_str()).collect();
        assert_eq!(blocks, ["other-lists"]);
    }

    #[test]
    fn pi_and_opencode_are_untouched_on_the_container_target() {
        let t = TempDir::new("auto-others");
        let r = auto_root(&t);
        let all = "harness: claude\nharnesses: [claude, pi, opencode]\n";
        // Without blocks they write no settings file at all.
        let out = try_render_as(&t, std::slice::from_ref(&r), all, &[], &[], true).unwrap();
        assert!(paths(&out, Harness::Pi).is_empty());
        assert!(paths(&out, Harness::Opencode).is_empty());
        // With blocks the file is the block's alone.
        let with = format!("{all}settings:\n  pi: [p]\n  opencode: [o]\n");
        let out = try_render_as(&t, &[r], &with, &[], &[], true).unwrap();
        assert_eq!(
            text(&out, Harness::Pi, "settings.json"),
            "{\n  \"a\": 1\n}\n"
        );
        assert_eq!(
            text(&out, Harness::Opencode, "opencode.json"),
            "{\n  \"b\": 2\n}\n"
        );
        assert_eq!(
            file(&out, Harness::Pi, "settings.json").sources.len(),
            1,
            "only the block"
        );
        // Claude has its seed in the same render.
        assert!(text(&out, Harness::Claude, "settings.json").contains("autoMode"));
    }

    #[test]
    fn the_seed_comes_before_the_tier_env_vars_in_the_file_and_the_sources() {
        let t = TempDir::new("auto-tiers");
        let r = tier_root(&t);
        let profile = "harness: claude\npool: other\nsettings:\n  claude: [e, x]\n";
        let out = try_render_as(&t, std::slice::from_ref(&r), profile, &[], &[], true).unwrap();
        let v: Value = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["skipAutoPermissionPrompt", "autoMode", "env", "x"]);
        assert_eq!(
            v["env"],
            json!({"ANTHROPIC_DEFAULT_OPUS_MODEL": "mine", "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm"})
        );
        let srcs: Vec<_> = file(&out, Harness::Claude, "settings.json")
            .sources
            .iter()
            .map(|s| (s.block.as_str(), s.root.as_str()))
            .collect();
        assert_eq!(
            srcs,
            [
                ("auto mode (bree)", "r"),
                ("tiers.yaml", "r"),
                ("e", "r"),
                ("x", "r")
            ]
        );
        // No block: both seeds alone, the same two sources.
        let out =
            try_render_as(&t, &[r], "harness: claude\npool: other\n", &[], &[], true).unwrap();
        let v: Value = serde_json::from_str(&text(&out, Harness::Claude, "settings.json")).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["skipAutoPermissionPrompt", "autoMode", "env"]);
        let s = &file(&out, Harness::Claude, "settings.json").sources;
        assert_eq!(
            s.iter().map(|s| s.block.as_str()).collect::<Vec<_>>(),
            ["auto mode (bree)", "tiers.yaml"]
        );
    }

    #[test]
    fn the_seeds_root_is_the_one_that_holds_the_profile() {
        let t = TempDir::new("auto-root");
        let a = root(&t, "a", &[("rules/x.md", "x\n")]);
        let b = root(&t, "b", &[("profiles/p.yaml", "harness: claude\n")]);
        let file_b = b.join("profiles/p.yaml");
        let p = crate::profile::load(&file_b, "p", &env()).unwrap();
        let opts = Options {
            harnesses: &[],
            adds: &[],
            vars: &[],
            container: true,
        };
        let out = render(&p, &file_b, &[a.clone(), b.clone()], &opts).unwrap();
        let f = file(&out, Harness::Claude, "settings.json");
        assert_eq!(
            (f.sources[0].block.as_str(), f.sources[0].root.as_str()),
            ("auto mode (bree)", "b")
        );

        let roots = |paths: &[&Path]| -> Vec<Root> {
            paths
                .iter()
                .map(|p| Root {
                    name: p.file_name().unwrap().to_string_lossy().into(),
                    path: p.to_path_buf(),
                })
                .collect()
        };
        // The deepest root wins, and a profile outside them counts for the first.
        let nested = roots(&[Path::new("/r"), Path::new("/r/inner")]);
        assert_eq!(
            profile_root(&nested, Path::new("/r/inner/profiles/p.yaml")),
            "inner"
        );
        assert_eq!(profile_root(&nested, Path::new("/r/profiles/p.yaml")), "r");
        assert_eq!(profile_root(&nested, Path::new("/elsewhere/p.yaml")), "r");
        assert_eq!(profile_root(&[], Path::new("/p.yaml")), "");
    }
}
