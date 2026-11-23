//! `bree run`: turns a profile into one `podman run` (docs/design.md, "`bree
//! run`" and "The container"). [`build`] is pure apart from the secret lookup
//! it is given; the caller prints or execs the result.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::RunHarness;
use crate::config::{Env, expand_tilde, normalize};
use crate::error::{Error, Result};
use crate::profile::{
    BROKER_RESERVED_ENV, Harness, Kvm, MountSource, Profile, Secret, SecretSrc, Source, Workdir,
};

pub struct RunOpts<'a> {
    /// `None` starts the profile's harness.
    pub harness: Option<RunHarness>,
    pub args: &'a [String],
    pub mounts: &'a [String],
    pub no_pull: bool,
    pub envs: &'a [String],
}

/// The result of [`build`]. `groups` are the printed lines (`-e NAME` is one
/// group of two arguments); `args` is the same, flattened, with `podman` first.
#[derive(Debug)]
pub struct Invocation {
    pub groups: Vec<Vec<String>>,
    pub args: Vec<String>,
    /// Secret values for podman's environment, never in `args`.
    pub env: Vec<(String, String)>,
    pub warnings: Vec<String>,
    /// `None` when the profile keeps the image's home (`state.home: image`).
    pub state_dir: Option<PathBuf>,
    /// The mount destinations that lie in the state directory, as paths in it
    /// on the host, with the host source of each (`None` for a named volume).
    /// [`exec`] creates them before podman, see [`prepare_mountpoints`].
    pub mountpoints: Vec<(PathBuf, Option<PathBuf>)>,
}

impl Invocation {
    /// The command as `--dry-run` and `--verbose` print it.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, g) in self.groups.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&g.join(" "));
            out.push('\n');
        }
        out
    }
}

/// One mount of the container, with a description for error messages.
struct Vol {
    /// A host path, or a volume name when `volume` is set.
    src: String,
    dest: PathBuf,
    read_only: bool,
    /// `:z`: podman relabels the host path for the container.
    relabel: bool,
    volume: bool,
    origin: String,
}

impl Vol {
    fn arg(&self) -> String {
        let mut opts = Vec::new();
        if self.read_only {
            opts.push("ro");
        }
        if self.relabel {
            opts.push("z");
        }
        let mut arg = format!("{}:{}", self.src, self.dest.display());
        if !opts.is_empty() {
            arg.push(':');
            arg.push_str(&opts.join(","));
        }
        arg
    }
}

fn refuse_host(path: &Path, home: &Path) -> Result<()> {
    if path == Path::new("/") || home.starts_with(path) {
        return Err(Error::new(format!(
            "refusing to mount {}: it is / or the home directory or a parent of it, and the home must never reach the container",
            path.display()
        )));
    }
    Ok(())
}

fn refuse_dest(path: &Path, home: &Path) -> Result<()> {
    if path == Path::new("/") || home.starts_with(path) {
        return Err(Error::new(format!(
            "refusing mount destination {}: it is / or the home directory or a parent of it (it would hide the state directory)",
            path.display()
        )));
    }
    Ok(())
}

/// A `--mount` of the command line.
#[derive(Debug)]
struct CliMount {
    host: PathBuf,
    container: PathBuf,
    read_only: bool,
}

/// `HOST[:CONTAINER][:ro]` of `--mount`.
fn parse_mount_spec(spec: &str, env: &Env) -> Result<CliMount> {
    let parts: Vec<&str> = spec.split(':').collect();
    let mode = |m: &str| match m {
        "ro" => Ok(true),
        "rw" => Ok(false),
        _ => Err(Error::new(format!(
            "--mount {spec}: the mode must be ro or rw, got `{m}`"
        ))),
    };
    let (host, container, read_only) = match parts.as_slice() {
        [h] => (*h, None, false),
        [h, x] if matches!(*x, "ro" | "rw") => (*h, None, mode(x)?),
        [h, c] => (*h, Some(*c), false),
        [h, c, m] => (*h, Some(*c), mode(m)?),
        _ => {
            return Err(Error::new(format!(
                "--mount {spec}: expected HOST[:CONTAINER][:ro]"
            )));
        }
    };
    if host.is_empty() {
        return Err(Error::new(format!(
            "--mount {spec}: the host path is empty"
        )));
    }
    let host = expand_tilde(host, &env.home);
    let host = normalize(&if host.is_absolute() {
        host
    } else {
        env.cwd.join(host)
    });
    let container = match container {
        Some(c) => {
            let p = expand_tilde(c, &env.home);
            if !p.is_absolute() {
                return Err(Error::new(format!(
                    "--mount {spec}: the container path `{c}` must be absolute"
                )));
            }
            normalize(&p)
        }
        None => host.clone(),
    };
    Ok(CliMount {
        host,
        container,
        read_only,
    })
}

fn describe_source(s: &Secret) -> String {
    match &s.source {
        SecretSrc::Env { var } => format!("environment variable {var}"),
        SecretSrc::File { path } => format!("file {}", path.display()),
        SecretSrc::Broker { token, .. } => format!("broker token file {}", token.display()),
    }
}

/// The host's gws login files to mount: the plaintext credentials, and the
/// encrypted pair when both it and the key file are present. The token cache
/// stays in the container home, so gws can refresh it without writing the
/// host's files.
fn gws_login(home: &Path, env: &Env) -> Vec<&'static str> {
    let dir = home.join(".config/gws");
    let mut files = Vec::new();
    if env.exists(&dir.join("credentials.json")) {
        files.push("credentials.json");
    }
    if env.exists(&dir.join("credentials.enc")) && env.exists(&dir.join(".encryption_key")) {
        files.extend(["credentials.enc", ".encryption_key"]);
    }
    files
}

