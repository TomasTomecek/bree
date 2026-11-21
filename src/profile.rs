//! Profiles (docs/design.md, "Profiles" and "Secrets"): the types, loading and
//! validation. A profile file is read through the strict YAML loader into raw
//! types, then validated and filled with defaults into [`Profile`].

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::config::{Env, expand_tilde, normalize};
use crate::error::{Error, Result};
use crate::yaml::{self, Ordered, Spanned};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Pi,
    Opencode,
}

impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    pub const ALL: [Harness; 3] = [Harness::Claude, Harness::Pi, Harness::Opencode];

    pub fn from_name(name: &str) -> Option<Harness> {
        Harness::ALL.into_iter().find(|h| h.name() == name)
    }
}

/// `all`, or an explicit list of block names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocks {
    All,
    List(Vec<String>),
}

impl Default for Blocks {
    fn default() -> Self {
        Blocks::List(Vec::new())
    }
}

impl<'de> Deserialize<'de> for Blocks {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Vis;
        impl<'de> Visitor<'de> for Vis {
            type Value = Blocks;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`all` or a list of block names")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Blocks, E> {
                if v == "all" {
                    Ok(Blocks::All)
                } else {
                    Err(E::custom(format!(
                        "expected `all` or a list of block names, found `{v}`"
                    )))
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Blocks, A::Error> {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    out.push(s);
                }
                Ok(Blocks::List(out))
            }
        }
        d.deserialize_any(Vis)
    }
}

/// One value per harness, for the keys that are maps from harness name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerHarness<T: Default> {
    #[serde(default)]
    pub claude: T,
    #[serde(default)]
    pub pi: T,
    #[serde(default)]
    pub opencode: T,
}

impl<T: Default> PerHarness<T> {
    pub fn get(&self, h: Harness) -> &T {
        match h {
            Harness::Claude => &self.claude,
            Harness::Pi => &self.pi,
            Harness::Opencode => &self.opencode,
        }
    }

    pub fn get_mut(&mut self, h: Harness) -> &mut T {
        match h {
            Harness::Claude => &mut self.claude,
            Harness::Pi => &mut self.pi,
            Harness::Opencode => &mut self.opencode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Build,
    Pull,
    /// A local image used as it is: never built or pulled (crew profiles).
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct Image {
    #[serde(rename = "ref")]
    pub reference: String,
    pub source: Source,
}

impl Default for Image {
    fn default() -> Self {
        Image {
            reference: concat!("localhost/bree-base:", env!("CARGO_PKG_VERSION")).to_string(),
            source: Source::Build,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Pasta,
    Host,
    None,
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Pasta => "pasta",
            Network::Host => "host",
            Network::None => "none",
        }
    }
}

/// `container.workdir`: `cwd`, `mount`, or a fixed absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Workdir {
    #[default]
    Cwd,
    Mount,
    /// A fixed `--workdir`; the current directory is neither used nor mounted.
    Path(PathBuf),
}

impl<'de> Deserialize<'de> for Workdir {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Vis;
        impl Visitor<'_> for Vis {
            type Value = Workdir;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`cwd`, `mount` or an absolute path")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Workdir, E> {
                match v {
                    "cwd" => Ok(Workdir::Cwd),
                    "mount" => Ok(Workdir::Mount),
                    p if Path::new(p).is_absolute() => Ok(Workdir::Path(normalize(Path::new(p)))),
                    _ => Err(E::custom(format!(
                        "expected `cwd`, `mount` or an absolute path, found `{v}`"
                    ))),
                }
            }
        }
        d.deserialize_any(Vis)
    }
}

/// `container.kvm`: `true`, `false`, or `optional` (only when the host has
/// `/dev/kvm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kvm {
    #[default]
    Off,
    On,
    Optional,
}

impl<'de> Deserialize<'de> for Kvm {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Vis;
        impl Visitor<'_> for Vis {
            type Value = Kvm;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("true, false or optional")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Kvm, E> {
                Ok(if v { Kvm::On } else { Kvm::Off })
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Kvm, E> {
                match v {
                    "optional" => Ok(Kvm::Optional),
                    _ => Err(E::custom(format!(
                        "expected true, false or optional, found `{v}`"
                    ))),
                }
            }
        }
        d.deserialize_any(Vis)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct Container {
    pub network: Network,
    pub nested_podman: bool,
    /// `label=disable` without the nested storage volume.
    pub label_disable: bool,
    pub kvm: Kvm,
    pub workdir: Workdir,
    pub tz: String,
    pub env: Ordered<String>,
    pub pass_env: Vec<String>,
    /// Fixed `--name` instead of `bree-<profile>-<dir>-<hex>`.
    pub name: Option<String>,
    /// Nested storage volume instead of `bree-<profile>-containers`.
    pub storage_volume: Option<String>,
}

impl Default for Container {
    fn default() -> Self {
        Container {
            network: Network::Pasta,
            nested_podman: true,
            label_disable: false,
            kvm: Kvm::Off,
            workdir: Workdir::Cwd,
            tz: "local".to_string(),
            env: Ordered(Vec::new()),
            pass_env: vec!["TERM".to_string()],
            name: None,
            storage_volume: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawState {
    dir: Option<String>,
    home: Option<StateHome>,
}

/// `state.home`: the only value is `image`, keep the image's home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StateHome {
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Rw,
    Ro,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawMount {
    host: Option<String>,
    volume: Option<String>,
    container: Option<String>,
    #[serde(default)]
    mode: Mode,
    optional: Option<bool>,
    relabel: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SecretSource {
    Env,
    File,
    Broker,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawSecret {
    source: Option<SecretSource>,
    var: Option<String>,
    path: Option<String>,
    url: Option<String>,
    token: Option<String>,
    route: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    harness_env: PerHarness<Vec<String>>,
    mount: Option<String>,
    #[serde(default = "yes")]
    required: bool,
    // Only here to give a specific error; see validation.
    value: Option<IgnoredAny>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawProfile {
    #[serde(default)]
    description: String,
    harness: Option<Harness>,
    command: Option<Vec<String>>,
    harnesses: Option<Vec<Harness>>,
    #[serde(default)]
    harness_command: PerHarness<Vec<String>>,
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    skills: Blocks,
    #[serde(default)]
    commands: Blocks,
    #[serde(default)]
    settings: PerHarness<Vec<String>>,
    #[serde(default)]
    files: PerHarness<Vec<String>>,
    #[serde(default)]
    vars: Ordered<String>,
    #[serde(default)]
    optional: Vec<String>,
    pool: Option<String>,
    #[serde(default)]
    image: Image,
    #[serde(default)]
    container: Container,
    #[serde(default)]
    state: RawState,
    #[serde(default)]
    mounts: Vec<Spanned<RawMount>>,
    #[serde(default)]
    secrets: Ordered<Spanned<RawSecret>>,
}

/// What a profile mount mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A host path, expanded and normalized.
    Host(PathBuf),
    /// A named podman volume.
    Volume(String),
}

/// A mount of the profile, paths expanded and normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: MountSource,
    pub container: PathBuf,
    pub read_only: bool,
    pub optional: bool,
    /// `:z` on a host path; always `false` for a volume.
    pub relabel: bool,
    /// The host path was written with `~`, which `bree export quadlet` turns
    /// into `%h`; always `false` for a volume.
    pub tilde: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSrc {
    Env {
        var: String,
    },
    File {
        path: PathBuf,
    },
    /// The credential broker: its base URL (no trailing `/`) and the host file
    /// with the operator token. The only route is `anthropic`.
    Broker {
        url: String,
        token: PathBuf,
    },
}

/// Variables a profile with a broker secret never sets itself: `bree run`
/// sets the first three, and the last two would give the harness a credential
/// that bypasses the broker.
pub const BROKER_RESERVED_ENV: [&str; 5] = [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    pub source: SecretSrc,
    pub env: Vec<String>,
    pub harness_env: PerHarness<Vec<String>>,
    pub mount: Option<PathBuf>,
    pub required: bool,
    /// A file source's path (or a broker's token file) was written with `~`
    /// (see [`Mount::tilde`]).
    pub tilde: bool,
}

/// A validated profile with the defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub description: String,
    /// `None` exactly when `command` is set.
    pub harness: Option<Harness>,
    /// The argv a crew profile runs instead of `bree entrypoint -- <harness>`.
    pub command: Option<Vec<String>>,
    pub harnesses: Vec<Harness>,
    pub harness_command: PerHarness<Vec<String>>,
    pub rules: Vec<String>,
    pub skills: Blocks,
    pub commands: Blocks,
    pub settings: PerHarness<Vec<String>>,
    pub files: PerHarness<Vec<String>>,
    pub vars: Vec<(String, String)>,
    pub optional: Vec<String>,
    pub pool: Option<String>,
    pub image: Image,
    pub container: Container,
    /// The state directory mounted as the home; `None` for `state.home:
    /// image`, which keeps the image's home.
    pub state_dir: Option<PathBuf>,
    pub mounts: Vec<Mount>,
    pub secrets: Vec<Secret>,
}

/// A podman container or volume name: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
pub fn valid_podman_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reads and validates the profile `name` from `path`.
pub fn load(path: &Path, name: &str, env: &Env) -> Result<Profile> {
    let file = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::new(format!("cannot read profile {file}: {e}")))?;
    let raw: RawProfile = yaml::from_str(&text, &file)?;
    validate(raw, name, &file, env)
}

fn bad(file: &str, line: Option<usize>, path: &str, message: impl Into<String>) -> Error {
    yaml::Error::new(file, line, path, message).into()
}

fn check_blocks(file: &str, list: &str, names: &[String], prefixes: bool) -> Result<()> {
    for entry in names {
        let name = match entry.split_once(':') {
            Some((h, n)) if prefixes => {
                if !matches!(h, "claude" | "pi" | "opencode") {
                    return Err(bad(
                        file,
                        None,
                        list,
                        format!("`{entry}`: the prefix before `:` must be claude, pi or opencode"),
                    ));
                }
                n
            }
            _ => entry.as_str(),
        };
        if name.is_empty() || name.contains('/') {
            return Err(bad(
                file,
                None,
                list,
                format!("`{entry}` is not a block name (non-empty, no `/`)"),
            ));
        }
    }
    Ok(())
}

fn check_harness_command(file: &str, h: Harness, argv: &[String]) -> Result<()> {
    let path = format!("harness-command.{}", h.name());
    for (i, arg) in argv.iter().enumerate() {
        if arg.is_empty() {
            return Err(bad(
                file,
                None,
                &path,
                format!("argument {} is empty", i + 1),
            ));
        }
        if arg.contains('\0') {
            return Err(bad(
                file,
                None,
                &path,
                format!("argument {} contains a NUL byte", i + 1),
            ));
        }
    }
    Ok(())
}

fn check_env_names<'a>(
    file: &str,
    line: Option<usize>,
    path: &str,
    names: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    for n in names {
        if !valid_env_name(n) {
            return Err(bad(
                file,
                line,
                path,
                format!("`{n}` is not a valid environment variable name"),
            ));
        }
    }
    Ok(())
}

/// `~` expanded; must then be absolute. Normalized.
fn absolute(
    text: &str,
    home: &Path,
    file: &str,
    line: Option<usize>,
    path: &str,
) -> Result<PathBuf> {
    let p = expand_tilde(text, home);
    if !p.is_absolute() {
        return Err(bad(
            file,
            line,
            path,
            format!("`{text}` must be an absolute path or start with `~`"),
        ));
    }
    Ok(normalize(&p))
}

fn pinned(reference: &str) -> bool {
    if reference.contains("@sha256:") {
        return true;
    }
    let after_slash = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[after_slash..].rfind(':') {
        Some(i) => &reference[after_slash + i + 1..] != "latest",
        None => false,
    }
}

fn validate(raw: RawProfile, name: &str, file: &str, env: &Env) -> Result<Profile> {
    let home = &env.home;
    check_blocks(file, "rules", &raw.rules, true)?;
    for (list, blocks) in [("skills", &raw.skills), ("commands", &raw.commands)] {
        if let Blocks::List(names) = blocks {
            check_blocks(file, list, names, true)?;
        }
    }
    for h in [Harness::Claude, Harness::Pi, Harness::Opencode] {
        check_harness_command(file, h, raw.harness_command.get(h))?;
        check_blocks(
            file,
            &format!("settings.{}", h.name()),
            raw.settings.get(h),
            false,
        )?;
        check_blocks(
            file,
            &format!("files.{}", h.name()),
            raw.files.get(h),
            false,
        )?;
    }
    check_blocks(file, "optional", &raw.optional, false)?;

    if raw.image.source == Source::Pull && !pinned(&raw.image.reference) {
        return Err(bad(
            file,
            None,
            "image",
            "image.ref must be pinned by digest or a version tag when image.source is pull",
        ));
    }

    check_env_names(
        file,
        None,
        "container.env",
        raw.container.env.0.iter().map(|(k, _)| k),
    )?;
    check_env_names(file, None, "container.pass-env", &raw.container.pass_env)?;

    match (&raw.command, raw.harness) {
        (None, None) => {
            return Err(bad(
                file,
                None,
                "harness",
                "required, unless command is set",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(bad(
                file,
                None,
                "command",
                "command and harness exclude each other: a profile with command starts no harness",
            ));
        }
        (Some(_), None) if raw.harnesses.is_some() => {
            return Err(bad(
                file,
                None,
                "command",
                "command and harnesses exclude each other: a profile with command renders nothing",
            ));
        }
        (Some(_), None) if raw.harness_command != PerHarness::default() => {
            return Err(bad(
                file,
                None,
                "command",
                "command and harness-command exclude each other: a profile with command starts no harness",
            ));
        }
        (Some(argv), None) if argv.is_empty() => {
            return Err(bad(file, None, "command", "command must not be empty"));
        }
        _ => {}
    }

    if let Some(n) = &raw.container.name {
        if !valid_podman_name(n) {
            return Err(bad(
                file,
                None,
                "container.name",
                format!("`{n}` is not a valid container name ([a-zA-Z0-9][a-zA-Z0-9_.-]*)"),
            ));
        }
    }
    if let Some(n) = &raw.container.storage_volume {
        if !valid_podman_name(n) {
            return Err(bad(
                file,
                None,
                "container.storage-volume",
                format!("`{n}` is not a valid volume name ([a-zA-Z0-9][a-zA-Z0-9_.-]*)"),
            ));
        }
        if !raw.container.nested_podman {
            return Err(bad(
                file,
                None,
                "container.storage-volume",
                "needs container.nested-podman: true",
            ));
        }
    }

    let state_dir = match (raw.state.home, &raw.state.dir) {
        (Some(StateHome::Image), Some(_)) => {
            return Err(bad(
                file,
                None,
                "state",
                "state.dir and state.home: image exclude each other",
            ));
        }
        (Some(StateHome::Image), None) => {
            let renders = raw.harness.is_some()
                || raw.harnesses.is_some()
                || !raw.rules.is_empty()
                || raw.skills != Blocks::default()
                || raw.commands != Blocks::default()
                || Harness::ALL
                    .iter()
                    .any(|&h| !raw.settings.get(h).is_empty() || !raw.files.get(h).is_empty());
            if renders {
                return Err(bad(
                    file,
                    None,
                    "state.home",
                    "state.home: image keeps the image's home, so the profile renders nothing: \
                     set command instead of harness, and drop harnesses, rules, skills, commands, settings and files",
                ));
            }
            None
        }
        (None, Some(d)) => Some(absolute(d, home, file, None, "state.dir")?),
        (None, None) => Some(home.join(".local/share/bree/state").join(name)),
    };

    let mut mounts = Vec::new();
    for (i, m) in raw.mounts.iter().enumerate() {
        let path = format!("mounts[{i}]");
        let line = Some(m.line);
        mounts.push(validate_mount(&m.value, &path, file, line, env)?);
    }

    let mut secrets = Vec::new();
    for (sname, spanned) in &raw.secrets.0 {
        secrets.push(validate_secret(sname, spanned, file, home)?);
    }
    check_broker(&raw, &secrets, file)?;

    let harnesses = raw
        .harnesses
        .unwrap_or_else(|| raw.harness.into_iter().collect());
    Ok(Profile {
        name: name.to_string(),
        description: raw.description,
        harness: raw.harness,
        command: raw.command,
        harnesses,
        harness_command: raw.harness_command,
        rules: raw.rules,
        skills: raw.skills,
        commands: raw.commands,
        settings: raw.settings,
        files: raw.files,
        vars: raw.vars.0,
        optional: raw.optional,
        pool: raw.pool,
        image: raw.image,
        container: raw.container,
        state_dir,
        mounts,
        secrets,
    })
}

/// One `mounts` entry: a host path or a named volume (docs/design.md,
/// "Profiles" and "Crew profiles and quadlets").
fn validate_mount(
    m: &RawMount,
    path: &str,
    file: &str,
    line: Option<usize>,
    env: &Env,
) -> Result<Mount> {
    let home = &env.home;
    let container = match &m.container {
        Some(c) => Some(absolute(c, home, file, line, &format!("{path}.container"))?),
        None => None,
    };
    let (source, container, relabel, tilde) = match (&m.host, &m.volume) {
        (Some(_), Some(_)) => {
            return Err(bad(file, line, path, "host and volume exclude each other"));
        }
        (None, None) => {
            return Err(bad(file, line, path, "needs host or volume"));
        }
        (Some(h), None) => {
            let host = host_path(h, env, file, line, &format!("{path}.host"))?;
            let container = container.unwrap_or_else(|| host.clone());
            (
                MountSource::Host(host),
                container,
                m.relabel.unwrap_or(true),
                is_tilde(h),
            )
        }
        (None, Some(v)) => {
            if !valid_podman_name(v) {
                return Err(bad(
                    file,
                    line,
                    &format!("{path}.volume"),
                    format!("`{v}` is not a valid volume name ([a-zA-Z0-9][a-zA-Z0-9_.-]*)"),
                ));
            }
            let Some(container) = container else {
                return Err(bad(file, line, path, "a volume needs container"));
            };
            for (key, set) in [
                ("optional", m.optional.is_some()),
                ("relabel", m.relabel.is_some()),
            ] {
                if set {
                    return Err(bad(
                        file,
                        line,
                        &format!("{path}.{key}"),
                        "only for a host path, not a volume",
                    ));
                }
            }
            (MountSource::Volume(v.clone()), container, false, false)
        }
    };
    Ok(Mount {
        source,
        container,
        read_only: m.mode == Mode::Ro,
        optional: m.optional.unwrap_or(false),
        relabel,
        tilde,
    })
}

/// Whether a path is written as `~` or `~/...`, the forms [`expand_tilde`]
/// expands.
fn is_tilde(text: &str) -> bool {
    text == "~" || text.starts_with("~/")
}

/// A mount's host path: `~` is the home, a relative path resolves against
/// `--relative-to` and is an error without it.
fn host_path(
    text: &str,
    env: &Env,
    file: &str,
    line: Option<usize>,
    path: &str,
) -> Result<PathBuf> {
    let p = expand_tilde(text, &env.home);
    if p.is_absolute() {
        return Ok(normalize(&p));
    }
    match &env.relative_to {
        Some(dir) => Ok(normalize(&dir.join(p))),
        None => Err(bad(
            file,
            line,
            path,
            format!(
                "`{text}` is relative: make it absolute, start it with `~` or pass --relative-to DIR"
            ),
        )),
    }
}

fn validate_secret(
    name: &str,
    spanned: &Spanned<RawSecret>,
    file: &str,
    home: &Path,
) -> Result<Secret> {
    let s = &spanned.value;
    let line = Some(spanned.line);
    let path = format!("secrets.{name}");
    let err = |m: &str| bad(file, line, &path, m);

    if s.value.is_some() {
        return Err(err(
            "a profile never holds a secret value; use source env or file",
        ));
    }
    let source = match s.source {
        None => return Err(err("source is required (env, file or broker)")),
        Some(x) => x,
    };
    if source != SecretSource::Broker && (s.url.is_some() || s.token.is_some() || s.route.is_some())
    {
        return Err(err("url, token and route are only for source broker"));
    }
    let src = match source {
        SecretSource::Env => {
            if s.path.is_some() || s.mount.is_some() {
                return Err(err("path and mount are only for source file"));
            }
            let Some(var) = &s.var else {
                return Err(err("source env needs var"));
            };
            check_env_names(file, line, &path, [var])?;
            SecretSrc::Env { var: var.clone() }
        }
        SecretSource::File => {
            if s.var.is_some() {
                return Err(err("var is only for source env"));
            }
            let Some(p) = &s.path else {
                return Err(err("source file needs path"));
            };
            SecretSrc::File {
                path: absolute(p, home, file, line, &format!("{path}.path"))?,
            }
        }
        SecretSource::Broker => {
            if s.var.is_some() || s.path.is_some() || s.mount.is_some() {
                return Err(err("var, path and mount are not for source broker"));
            }
            if s.harness_env != PerHarness::default() {
                return Err(err(
                    "harness-env is not for source broker: it sets its variables itself, use env for extra ones",
                ));
            }
            let Some(url) = &s.url else {
                return Err(err("source broker needs url"));
            };
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || url
                    .split_once("://")
                    .is_some_and(|(_, rest)| rest.trim_end_matches('/').is_empty())
            {
                return Err(bad(
                    file,
                    line,
                    &format!("{path}.url"),
                    format!("`{url}` must start with http:// or https:// and name a host"),
                ));
            }
            let Some(token) = &s.token else {
                return Err(err("source broker needs token"));
            };
            match s.route.as_deref() {
                Some("anthropic") => {}
                Some(r) => {
                    return Err(bad(
                        file,
                        line,
                        &format!("{path}.route"),
                        format!("`{r}` is not a route; the only one is anthropic"),
                    ));
                }
                None => return Err(err("source broker needs route (anthropic)")),
            }
            SecretSrc::Broker {
                url: url.trim_end_matches('/').to_string(),
                token: absolute(token, home, file, line, &format!("{path}.token"))?,
            }
        }
    };
    check_env_names(file, line, &format!("{path}.env"), &s.env)?;
    for h in [Harness::Claude, Harness::Pi, Harness::Opencode] {
        check_env_names(
            file,
            line,
            &format!("{path}.harness-env.{}", h.name()),
            s.harness_env.get(h),
        )?;
    }
    let mount = match &s.mount {
        Some(m) => {
            let p = expand_tilde(m, home);
            if !p.is_absolute() {
                return Err(bad(
                    file,
                    line,
                    &format!("{path}.mount"),
                    format!("`{m}` must be an absolute path"),
                ));
            }
            Some(normalize(&p))
        }
        None => None,
    };
    let nowhere = source != SecretSource::Broker
        && s.env.is_empty()
        && [Harness::Claude, Harness::Pi, Harness::Opencode]
            .iter()
            .all(|h| s.harness_env.get(*h).is_empty())
        && mount.is_none();
    if nowhere {
        return Err(err("goes nowhere: set env, harness-env or mount"));
    }
    Ok(Secret {
        name: name.to_string(),
        source: src,
        env: s.env.clone(),
        harness_env: s.harness_env.clone(),
        mount,
        required: s.required,
        tilde: s
            .path
            .as_deref()
            .or(s.token.as_deref())
            .is_some_and(is_tilde),
    })
}

/// The rules a broker secret puts on the whole profile: one broker, the claude
/// harness only, and no variable that would hand the harness another
/// credential or override what `bree run` sets.
fn check_broker(raw: &RawProfile, secrets: &[Secret], file: &str) -> Result<()> {
    let brokers: Vec<(&Secret, usize)> = secrets
        .iter()
        .zip(&raw.secrets.0)
        .filter(|(s, _)| matches!(s.source, SecretSrc::Broker { .. }))
        .map(|(s, (_, spanned))| (s, spanned.line))
        .collect();
    let Some(&(first, line)) = brokers.first() else {
        return Ok(());
    };
    let path = format!("secrets.{}", first.name);
    let err = |m: String| bad(file, Some(line), &path, m);
    if brokers.len() > 1 {
        return Err(err("only one secret may have source broker".to_string()));
    }
    let harnesses = match (&raw.harnesses, raw.harness) {
        (Some(hs), _) => hs.clone(),
        (None, h) => h.into_iter().collect(),
    };
    if raw.command.is_some()
        || raw.harness != Some(Harness::Claude)
        || harnesses != [Harness::Claude]
    {
        return Err(err(
            "source broker works with the claude harness only: set harness: claude and no other harness in harnesses, and no command"
                .to_string(),
        ));
    }
    let reserved = |n: &str| BROKER_RESERVED_ENV.contains(&n);
    for s in secrets {
        let own = matches!(s.source, SecretSrc::Broker { .. });
        for n in s
            .env
            .iter()
            .chain(Harness::ALL.iter().flat_map(|&h| s.harness_env.get(h)))
        {
            if reserved(n) {
                return Err(bad(
                    file,
                    Some(line),
                    &format!("secrets.{}.env", s.name),
                    format!(
                        "`{n}` is not allowed in a profile with a broker secret: {}",
                        if own {
                            "bree sets the broker's variables itself"
                        } else {
                            "the harness must get no credential but the broker's"
                        }
                    ),
                ));
            }
        }
    }
    for n in raw
        .container
        .pass_env
        .iter()
        .chain(raw.container.env.0.iter().map(|(k, _)| k))
    {
        if reserved(n) {
            return Err(bad(
                file,
                Some(line),
                "container.env",
                format!(
                    "`{n}` is not allowed in a profile with a broker secret: the harness must get no credential but the broker's"
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::env;

    const WORK: &str = include_str!("../tests/fixtures/roots/shared/profiles/work.yaml");
    const HOME: &str = include_str!("../tests/fixtures/roots/shared/profiles/home.yaml");

    fn parse(text: &str) -> Result<Profile> {
        let raw: RawProfile = yaml::from_str(text, "p.yaml")?;
        validate(raw, "p", "p.yaml", &env())
    }

    fn fails(text: &str, needle: &str) {
        match parse(text) {
            Ok(_) => panic!("accepted:\n{text}"),
            Err(e) => assert!(e.0.contains(needle), "`{}` lacks `{needle}`", e.0),
        }
    }

    fn with(extra: &str) -> String {
        format!("harness: claude\n{extra}")
    }

    #[test]
    fn work_profile_parses_with_defaults() {
        let raw: RawProfile = yaml::from_str(WORK, "work.yaml").unwrap();
        let p = validate(raw, "work", "work.yaml", &env()).unwrap();
        assert_eq!(p.harness, Some(Harness::Claude));
        assert_eq!(
            p.harnesses,
            [Harness::Claude, Harness::Pi, Harness::Opencode]
        );
        assert_eq!(p.pool.as_deref(), Some("corp"));
        assert_eq!(p.skills, Blocks::All);
        assert_eq!(p.commands, Blocks::List(vec!["review-text".into()]));
        assert_eq!(p.rules.len(), 4);
        assert_eq!(p.optional, ["work-internal"]);
        assert_eq!(p.image.reference, "localhost/bree-base:0.1.0");
        assert_eq!(p.image.source, Source::Build);
        assert_eq!(p.container.network, Network::Pasta);
        assert!(p.container.nested_podman);
        assert_eq!(p.container.kvm, Kvm::Off);
        assert_eq!(p.container.workdir, Workdir::Cwd);
        assert_eq!(p.container.tz, "local");
        assert_eq!(p.container.pass_env, ["TERM"]);
        assert_eq!(
            p.state_dir,
            Some(PathBuf::from("/home/alice/.local/share/bree/state/work"))
        );
        assert_eq!(p.mounts.len(), 2);
        assert_eq!(
            p.mounts[0].source,
            MountSource::Host(PathBuf::from("/home/alice/src"))
        );
        assert_eq!(p.mounts[0].container, PathBuf::from("/home/alice/src"));
        assert!(!p.mounts[0].read_only && !p.mounts[0].optional);
        assert!(p.mounts[1].read_only && p.mounts[1].optional);
        assert_eq!(p.secrets.len(), 1);
        let s = &p.secrets[0];
        assert_eq!(s.name, "corp");
        assert_eq!(
            s.source,
            SecretSrc::Env {
                var: "CORP_KEY".into()
            }
        );
        assert!(!s.required);
        assert_eq!(s.env, ["CORP_KEY"]);
        assert_eq!(s.harness_env.pi, ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"]);
        assert_eq!(p.settings.claude, ["work", "work-internal"]);
        assert_eq!(p.files.pi, ["base", "work"]);
    }

    #[test]
    fn home_profile_parses_with_defaults() {
        let raw: RawProfile = yaml::from_str(HOME, "home.yaml").unwrap();
        let p = validate(raw, "home", "home.yaml", &env()).unwrap();
        assert_eq!(p.harnesses, [Harness::Claude, Harness::Pi]);
        assert_eq!(p.commands, Blocks::All);
        assert_eq!(p.container.kvm, Kvm::On);
        assert!(p.secrets.is_empty());
        assert_eq!(p.optional, Vec::<String>::new());
    }

    #[test]
    fn harnesses_default_to_the_harness() {
        let p = parse("harness: pi\n").unwrap();
        assert_eq!(p.harnesses, [Harness::Pi]);
    }

    #[test]
    fn harness_command_defaults_to_unset() {
        let p = parse("harness: opencode\n").unwrap();
        assert!(p.harness_command.opencode.is_empty());
        assert!(p.harness_command.claude.is_empty());
        assert!(p.harness_command.pi.is_empty());
    }

    #[test]
    fn harness_command_parses_per_harness() {
        let p = parse(&with("harness-command:\n  opencode: [opencode, mini]\n")).unwrap();
        assert_eq!(p.harness_command.opencode, ["opencode", "mini"]);
        assert!(p.harness_command.claude.is_empty());
        assert!(p.harness_command.pi.is_empty());
    }

    #[test]
    fn harness_command_wants_a_list_and_no_empty_arguments() {
        fails(
            &with("harness-command:\n  opencode: opencode mini\n"),
            "expected a list",
        );
        fails(
            &with("harness-command:\n  opencode: [opencode, \"\"]\n"),
            "harness-command.opencode: argument 2 is empty",
        );
        fails(
            &with("harness-command:\n  vim: [vim]\n"),
            "unknown field `vim`",
        );
        assert!(parse(&with("harness-command:\n  opencode: []\n")).is_ok());
    }

    #[test]
    fn missing_harness_is_an_error() {
        fails(
            "description: x\n",
            "p.yaml: harness: required, unless command is set",
        );
    }

    #[test]
    fn unknown_harness_is_an_error() {
        fails("harness: vim\n", "p.yaml:1: harness: unknown variant `vim`");
    }

    #[test]
    fn unknown_keys_are_errors_at_top_level_and_nested() {
        fails(
            &with("colour: red\n"),
            "p.yaml:2: colour: unknown field `colour`",
        );
        fails(
            &with("container:\n  network: pasta\n  kvmm: true\n"),
            "p.yaml:4: container.kvmm: unknown field `kvmm`",
        );
    }

    #[test]
    fn bad_harness_prefix_is_an_error() {
        fails(
            &with("rules: [vim:core]\n"),
            "rules: `vim:core`: the prefix before `:` must be claude, pi or opencode",
        );
        fails(
            &with("rules: [\"claude:\"]\n"),
            "`claude:` is not a block name",
        );
        fails(&with("skills: [a/b]\n"), "`a/b` is not a block name");
        assert!(parse(&with("rules: [claude:core]\n")).is_ok());
    }

    #[test]
    fn skills_must_be_all_or_a_list() {
        fails(
            &with("skills: some\n"),
            "p.yaml:2: skills: expected `all` or a list of block names, found `some`",
        );
    }

    #[test]
    fn pull_needs_a_pinned_image() {
        let msg = "image.ref must be pinned by digest or a version tag when image.source is pull";
        fails(&with("image:\n  source: pull\n  ref: x/y:latest\n"), msg);
        fails(&with("image:\n  source: pull\n  ref: x/y\n"), msg);
        fails(&with("image:\n  source: pull\n  ref: host:5000/y\n"), msg);
        assert!(parse(&with("image:\n  source: pull\n  ref: x/y:1.2\n")).is_ok());
        assert!(parse(&with("image:\n  source: pull\n  ref: host:5000/y:1\n")).is_ok());
        assert!(parse(&with("image:\n  source: pull\n  ref: x/y@sha256:abc\n")).is_ok());
        assert!(parse(&with("image:\n  source: pull\n")).is_ok());
        assert!(parse(&with("image:\n  ref: x/y:latest\n")).is_ok());
    }

    #[test]
    fn mount_needs_host_and_an_absolute_one() {
        fails(
            &with("mounts:\n  - mode: ro\n"),
            "p.yaml:3: mounts[0]: needs host or volume",
        );
        fails(
            &with("mounts:\n  - host: rel/dir\n"),
            "p.yaml:3: mounts[0].host: `rel/dir` is relative: make it absolute, start it with `~` or pass --relative-to DIR",
        );
        fails(
            &with("mounts:\n  - host: /a\n    container: b\n"),
            "mounts[0].container: `b` must be an absolute path",
        );
        fails(
            &with("mounts:\n  - host: /a\n    mode: rx\n"),
            "unknown variant `rx`",
        );
    }

    #[test]
    fn mount_defaults_and_tilde() {
        let p = parse(&with("mounts:\n  - host: ~/x\n    container: ~/y\n")).unwrap();
        assert_eq!(
            p.mounts[0].source,
            MountSource::Host(PathBuf::from("/home/alice/x"))
        );
        assert_eq!(p.mounts[0].container, PathBuf::from("/home/alice/y"));
        assert!(p.mounts[0].relabel, "relabel defaults to true");
    }

    #[test]
    fn mount_relabel_false() {
        let p = parse(&with("mounts:\n  - host: /srv/a\n    relabel: false\n")).unwrap();
        assert!(!p.mounts[0].relabel);
    }

    #[test]
    fn relative_mount_resolves_against_relative_to() {
        let mut e = env();
        e.relative_to = Some(PathBuf::from("/srv/checkout"));
        let text = with(
            "mounts:\n  - host: secrets/ssh\n    container: /run/ssh\n  - host: ../x/./y\n  - host: /abs\n  - host: ~/t\n",
        );
        let raw: RawProfile = yaml::from_str(&text, "p.yaml").unwrap();
        let p = validate(raw, "p", "p.yaml", &e).unwrap();
        let hosts: Vec<_> = p.mounts.iter().map(|m| m.source.clone()).collect();
        assert_eq!(
            hosts,
            [
                MountSource::Host(PathBuf::from("/srv/checkout/secrets/ssh")),
                MountSource::Host(PathBuf::from("/srv/x/y")),
                MountSource::Host(PathBuf::from("/abs")),
                MountSource::Host(PathBuf::from("/home/alice/t")),
            ]
        );
        // Without a container path the mount goes to the resolved host path.
        assert_eq!(p.mounts[1].container, PathBuf::from("/srv/x/y"));
    }

    #[test]
    fn volume_mount() {
        let p = parse(&with(
            "mounts:\n  - volume: crew-data\n    container: /data\n    mode: ro\n",
        ))
        .unwrap();
        let m = &p.mounts[0];
        assert_eq!(m.source, MountSource::Volume("crew-data".into()));
        assert_eq!(m.container, PathBuf::from("/data"));
        assert!(m.read_only && !m.relabel && !m.optional);
    }

    #[test]
    fn volume_mount_errors() {
        fails(
            &with("mounts:\n  - volume: v\n"),
            "p.yaml:3: mounts[0]: a volume needs container",
        );
        fails(
            &with("mounts:\n  - volume: v\n    host: /a\n    container: /c\n"),
            "mounts[0]: host and volume exclude each other",
        );
        fails(
            &with("mounts:\n  - volume: -v\n    container: /c\n"),
            "mounts[0].volume: `-v` is not a valid volume name",
        );
        fails(
            &with("mounts:\n  - volume: v\n    container: /c\n    relabel: true\n"),
            "mounts[0].relabel: only for a host path, not a volume",
        );
        fails(
            &with("mounts:\n  - volume: v\n    container: /c\n    optional: true\n"),
            "mounts[0].optional: only for a host path, not a volume",
        );
    }

    #[test]
    fn secret_value_is_refused() {
        fails(
            &with("secrets:\n  k:\n    value: hunter2\n    source: env\n"),
            "p.yaml:4: secrets.k: a profile never holds a secret value; use source env or file",
        );
        // Even without a source.
        fails(
            &with("secrets:\n  k:\n    value: hunter2\n"),
            "a profile never holds a secret value",
        );
    }

    #[test]
    fn secret_source_rules() {
        fails(
            &with("secrets:\n  k:\n    source: env\n    env: [A]\n"),
            "secrets.k: source env needs var",
        );
        fails(
            &with("secrets:\n  k:\n    source: file\n    env: [A]\n"),
            "secrets.k: source file needs path",
        );
        fails(
            &with("secrets:\n  k:\n    source: file\n    path: /x\n    var: V\n    env: [A]\n"),
            "var is only for source env",
        );
        fails(
            &with("secrets:\n  k:\n    source: env\n    var: V\n    path: /x\n    env: [A]\n"),
            "path and mount are only for source file",
        );
        fails(
            &with("secrets:\n  k:\n    source: broker\n    env: [A]\n"),
            "secrets.k: source broker needs url",
        );
    }

    const BROKER: &str = "secrets:\n  k:\n    source: broker\n    url: https://b.example/\n    token: ~/t\n    route: anthropic\n";

    #[test]
    fn broker_secret_parses() {
        let p = parse(&with(&format!("{BROKER}    env: [X]\n"))).unwrap();
        let s = &p.secrets[0];
        assert_eq!(
            s.source,
            SecretSrc::Broker {
                url: "https://b.example".to_string(),
                token: PathBuf::from("/home/alice/t"),
            }
        );
        assert_eq!(s.env, ["X"]);
        assert!(s.tilde);
    }

    #[test]
    fn broker_secret_key_rules() {
        fails(
            &with(&BROKER.replace("    url: https://b.example/\n", "")),
            "source broker needs url",
        );
        fails(
            &with(&BROKER.replace("    token: ~/t\n", "")),
            "source broker needs token",
        );
        fails(
            &with(&BROKER.replace("    route: anthropic\n", "")),
            "source broker needs route",
        );
        fails(
            &with(&BROKER.replace("anthropic", "openai")),
            "secrets.k.route: `openai` is not a route",
        );
        fails(
            &with(&BROKER.replace("https://b.example/", "b.example")),
            "secrets.k.url: `b.example` must start with http://",
        );
        fails(
            &with(&BROKER.replace("~/t", "rel")),
            "secrets.k.token: `rel` must be an absolute path",
        );
        fails(
            &with(&format!("{BROKER}    var: V\n")),
            "var, path and mount are not for source broker",
        );
        fails(
            &with(&format!("{BROKER}    mount: /x\n")),
            "var, path and mount are not for source broker",
        );
        fails(
            &with(&format!("{BROKER}    harness-env:\n      claude: [A]\n")),
            "harness-env is not for source broker",
        );
        fails(
            &with(&format!("{BROKER}    value: x\n")),
            "never holds a secret value",
        );
        fails(
            &with("secrets:\n  k:\n    source: env\n    var: V\n    env: [A]\n    token: /x\n"),
            "url, token and route are only for source broker",
        );
        fails(
            &with(
                "secrets:\n  k:\n    source: file\n    path: /x\n    env: [A]\n    url: http://x\n",
            ),
            "url, token and route are only for source broker",
        );
    }

    #[test]
    fn broker_profile_is_claude_only() {
        for (head, _) in [
            ("harness: pi\n", ""),
            ("harness: claude\nharnesses: [claude, pi]\n", ""),
            ("command: [x]\n", ""),
        ] {
            let e = parse(&format!("{head}{BROKER}")).unwrap_err().0;
            assert!(
                e.contains("source broker works with the claude harness only"),
                "{head}: {e}"
            );
        }
        assert!(parse(&with(&format!("harnesses: [claude]\n{BROKER}"))).is_ok());
        fails(
            &with(&format!(
                "{BROKER}  j:\n    source: broker\n    url: http://x\n    token: /t\n    route: anthropic\n"
            )),
            "only one secret may have source broker",
        );
    }

    #[test]
    fn broker_profile_sets_no_other_credential() {
        for (extra, needle) in [
            (
                "secrets:\n  o:\n    source: env\n    var: V\n    env: [ANTHROPIC_API_KEY]\n",
                "`ANTHROPIC_API_KEY` is not allowed",
            ),
            (
                "secrets:\n  o:\n    source: env\n    var: V\n    harness-env:\n      claude: [CLAUDE_CODE_OAUTH_TOKEN]\n",
                "`CLAUDE_CODE_OAUTH_TOKEN` is not allowed",
            ),
            (
                "container:\n  pass-env: [ANTHROPIC_API_KEY]\n",
                "`ANTHROPIC_API_KEY` is not allowed",
            ),
            (
                "container:\n  env:\n    ANTHROPIC_BASE_URL: http://x\n",
                "`ANTHROPIC_BASE_URL` is not allowed",
            ),
        ] {
            // `secrets:` appears once, so merge the extra secret into the broker's map.
            let text = if let Some(rest) = extra.strip_prefix("secrets:\n") {
                with(&format!("{BROKER}{rest}"))
            } else {
                with(&format!("{BROKER}{extra}"))
            };
            fails(&text, needle);
        }
        fails(
            &with(&format!("{BROKER}    env: [ANTHROPIC_CUSTOM_HEADERS]\n")),
            "bree sets the broker's variables itself",
        );
    }

    #[test]
    fn secret_going_nowhere_is_an_error() {
        fails(
            &with("secrets:\n  k:\n    source: env\n    var: V\n"),
            "secrets.k: goes nowhere",
        );
        assert!(
            parse(&with(
                "secrets:\n  k:\n    source: file\n    path: /x\n    mount: /run/secrets/x\n"
            ))
            .is_ok()
        );
    }

    #[test]
    fn secret_mount_must_be_absolute() {
        fails(
            &with("secrets:\n  k:\n    source: file\n    path: /x\n    mount: rel\n"),
            "secrets.k.mount: `rel` must be an absolute path",
        );
    }

    #[test]
    fn bad_env_names_are_errors() {
        fails(
            &with("secrets:\n  k:\n    source: env\n    var: V\n    env: [\"1A\"]\n"),
            "secrets.k.env: `1A` is not a valid environment variable name",
        );
        fails(
            &with(
                "secrets:\n  k:\n    source: env\n    var: V\n    harness-env:\n      pi: [a-b]\n",
            ),
            "secrets.k.harness-env.pi: `a-b`",
        );
        fails(
            &with("container:\n  pass-env: [\"A B\"]\n"),
            "container.pass-env: `A B`",
        );
        fails(
            &with("container:\n  env:\n    \"x y\": z\n"),
            "container.env: `x y`",
        );
    }

    #[test]
    fn container_env_value_must_be_a_string() {
        fails(
            &with("container:\n  env:\n    A: [1, 2]\n"),
            "p.yaml:4: container.env.A: expected a string, found a list",
        );
    }

    #[test]
    fn container_env_keeps_file_order() {
        let p = parse(&with("container:\n  env:\n    B: '1'\n    A: 2\n")).unwrap();
        assert_eq!(
            p.container.env.0,
            [
                ("B".to_string(), "1".to_string()),
                ("A".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn yes_is_a_string_for_a_string_key_and_an_error_for_a_bool_key() {
        let p = parse(&with("container:\n  env:\n    A: no\n    B: on\n")).unwrap();
        assert_eq!(p.container.env.0[0].1, "no");
        fails(
            &with("container:\n  nested-podman: yes\n"),
            "p.yaml:3: container.nested-podman: expected true or false, found `yes`",
        );
        fails(
            &with("container:\n  kvm: yes\n"),
            "p.yaml:3: container.kvm: expected true, false or optional, found `yes`",
        );
    }

    const CREW: &str = "command: [crew-agent, review]\nstate:\n  home: image\n";

    #[test]
    fn crew_profile_parses() {
        let p = parse(&format!(
            "{CREW}image:\n  ref: localhost/crew:latest\n  source: local\n\
             container:\n  name: crew-review\n  workdir: /home/agent/code/\n  kvm: optional\n  \
             storage-volume: crew-storage\n"
        ))
        .unwrap();
        assert_eq!(p.harness, None);
        assert!(p.harnesses.is_empty());
        assert_eq!(
            p.command,
            Some(vec!["crew-agent".to_string(), "review".to_string()])
        );
        assert_eq!(p.state_dir, None);
        assert_eq!(p.image.source, Source::Local);
        assert_eq!(p.container.name.as_deref(), Some("crew-review"));
        assert_eq!(
            p.container.workdir,
            Workdir::Path(PathBuf::from("/home/agent/code"))
        );
        assert_eq!(p.container.kvm, Kvm::Optional);
        assert_eq!(p.container.storage_volume.as_deref(), Some("crew-storage"));
    }

    #[test]
    fn command_with_a_harness_is_an_error() {
        fails(
            "harness: claude\ncommand: [x]\n",
            "p.yaml: command: command and harness exclude each other",
        );
        fails(
            "harnesses: [pi]\ncommand: [x]\n",
            "p.yaml: command: command and harnesses exclude each other",
        );
        fails(
            "command: [x]\nharness-command:\n  opencode: [opencode, mini]\n",
            "p.yaml: command: command and harness-command exclude each other",
        );
        fails(
            "command: []\n",
            "p.yaml: command: command must not be empty",
        );
        assert!(parse("command: [x]\n").is_ok());
    }

    #[test]
    fn image_home_with_rendered_config_is_an_error() {
        let msg = "p.yaml: state.home: state.home: image keeps the image's home, so the profile renders nothing";
        fails(&with("state:\n  home: image\n"), msg);
        for extra in [
            "rules: [core]\n",
            "skills: all\n",
            "commands: [c]\n",
            "settings:\n  claude: [s]\n",
            "files:\n  pi: [f]\n",
        ] {
            fails(&format!("{CREW}{extra}"), msg);
        }
        fails(
            &format!("{CREW}  dir: /x\n"),
            "state.dir and state.home: image exclude each other",
        );
        fails(
            &with("state:\n  home: host\n"),
            "p.yaml:3: state.home: unknown variant `host`",
        );
    }

    #[test]
    fn crew_container_keys_are_checked() {
        fails(
            &with("container:\n  workdir: rel/dir\n"),
            "p.yaml:3: container.workdir: expected `cwd`, `mount` or an absolute path, found `rel/dir`",
        );
        fails(
            &with("container:\n  name: -x\n"),
            "container.name: `-x` is not a valid container name",
        );
        fails(
            &with("container:\n  storage-volume: a/b\n"),
            "container.storage-volume: `a/b` is not a valid volume name",
        );
        fails(
            &with("container:\n  nested-podman: false\n  storage-volume: v\n"),
            "container.storage-volume: needs container.nested-podman: true",
        );
        fails(
            &with("image:\n  source: local-ish\n"),
            "unknown variant `local-ish`",
        );
    }

    #[test]
    fn state_dir_is_expanded() {
        let p = parse(&with("state:\n  dir: ~/s\n")).unwrap();
        assert_eq!(p.state_dir, Some(PathBuf::from("/home/alice/s")));
    }
}