/// Reads a secret's value from its source: `None` when it is not available
/// (variable unset or empty, file missing or empty).
pub fn read_secret(secret: &Secret, env: &Env) -> Result<Option<String>> {
    match &secret.source {
        SecretSrc::Env { var } => Ok(env.var(var).filter(|v| !v.is_empty()).map(str::to_string)),
        SecretSrc::File { path } | SecretSrc::Broker { token: path, .. } => {
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(Error::new(format!(
                        "secret {}: cannot read {}: {e}",
                        secret.name,
                        path.display()
                    )));
                }
            };
            if meta.mode() & 0o077 != 0 {
                return Err(Error::new(format!(
                    "secret {}: {} is readable by group or others (mode {:03o}); run chmod 600 {}",
                    secret.name,
                    path.display(),
                    meta.mode() & 0o777,
                    path.display()
                )));
            }
            let text = std::fs::read_to_string(path).map_err(|e| {
                Error::new(format!(
                    "secret {}: cannot read {}: {e}",
                    secret.name,
                    path.display()
                ))
            })?;
            let text = text.trim_end_matches(['\n', '\r']);
            Ok((!text.is_empty()).then(|| text.to_string()))
        }
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Builds the podman invocation of docs/design.md, "`bree run`".
///
/// `roots` are the resolved config roots, `suffix` the four hex digits of the
/// container name, `lookup` reads a secret's value (see [`read_secret`]).
pub fn build(
    profile: &Profile,
    roots: &[PathBuf],
    opts: &RunOpts,
    env: &Env,
    suffix: &str,
    lookup: &dyn Fn(&Secret, &Env) -> Result<Option<String>>,
) -> Result<Invocation> {
    let home = &env.home;
    let cwd = normalize(&env.cwd);
    let fixed_workdir = match &profile.container.workdir {
        Workdir::Path(p) => Some(p.clone()),
        Workdir::Cwd | Workdir::Mount => None,
    };
    if fixed_workdir.is_none() {
        refuse_host(&cwd, home)?;
    }

    // What the container runs: the entry point and a harness, or the
    // profile's command.
    let (harness, mut tail): (Option<Harness>, Vec<String>) = match (&profile.command, opts.harness)
    {
        (Some(argv), None) => (None, argv.clone()),
        (Some(_), Some(_)) => {
            return Err(Error::new(format!(
                "profile {} runs its command and starts no harness; drop the harness argument",
                profile.name
            )));
        }
        (None, h) => {
            let harness = match h {
                None => Some(profile.harness.expect("validated: harness or command")),
                Some(RunHarness::Claude) => Some(Harness::Claude),
                Some(RunHarness::Pi) => Some(Harness::Pi),
                Some(RunHarness::Opencode) => Some(Harness::Opencode),
                Some(RunHarness::Shell) => None,
            };
            // The entry point loads the profile again, so its relative
            // mount paths need the same --relative-to.
            let mut tail = vec!["bree".to_string()];
            if let Some(dir) = &env.relative_to {
                tail.push("--relative-to".to_string());
                tail.push(dir.display().to_string());
            }
            tail.extend(["entrypoint", "--"].map(str::to_string));
            match harness {
                Some(h) if !profile.harness_command.get(h).is_empty() => {
                    tail.extend(profile.harness_command.get(h).iter().cloned())
                }
                Some(h) => tail.push(h.name().to_string()),
                None => tail.push("bash".to_string()),
            }
            (harness, tail)
        }
    };
    tail.extend(opts.args.iter().cloned());

    // The mounts the user asked for: the profile's and --mount.
    let mut user: Vec<Vol> = Vec::new();
    for m in &profile.mounts {
        let host = match &m.source {
            MountSource::Volume(name) => {
                user.push(Vol {
                    src: name.clone(),
                    dest: m.container.clone(),
                    read_only: m.read_only,
                    relabel: false,
                    volume: true,
                    origin: format!("profile volume {name}"),
                });
                continue;
            }
            MountSource::Host(host) => host,
        };
        refuse_host(host, home)?;
        if !env.exists(host) {
            if m.optional {
                continue;
            }
            return Err(Error::new(format!(
                "profile {}: mount host path {} does not exist",
                profile.name,
                host.display()
            )));
        }
        user.push(Vol {
            src: host.display().to_string(),
            dest: m.container.clone(),
            read_only: m.read_only,
            relabel: m.relabel,
            volume: false,
            origin: format!("profile mount {}", host.display()),
        });
    }
    for spec in opts.mounts {
        let m = parse_mount_spec(spec, env)?;
        refuse_host(&m.host, home)?;
        if !env.exists(&m.host) {
            return Err(Error::new(format!(
                "--mount {spec}: host path {} does not exist",
                m.host.display()
            )));
        }
        user.push(Vol {
            src: m.host.display().to_string(),
            dest: m.container,
            read_only: m.read_only,
            relabel: false,
            volume: false,
            origin: format!("--mount {spec}"),
        });
    }
    for v in &user {
        refuse_dest(&v.dest, home)?;
    }

    // The working directory: fixed by the profile, or the current directory,
    // covered by the deepest mount that holds it.
    let covering = user
        .iter()
        .filter(|v| !v.volume && cwd.starts_with(&v.src))
        .max_by_key(|v| Path::new(&v.src).components().count());
    let workdir = match (fixed_workdir, covering) {
        (Some(w), _) => w,
        (None, Some(v)) => {
            let rest = cwd.strip_prefix(&v.src).expect("checked by starts_with");
            normalize(&v.dest.join(rest))
        }
        (None, None) => {
            if profile.container.workdir == Workdir::Mount {
                return Err(Error::new(format!(
                    "the working directory {} is not under any mount of profile {}, and container.workdir is mount",
                    cwd.display(),
                    profile.name
                )));
            }
            user.push(Vol {
                src: cwd.display().to_string(),
                dest: cwd.clone(),
                read_only: false,
                relabel: false,
                volume: false,
                origin: "the working directory".to_string(),
            });
            cwd.clone()
        }
    };
    // A parent before what is under it (Path orders by components).
    user.sort_by(|a, b| a.dest.cmp(&b.dest));

    // A broker profile never spends from a stored login: claude would take
    // `.credentials.json` over the token it is given.
    let broker = profile
        .secrets
        .iter()
        .any(|s| matches!(s.source, SecretSrc::Broker { .. }));
    if broker {
        if harness != Some(Harness::Claude) {
            return Err(Error::new(format!(
                "profile {} has a broker secret, which only the claude harness may use; run `bree run claude`",
                profile.name
            )));
        }
        if let Some(bad) = opts
            .envs
            .iter()
            .find(|n| BROKER_RESERVED_ENV.contains(&n.as_str()))
        {
            return Err(Error::new(format!(
                "--env {bad}: not allowed in a profile with a broker secret"
            )));
        }
        if let Some(dir) = &profile.state_dir {
            let creds = dir.join(".claude/.credentials.json");
            if std::fs::symlink_metadata(&creds).is_ok() {
                return Err(Error::new(format!(
                    "profile {}: {} holds a stored login, which a broker job must not spend from; move it away (for example mv {} {}.away) and run again",
                    profile.name,
                    creds.display(),
                    creds.display(),
                    creds.display()
                )));
            }
        }
    }

    // Secrets, in profile order.
    let mut plain_env: Vec<(String, String)> = Vec::new();
    let mut warnings = Vec::new();
    let mut secret_env: Vec<(String, String)> = Vec::new();
    let mut secret_names: Vec<String> = Vec::new();
    let mut secret_mounts: Vec<Vol> = Vec::new();
    for s in &profile.secrets {
        let Some(value) = lookup(s, env)? else {
            if s.required {
                return Err(Error::new(format!(
                    "secret {}: {} is not available",
                    s.name,
                    describe_source(s)
                )));
            }
            warnings.push(format!(
                "bree: secret {}: {} is not available, starting without it",
                s.name,
                describe_source(s)
            ));
            continue;
        };
        let mut names: Vec<&String> = s.env.iter().collect();
        if let SecretSrc::Broker { url, .. } = &s.source {
            if value.chars().any(char::is_control) {
                return Err(Error::new(format!(
                    "secret {}: the token file holds a control character; it must be one line",
                    s.name
                )));
            }
            plain_env.push(("ANTHROPIC_BASE_URL".into(), format!("{url}/anthropic")));
            plain_env.push((
                "ANTHROPIC_AUTH_TOKEN".into(),
                "praxis-substitute:anthropic".into(),
            ));
            secret_names.push("ANTHROPIC_CUSTOM_HEADERS".into());
            secret_env.push((
                "ANTHROPIC_CUSTOM_HEADERS".into(),
                format!("x-run-token: {value}"),
            ));
        }
        if let Some(h) = harness {
            names.extend(s.harness_env.get(h));
        }
        for n in names {
            if !secret_names.contains(n) {
                secret_names.push(n.clone());
                secret_env.push((n.clone(), value.clone()));
            }
        }
        if let (Some(mount), SecretSrc::File { path }) = (&s.mount, &s.source) {
            secret_mounts.push(Vol {
                src: path.display().to_string(),
                dest: mount.clone(),
                read_only: true,
                relabel: false,
                volume: false,
                origin: format!("secret {}", s.name),
            });
        }
    }

    // Assemble the volumes in the design's order and check the destinations.
    let mut vols: Vec<Vol> = Vec::new();
    if let Some(dir) = &profile.state_dir {
        refuse_host(dir, home)?;
        vols.push(Vol {
            src: dir.display().to_string(),
            dest: home.clone(),
            read_only: false,
            relabel: false,
            volume: false,
            origin: "the state directory".to_string(),
        });
    }
    for (n, r) in roots.iter().enumerate() {
        vols.push(Vol {
            src: r.display().to_string(),
            dest: PathBuf::from(format!("/run/bree/roots/{n}")),
            read_only: true,
            relabel: false,
            volume: false,
            origin: format!("config root {}", r.display()),
        });
    }
    if profile.container.nested_podman {
        let volume = match &profile.container.storage_volume {
            Some(v) => v.clone(),
            None => format!("bree-{}-containers", profile.name),
        };
        vols.push(Vol {
            src: volume,
            dest: home.join(".local/share/containers"),
            read_only: false,
            relabel: false,
            volume: true,
            origin: "the nested podman storage".to_string(),
        });
    }
    vols.extend(user);
    vols.extend(secret_mounts);
    // gws is in the base image; share the host's login when it has one. A
    // profile mount at the same destination wins, so a profile can replace it.
    let gws_dir = home.join(".config/gws");
    for name in gws_login(home, env) {
        let dest = gws_dir.join(name);
        if vols.iter().any(|v| v.dest == dest) {
            continue;
        }
        vols.push(Vol {
            src: gws_dir.join(name).display().to_string(),
            dest,
            read_only: true,
            relabel: false,
            volume: false,
            origin: format!("the host's gws login ({name})"),
        });
    }
    let mut seen: HashMap<&Path, &str> = HashMap::new();
    for v in &vols {
        if let Some(first) = seen.insert(&v.dest, &v.origin) {
            return Err(Error::new(format!(
                "mount destination {} is used twice: by {first} and by {}",
                v.dest.display(),
                v.origin
            )));
        }
    }

    // The command line.
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut line = |parts: &[&str]| groups.push(parts.iter().map(|s| s.to_string()).collect());
    line(&["podman", "run", "--rm", "-it", "--init"]);
    let name = match &profile.container.name {
        Some(n) => n.clone(),
        None => {
            // The current directory's name, or the fixed workdir's.
            let named = match &profile.container.workdir {
                Workdir::Path(_) => &workdir,
                Workdir::Cwd | Workdir::Mount => &cwd,
            };
            let base = named
                .file_name()
                .map(|n| sanitize(&n.to_string_lossy()))
                .unwrap_or_default();
            format!("bree-{}-{base}-{suffix}", profile.name)
        }
    };
    line(&["--name", &name]);
    line(&["--userns=keep-id:uid=1000,gid=1000"]);
    line(&[&format!(
        "--passwd-entry={}:*:1000:1000::{}:/bin/bash",
        env.user,
        home.display()
    )]);
    if profile.container.nested_podman || profile.container.label_disable {
        line(&["--security-opt=label=disable"]);
    }
    let kvm = match profile.container.kvm {
        Kvm::Off => false,
        Kvm::On => true,
        Kvm::Optional => env.exists(Path::new("/dev/kvm")),
    };
    if kvm {
        line(&["--device=/dev/kvm"]);
        line(&["--security-opt=unmask=ALL"]);
    }
    match profile.container.tz.as_str() {
        "local" => {
            if env.localtime_exists {
                line(&["--tz=local"]);
            }
        }
        zone => line(&[&format!("--tz={zone}")]),
    }
    line(&[&format!("--network={}", profile.container.network.name())]);
    if opts.no_pull || profile.image.source == Source::Local {
        line(&["--pull=never"]);
    }
    let mut passed: Vec<&str> = Vec::new();
    for n in profile.container.pass_env.iter().chain(opts.envs.iter()) {
        if !passed.contains(&n.as_str()) {
            passed.push(n);
            line(&["-e", n]);
        }
    }
    line(&["-e", &format!("BREE_PROFILE={}", profile.name)]);
    for (k, v) in &profile.container.env.0 {
        line(&["-e", &format!("{k}={v}")]);
    }
    for (k, v) in &plain_env {
        line(&["-e", &format!("{k}={v}")]);
    }
    for n in &secret_names {
        line(&["-e", n]);
    }
    for v in &vols {
        line(&["-v", &v.arg()]);
    }
    line(&["-w", &workdir.display().to_string()]);
    line(&[&profile.image.reference]);
    groups.push(tail);

    let mut args: Vec<String> = groups.iter().flatten().cloned().collect();
    args.shrink_to_fit();
    Ok(Invocation {
        groups,
        args,
        env: secret_env,
        warnings,
        state_dir: profile.state_dir.clone(),
        mountpoints: match &profile.state_dir {
            Some(dir) => mountpoints(&vols, home, dir),
            None => Vec::new(),
        },
    })
}

/// The destinations of `vols` that podman would create in the state
/// directory: under the home and not inside another mount. `vols[0]` is the
/// state directory itself.
fn mountpoints(vols: &[Vol], home: &Path, state_dir: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut out = Vec::new();
    for v in &vols[1..] {
        let Ok(rel) = v.dest.strip_prefix(home) else {
            continue;
        };
        let inside_another = vols[1..]
            .iter()
            .any(|w| w.dest != v.dest && v.dest.starts_with(&w.dest));
        if rel.as_os_str().is_empty() || inside_another {
            continue;
        }
        let src = Path::new(&v.src);
        out.push((
            state_dir.join(rel),
            src.is_absolute().then(|| src.to_path_buf()),
        ));
    }
    out
}

/// Creates the mount destinations in the state directory as the user who
/// runs bree, so podman finds them present. Otherwise crun creates the
/// missing ones and their parents as the container's root, which is a subuid
/// on the host under `--userns=keep-id`, and the harness (uid 1000) can no
/// longer create its files next to them (a design note). A destination whose
/// source is a file gets its parent and an empty file.
///
/// Before it creates anything it refuses when a directory on the way, or a
/// destination, exists and belongs to another uid than `uid`: such a state
/// directory was left by an earlier run, and the error says how to repair it.
pub fn prepare_mountpoints(
    state_dir: &Path,
    mountpoints: &[(PathBuf, Option<PathBuf>)],
    uid: u32,
) -> Result<()> {
    let mut foreign: Vec<PathBuf> = Vec::new();
    for (target, _) in mountpoints {
        let Ok(rel) = target.strip_prefix(state_dir) else {
            continue;
        };
        let mut p = state_dir.to_path_buf();
        for c in rel.components() {
            p.push(c);
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.uid() != uid && !foreign.contains(&p) => foreign.push(p.clone()),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    if !foreign.is_empty() {
        let paths: Vec<String> = foreign.iter().map(|p| p.display().to_string()).collect();
        return Err(Error::new(format!(
            "the state directory {} has paths that belong to another user, created by podman on an earlier run: {}; give them back to you with: podman unshare chown 0:0 {}",
            state_dir.display(),
            paths.join(", "),
            paths.join(" ")
        )));
    }
    for (target, src) in mountpoints {
        let file = src
            .as_deref()
            .and_then(|s| std::fs::metadata(s).ok())
            .is_some_and(|m| !m.is_dir());
        let dir = if file {
            target.parent().unwrap_or(state_dir)
        } else {
            target
        };
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::new(format!(
                "cannot create the mount destination {}: {e}",
                dir.display()
            ))
        })?;
        if file && std::fs::symlink_metadata(target).is_err() {
            std::fs::File::create(target).map_err(|e| {
                Error::new(format!(
                    "cannot create the mount destination {}: {e}",
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}

/// Four hex digits for the container name: two random bytes.
pub fn random_suffix() -> String {
    let mut buf = [0u8; 2];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mixed = nanos ^ std::process::id();
        buf = [(mixed >> 8) as u8, mixed as u8];
    }
    format!("{:02x}{:02x}", buf[0], buf[1])
}

/// Prints warnings, creates the state directory and the mount destinations in
/// it, and replaces this process with podman. Only returns on failure.
pub fn exec(inv: &Invocation, verbose: bool) -> Error {
    if verbose {
        eprint!("{}", inv.render());
    }
    for w in &inv.warnings {
        eprintln!("{w}");
    }
    if let Some(dir) = &inv.state_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Error::new(format!(
                "cannot create the state directory {}: {e}",
                dir.display()
            ));
        }
        let uid = match std::fs::metadata("/proc/self") {
            Ok(m) => m.uid(),
            Err(e) => return Error::new(format!("cannot find the user id: {e}")),
        };
        if let Err(e) = prepare_mountpoints(dir, &inv.mountpoints, uid) {
            return e;
        }
    }
    let err = Command::new("podman")
        .args(&inv.args[1..])
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .exec();
    Error::new(format!("cannot run podman: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::{TempDir, env};
    use crate::profile;

    const WORK: &str = include_str!("../tests/fixtures/roots/shared/profiles/work.yaml");
    const HOME: &str = include_str!("../tests/fixtures/roots/shared/profiles/home.yaml");

    const ROOTS: [&str; 2] = [
        "/home/alice/.local/share/bree/roots/shared",
        "/home/alice/.config/bree/private",
    ];

    fn roots() -> Vec<PathBuf> {
        ROOTS.iter().map(PathBuf::from).collect()
    }

    fn load(text: &str, name: &str, e: &Env) -> Profile {
        let dir = TempDir::new("run");
        let path = dir.0.join(format!("{name}.yaml"));
        std::fs::write(&path, text).unwrap();
        profile::load(&path, name, e).unwrap()
    }

    fn opts<'a>() -> RunOpts<'a> {
        RunOpts {
            harness: None,
            args: &[],
            mounts: &[],
            no_pull: false,
            envs: &[],
        }
    }

    fn try_build(p: &Profile, o: &RunOpts, e: &Env) -> Result<Invocation> {
        build(p, &roots(), o, e, "5f2c", &read_secret)
    }

    fn lines(inv: &Invocation) -> Vec<String> {
        inv.render().lines().map(str::to_string).collect()
    }

    fn work_env() -> Env {
        let mut e = env();
        e.vars.insert("CORP_KEY".into(), "s3cret".into());
        e
    }

    fn minimal(extra: &str) -> String {
        format!("harness: claude\ncontainer:\n  nested-podman: false\n{extra}")
    }

    fn err(p: &Profile, o: &RunOpts, e: &Env) -> String {
        try_build(p, o, e).unwrap_err().0
    }

    #[test]
    fn work_dry_run_is_the_design_example() {
        let e = work_env();
        let p = load(WORK, "work", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        let expected = "\
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
";
        assert_eq!(inv.render(), expected);
        assert_eq!(
            inv.env,
            [
                ("CORP_KEY".to_string(), "s3cret".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "s3cret".to_string())
            ]
        );
        assert!(inv.warnings.is_empty());
        assert!(!inv.args.iter().any(|a| a.contains("s3cret")));
        assert_eq!(inv.args[..5], ["podman", "run", "--rm", "-it", "--init"]);
    }

    #[test]
    fn home_dry_run() {
        let mut e = env();
        e.cwd = PathBuf::from("/home/alice/src/project");
        let p = load(HOME, "home", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        let expected = "\
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
";
        assert_eq!(inv.render(), expected);
    }

    #[test]
    fn kvm_optional_passes_the_device_only_when_the_host_has_it() {
        let text = minimal("  kvm: optional\n");
        for present in [true, false] {
            let mut e = env();
            e.path_exists = Box::new(move |p| p != Path::new("/dev/kvm") || present);
            let p = load(&text, "p", &e);
            let l = lines(&try_build(&p, &opts(), &e).unwrap());
            assert_eq!(l.contains(&"  --device=/dev/kvm".to_string()), present);
            assert_eq!(
                l.contains(&"  --security-opt=unmask=ALL".to_string()),
                present
            );
        }
    }

    #[test]
    fn image_home_prepares_no_mountpoints() {
        let e = env();
        let text = "command: [x]\nstate:\n  home: image\nmounts:\n  - host: /srv/a\n    container: /home/alice/a\n";
        let p = load(text, "p", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        assert_eq!(inv.state_dir, None);
        assert!(inv.mountpoints.is_empty());
        assert!(lines(&inv).contains(&"  -v /srv/a:/home/alice/a:z".to_string()));
    }

    #[test]
    fn fixed_workdir_allows_running_from_the_home() {
        let mut e = env();
        e.cwd = PathBuf::from("/home/alice");
        let p = load(&minimal("  workdir: /srv/w\n"), "p", &e);
        let l = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(l.contains(&"  -w /srv/w".to_string()));
        let p = load(&minimal(""), "p", &e);
        assert!(err(&p, &opts(), &e).starts_with("refusing to mount /home/alice"));
    }

    #[test]
    fn optional_secret_missing_warns_and_passes_nothing() {
        let e = env();
        let p = load(WORK, "work", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        assert_eq!(
            inv.warnings,
            [
                "bree: secret corp: environment variable CORP_KEY is not available, starting without it"
            ]
        );
        assert!(inv.env.is_empty());
        assert!(!lines(&inv).iter().any(|l| l.contains("CORP_KEY")));
    }

    #[test]
    fn required_secret_missing_is_an_error() {
        let e = env();
        let text = minimal("secrets:\n  k:\n    source: env\n    var: K\n    env: [K]\n");
        let p = load(&text, "p", &e);
        assert_eq!(
            err(&p, &opts(), &e),
            "secret k: environment variable K is not available"
        );
        let mut e = env();
        e.vars.insert("K".into(), String::new());
        assert!(
            try_build(&p, &opts(), &e).is_err(),
            "empty counts as missing"
        );
    }

    #[test]
    fn file_secret_with_group_readable_mode_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("secret");
        let f = t.0.join("tok");
        std::fs::write(&f, "abc\n").unwrap();
        let e = env();
        let text = minimal(&format!(
            "secrets:\n  k:\n    source: file\n    path: {}\n    env: [TOK]\n",
            f.display()
        ));
        let p = load(&text, "p", &e);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let msg = err(&p, &opts(), &e);
        assert!(
            msg.contains("chmod 600") && msg.contains(&f.display().to_string()),
            "{msg}"
        );
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let inv = try_build(&p, &opts(), &e).unwrap();
        assert_eq!(inv.env, [("TOK".to_string(), "abc".to_string())]);
    }

    #[test]
    fn file_secret_mount_comes_last_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("secret");
        let f = t.0.join("tok");
        std::fs::write(&f, "abc").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let e = env();
        let text = minimal(&format!(
            "mounts:\n  - host: /srv/z\nsecrets:\n  k:\n    source: file\n    path: {}\n    mount: /a/secret\n",
            f.display()
        ));
        let p = load(&text, "p", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        let vs: Vec<_> = lines(&inv)
            .into_iter()
            .filter(|l| l.contains("-v "))
            .collect();
        assert_eq!(
            vs.last().unwrap(),
            &format!("  -v {}:/a/secret:ro", f.display())
        );
        assert!(vs[vs.len() - 2].contains("/srv/z"), "{vs:?}");
        assert!(inv.env.is_empty());
    }

    #[test]
    fn host_gws_login_is_mounted_read_only() {
        let mut e = env();
        e.path_exists = Box::new(|p| p.starts_with("/home/alice/.config/gws"));
        let p = load(&minimal(""), "p", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        let vs: Vec<_> = lines(&inv)
            .into_iter()
            .filter(|l| l.contains("-v "))
            .collect();
        for name in ["credentials.json", "credentials.enc", ".encryption_key"] {
            let path = format!("/home/alice/.config/gws/{name}");
            assert!(
                vs.contains(&format!("  -v {path}:{path}:ro")),
                "{name} is not mounted read-only: {vs:?}"
            );
        }
        assert_eq!(
            vs.last().unwrap(),
            "  -v /home/alice/.config/gws/.encryption_key:/home/alice/.config/gws/.encryption_key:ro"
        );
    }

    #[test]
    fn host_gws_encrypted_login_needs_the_key_file() {
        for present in [["credentials.enc"], [".encryption_key"]] {
            let mut e = env();
            let only = format!("/home/alice/.config/gws/{}", present[0]);
            e.path_exists = Box::new(move |p| p == Path::new(&only));
            let p = load(&minimal(""), "p", &e);
            let inv = try_build(&p, &opts(), &e).unwrap();
            assert!(
                !lines(&inv).iter().any(|l| l.contains("/.config/gws/")),
                "{} alone is mounted: {}",
                present[0],
                inv.render()
            );
        }
    }

    #[test]
    fn profile_mount_wins_over_the_host_gws_login() {
        let mut e = env();
        e.path_exists =
            Box::new(|p| p.starts_with("/home/alice/.config/gws") || p == Path::new("/srv/gws"));
        let p = load(
            &minimal(
                "mounts:\n  - host: /srv/gws\n    container: ~/.config/gws/credentials.json\n",
            ),
            "p",
            &e,
        );
        let inv = try_build(&p, &opts(), &e).unwrap();
        let ls = lines(&inv);
        assert!(
            ls.contains(&"  -v /srv/gws:/home/alice/.config/gws/credentials.json:z".to_string())
        );
        assert!(!ls.contains(
            &"  -v /home/alice/.config/gws/credentials.json:/home/alice/.config/gws/credentials.json:ro"
                .to_string()
        ));
    }

    #[test]
    fn shell_starts_bash_without_harness_env() {
        let e = work_env();
        let p = load(WORK, "work", &e);
        let o = RunOpts {
            harness: Some(RunHarness::Shell),
            ..opts()
        };
        let inv = try_build(&p, &o, &e).unwrap();
        let ls = lines(&inv);
        assert_eq!(ls.last().unwrap(), "  bree entrypoint -- bash");
        assert!(ls.contains(&"  -e CORP_KEY".to_string()));
        assert!(!ls.iter().any(|l| l.contains("ANTHROPIC_API_KEY")));
        let o = RunOpts {
            harness: Some(RunHarness::Pi),
            ..opts()
        };
        let ls = lines(&try_build(&p, &o, &e).unwrap());
        assert!(ls.contains(&"  -e OPENAI_API_KEY".to_string()));
        assert_eq!(ls.last().unwrap(), "  bree entrypoint -- pi");
    }

    #[test]
    fn args_follow_the_harness() {
        let e = env();
        let p = load(&minimal(""), "p", &e);
        let args = ["--continue".to_string(), "-p".to_string()];
        let o = RunOpts {
            args: &args,
            ..opts()
        };
        let ls = lines(&try_build(&p, &o, &e).unwrap());
        assert_eq!(
            ls.last().unwrap(),
            "  bree entrypoint -- claude --continue -p"
        );
    }

    #[test]
    fn harness_command_replaces_the_argv_but_not_the_harness() {
        let e = work_env();
        let text = minimal(
            "harness-command:\n  opencode: [opencode, mini]\nsecrets:\n  k:\n    source: env\n    var: CORP_KEY\n    env: [BASE_KEY]\n    harness-env:\n      opencode: [OPENCODE_KEY]\n",
        );
        let p = load(&text, "p", &e);
        let args = ["--continue".to_string()];
        let o = RunOpts {
            harness: Some(RunHarness::Opencode),
            args: &args,
            ..opts()
        };
        let inv = try_build(&p, &o, &e).unwrap();
        let ls = lines(&inv);
        assert_eq!(
            ls.last().unwrap(),
            "  bree entrypoint -- opencode mini --continue"
        );
        assert!(ls.contains(&"  -e BASE_KEY".to_string()));
        assert!(ls.contains(&"  -e OPENCODE_KEY".to_string()));
        assert_eq!(inv.env.len(), 2);

        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert_eq!(ls.last().unwrap(), "  bree entrypoint -- claude");

        let o = RunOpts {
            harness: Some(RunHarness::Shell),
            ..opts()
        };
        let ls = lines(&try_build(&p, &o, &e).unwrap());
        assert_eq!(ls.last().unwrap(), "  bree entrypoint -- bash");
    }

    #[test]
    fn mount_spec_forms() {
        let mut e = env();
        e.cwd = PathBuf::from("/home/alice/src/x");
        let m = |s: &str| parse_mount_spec(s, &e).unwrap();
        let c = |s: &str| (m(s).host, m(s).container, m(s).read_only);
        assert_eq!(c("/srv"), ("/srv".into(), "/srv".into(), false));
        assert_eq!(c("/srv:ro"), ("/srv".into(), "/srv".into(), true));
        assert_eq!(c("/srv:rw"), ("/srv".into(), "/srv".into(), false));
        assert_eq!(c("/srv:/mnt"), ("/srv".into(), "/mnt".into(), false));
        assert_eq!(c("/srv:/mnt:ro"), ("/srv".into(), "/mnt".into(), true));
        assert_eq!(c("/srv:/mnt:rw"), ("/srv".into(), "/mnt".into(), false));
        assert_eq!(
            c("rel:/mnt"),
            ("/home/alice/src/x/rel".into(), "/mnt".into(), false)
        );
        assert_eq!(
            c("~/d:~/e"),
            ("/home/alice/d".into(), "/home/alice/e".into(), false)
        );
        for bad in ["/srv:/mnt:xx", "/srv:rel", "/a:/b:ro:x", ":ro", ""] {
            assert!(parse_mount_spec(bad, &e).is_err(), "{bad}");
        }
    }

    #[test]
    fn mount_option_adds_a_volume_and_must_exist() {
        let t = TempDir::new("m");
        let mut e = env();
        let existing = t.0.clone();
        e.path_exists = Box::new(move |p| p.starts_with(&existing) || p == Path::new("/srv"));
        let p = load(&minimal(""), "p", &e);
        let specs = ["/srv:/mnt:ro".to_string()];
        let o = RunOpts {
            mounts: &specs,
            ..opts()
        };
        let ls = lines(&try_build(&p, &o, &e).unwrap());
        assert!(ls.contains(&"  -v /srv:/mnt:ro".to_string()));
        let specs = ["/nowhere".to_string()];
        let o = RunOpts {
            mounts: &specs,
            ..opts()
        };
        assert!(err(&p, &o, &e).contains("host path /nowhere does not exist"));
    }

    #[test]
    fn covered_cwd_gets_no_mount_and_is_translated() {
        let e = env();
        let p = load(
            &minimal("mounts:\n  - host: ~/src\n    container: /work\n"),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(ls.contains(&"  -w /work/shared".to_string()), "{ls:?}");
        assert_eq!(ls[1], "  --name bree-p-shared-5f2c");
        assert!(
            !ls.iter()
                .any(|l| l == "  -v /home/alice/src/shared:/home/alice/src/shared")
        );
    }

    #[test]
    fn deepest_covering_mount_wins() {
        let e = env();
        let p = load(
            &minimal(
                "mounts:\n  - host: ~/src\n    container: /a\n  - host: ~/src/shared\n    container: /b\n",
            ),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(ls.contains(&"  -w /b".to_string()), "{ls:?}");
    }

    #[test]
    fn uncovered_cwd_is_mounted_at_the_same_path() {
        let e = env();
        let p = load(&minimal(""), "p", &e);
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(ls.contains(&"  -v /home/alice/src/shared:/home/alice/src/shared".to_string()));
        assert!(ls.contains(&"  -w /home/alice/src/shared".to_string()));
    }

    #[test]
    fn workdir_mount_needs_a_covering_mount() {
        let e = env();
        let text = minimal("").replace("container:\n", "container:\n  workdir: mount\n");
        let p = load(&text, "p", &e);
        assert_eq!(
            err(&p, &opts(), &e),
            "the working directory /home/alice/src/shared is not under any mount of profile p, and container.workdir is mount"
        );
        let text = format!("{text}mounts:\n  - host: ~/src\n");
        let p = load(&text, "p", &e);
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(ls.contains(&"  -w /home/alice/src/shared".to_string()));
    }

    #[test]
    fn home_and_parents_and_root_are_refused() {
        let mut e = env();
        e.cwd = PathBuf::from("/home/alice");
        let p = load(&minimal(""), "p", &e);
        assert!(
            err(&p, &opts(), &e).starts_with("refusing to mount /home/alice: it is / or the home")
        );
        e.cwd = PathBuf::from("/home/alice/src/x");
        for host in ["/", "/home", "\"~\""] {
            let p = load(&minimal(&format!("mounts:\n  - host: {host}\n")), "p", &e);
            assert!(
                err(&p, &opts(), &e).starts_with("refusing to mount"),
                "{host}"
            );
        }
        let specs = ["/".to_string()];
        let o = RunOpts {
            mounts: &specs,
            ..opts()
        };
        let p = load(&minimal(""), "p", &e);
        assert!(err(&p, &o, &e).starts_with("refusing to mount /:"));
    }

    #[test]
    fn destination_that_hides_the_home_is_refused() {
        let e = env();
        let p = load(
            &minimal("mounts:\n  - host: /srv\n    container: /home\n"),
            "p",
            &e,
        );
        assert!(err(&p, &opts(), &e).starts_with("refusing mount destination /home"));
    }

    #[test]
    fn destination_used_twice_is_an_error_naming_both() {
        let e = env();
        let p = load(
            &minimal(
                "mounts:\n  - host: /srv/a\n    container: /m\n  - host: /srv/b\n    container: /m\n",
            ),
            "p",
            &e,
        );
        let msg = err(&p, &opts(), &e);
        assert!(
            msg.contains("/m is used twice") && msg.contains("/srv/a") && msg.contains("/srv/b"),
            "{msg}"
        );
    }

    #[test]
    fn optional_mount_missing_is_skipped_required_is_an_error() {
        let mut e = env();
        e.path_exists = Box::new(|p| p != Path::new("/srv/gone"));
        let p = load(
            &minimal("mounts:\n  - host: /srv/gone\n    optional: true\n"),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(!ls.iter().any(|l| l.contains("/srv/gone")));
        let p = load(&minimal("mounts:\n  - host: /srv/gone\n"), "p", &e);
        assert!(err(&p, &opts(), &e).contains("/srv/gone does not exist"));
    }

    #[test]
    fn kvm_lines() {
        let e = env();
        let p = load(
            &minimal("").replace("container:\n", "container:\n  kvm: true\n"),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        let i = ls.iter().position(|l| l == "  --device=/dev/kvm").unwrap();
        assert_eq!(ls[i + 1], "  --security-opt=unmask=ALL");
        let p = load(&minimal(""), "p", &e);
        assert!(
            !lines(&try_build(&p, &opts(), &e).unwrap())
                .iter()
                .any(|l| l.contains("kvm"))
        );
    }

    #[test]
    fn tz_variants() {
        let mut e = env();
        let p = load(
            &minimal("").replace("container:\n", "container:\n  tz: Europe/Prague\n"),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert!(ls.contains(&"  --tz=Europe/Prague".to_string()));
        e.localtime_exists = false;
        let p = load(&minimal(""), "p", &e);
        assert!(
            !lines(&try_build(&p, &opts(), &e).unwrap())
                .iter()
                .any(|l| l.contains("--tz"))
        );
        // A named zone does not need /etc/localtime.
        let p = load(
            &minimal("").replace("container:\n", "container:\n  tz: UTC\n"),
            "p",
            &e,
        );
        assert!(lines(&try_build(&p, &opts(), &e).unwrap()).contains(&"  --tz=UTC".to_string()));
    }

    #[test]
    fn no_pull_env_and_container_env() {
        let e = env();
        let text = minimal("").replace(
            "container:\n",
            "container:\n  pass-env: [TERM, EDITOR]\n  env:\n    B: '1'\n    A: two\n",
        );
        let p = load(&text, "p", &e);
        let envs = ["EDITOR".to_string(), "LANG".to_string()];
        let o = RunOpts {
            no_pull: true,
            envs: &envs,
            ..opts()
        };
        let ls = lines(&try_build(&p, &o, &e).unwrap());
        let net = ls.iter().position(|l| l == "  --network=pasta").unwrap();
        assert_eq!(ls[net + 1], "  --pull=never");
        assert_eq!(
            ls[net + 2..net + 8],
            [
                "  -e TERM",
                "  -e EDITOR",
                "  -e LANG",
                "  -e BREE_PROFILE=p",
                "  -e B=1",
                "  -e A=two"
            ]
        );
    }

    #[test]
    fn mounts_sort_parents_before_children() {
        let e = env();
        let p = load(
            &minimal(
                "mounts:\n  - host: /srv/b\n    container: /home/alice/src/sub\n  - host: /srv/a\n    container: /home/alice/src\n  - host: /srv/c\n    container: /home/alice/.cfg\n",
            ),
            "p",
            &e,
        );
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        let dests: Vec<&str> = ls
            .iter()
            .filter_map(|l| l.strip_prefix("  -v /srv/"))
            .collect();
        assert_eq!(
            dests,
            [
                "c:/home/alice/.cfg:z",
                "a:/home/alice/src:z",
                "b:/home/alice/src/sub:z"
            ]
        );
    }

    #[test]
    fn container_name_is_sanitized() {
        let mut e = env();
        e.cwd = PathBuf::from("/home/alice/src/my proj@1");
        let p = load(&minimal(""), "p", &e);
        let ls = lines(&try_build(&p, &opts(), &e).unwrap());
        assert_eq!(ls[1], "  --name bree-p-my-proj-1-5f2c");
    }

    #[test]
    fn mountpoints_are_the_destinations_in_the_state_directory() {
        let e = work_env();
        let p = load(WORK, "work", &e);
        let inv = try_build(&p, &opts(), &e).unwrap();
        let state = PathBuf::from("/home/alice/.local/share/bree/state/work");
        assert_eq!(
            inv.mountpoints,
            [
                (state.join(".local/share/containers"), None),
                (
                    state.join(".gitconfig"),
                    Some(PathBuf::from("/home/alice/.gitconfig"))
                ),
                (state.join("src"), Some(PathBuf::from("/home/alice/src"))),
            ]
        );
    }

    #[test]
    fn mountpoints_skip_destinations_inside_another_mount_or_outside_home() {
        let e = env();
        let p = load(
            &minimal(
                "mounts:\n  - host: /srv/b\n    container: /home/alice/src/sub\n  - host: /srv/a\n    container: /home/alice/src\n  - host: /srv/c\n    container: /opt/c\n",
            ),
            "p",
            &e,
        );
        let inv = try_build(&p, &opts(), &e).unwrap();
        let targets: Vec<&Path> = inv.mountpoints.iter().map(|(t, _)| t.as_path()).collect();
        assert_eq!(targets, [p.state_dir.clone().unwrap().join("src")]);
    }

    #[test]
    fn prepare_mountpoints_creates_directories_and_file_stubs() {
        let t = TempDir::new("mp");
        let state = t.0.join("state");
        std::fs::create_dir(&state).unwrap();
        let file = t.0.join("gitconfig");
        std::fs::write(&file, "x").unwrap();
        let dir = t.0.join("src");
        std::fs::create_dir(&dir).unwrap();
        let mps = vec![
            (state.join(".local/share/containers"), None),
            (state.join(".config/git/config"), Some(file)),
            (state.join("src"), Some(dir)),
            (state.join("gone"), Some(t.0.join("missing"))),
        ];
        let uid = std::fs::metadata(&state).unwrap().uid();
        prepare_mountpoints(&state, &mps, uid).unwrap();
        assert!(state.join(".local/share/containers").is_dir());
        assert!(state.join(".config/git").is_dir());
        let stub = std::fs::metadata(state.join(".config/git/config")).unwrap();
        assert!(stub.is_file() && stub.len() == 0);
        assert!(state.join("src").is_dir());
        assert!(
            state.join("gone").is_dir(),
            "a missing source counts as a directory"
        );
        // A second run finds everything present and changes nothing.
        prepare_mountpoints(&state, &mps, uid).unwrap();
    }

    #[test]
    fn prepare_mountpoints_refuses_paths_of_another_user() {
        let t = TempDir::new("mp");
        let state = t.0.join("state");
        std::fs::create_dir_all(state.join(".local/share")).unwrap();
        let mps = vec![
            (state.join(".local/share/containers"), None),
            (state.join("src"), None),
        ];
        let other = std::fs::metadata(&state).unwrap().uid() + 1;
        let msg = prepare_mountpoints(&state, &mps, other).unwrap_err().0;
        let local = state.join(".local").display().to_string();
        let share = state.join(".local/share").display().to_string();
        assert!(
            msg.contains(&format!("podman unshare chown 0:0 {local} {share}")),
            "{msg}"
        );
        assert!(!state.join("src").exists(), "nothing is created on refusal");
    }

    const BROKER: &str = "harness: claude\ncontainer:\n  nested-podman: false\nsecrets:\n  b:\n    source: broker\n    url: http://127.0.0.1:8088/\n    token: /run/tok\n    route: anthropic\n    env: [BROKER_TOKEN]\n";

    fn token(_: &Secret, _: &Env) -> Result<Option<String>> {
        Ok(Some("tok-abc123".to_string()))
    }

    #[test]
    fn broker_sets_the_claude_variables_and_passes_the_token_by_name() {
        let e = env();
        let p = load(BROKER, "b", &e);
        let inv = build(&p, &roots(), &opts(), &e, "5f2c", &token).unwrap();
        let l = lines(&inv);
        assert!(l.contains(&"  -e ANTHROPIC_BASE_URL=http://127.0.0.1:8088/anthropic".to_string()));
        assert!(l.contains(&"  -e ANTHROPIC_AUTH_TOKEN=praxis-substitute:anthropic".to_string()));
        assert!(l.contains(&"  -e ANTHROPIC_CUSTOM_HEADERS".to_string()));
        assert!(l.contains(&"  -e BROKER_TOKEN".to_string()));
        assert_eq!(
            inv.env,
            [
                (
                    "ANTHROPIC_CUSTOM_HEADERS".to_string(),
                    "x-run-token: tok-abc123".to_string()
                ),
                ("BROKER_TOKEN".to_string(), "tok-abc123".to_string())
            ]
        );
        assert!(!inv.render().contains("tok-abc123"));
        assert!(!inv.args.iter().any(|a| a.contains("tok-abc123")));
        for n in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            assert!(!inv.args.iter().any(|a| a.contains(n)), "{n}");
            assert!(!inv.env.iter().any(|(k, _)| k == n), "{n}");
        }
    }

    #[test]
    fn broker_token_file_has_the_file_secret_checks() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("broker-token");
        let f = t.0.join("tok");
        let text = BROKER.replace("/run/tok", &f.display().to_string());
        let e = env();
        let p = load(&text, "b", &e);
        // Missing: required by default.
        let m = err(&p, &opts(), &e);
        assert!(
            m.contains("broker token file") && m.contains("is not available"),
            "{m}"
        );
        std::fs::write(&f, "tok-abc123\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        let m = err(&p, &opts(), &e);
        assert!(m.contains("readable by group or others (mode 640)"), "{m}");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let inv = try_build(&p, &opts(), &e).unwrap();
        assert_eq!(inv.env[0].1, "x-run-token: tok-abc123");
    }

    #[test]
    fn broker_refuses_a_state_directory_with_a_stored_login() {
        let t = TempDir::new("broker-state");
        let text = format!("{BROKER}state:\n  dir: {}\n", t.0.display());
        let e = env();
        let p = load(&text, "b", &e);
        assert!(build(&p, &roots(), &opts(), &e, "5f2c", &token).is_ok());
        std::fs::create_dir_all(t.0.join(".claude")).unwrap();
        std::fs::write(t.0.join(".claude/.credentials.json"), "{}").unwrap();
        let m = build(&p, &roots(), &opts(), &e, "5f2c", &token)
            .unwrap_err()
            .0;
        assert!(
            m.contains(".claude/.credentials.json") && m.contains("move it away"),
            "{m}"
        );
    }

    #[test]
    fn broker_runs_the_claude_harness_only() {
        let e = env();
        let p = load(BROKER, "b", &e);
        let o = RunOpts {
            harness: Some(RunHarness::Shell),
            ..opts()
        };
        let m = build(&p, &roots(), &o, &e, "5f2c", &token).unwrap_err().0;
        assert!(m.contains("only the claude harness"), "{m}");
        let envs = ["ANTHROPIC_API_KEY".to_string()];
        let o = RunOpts {
            envs: &envs,
            ..opts()
        };
        let m = build(&p, &roots(), &o, &e, "5f2c", &token).unwrap_err().0;
        assert!(m.contains("--env ANTHROPIC_API_KEY"), "{m}");
    }

    #[test]
    fn random_suffix_is_four_hex_digits() {
        let s = random_suffix();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
