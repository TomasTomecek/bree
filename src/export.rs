//! `bree export quadlet`: the quadlet `[Container]` section of a crew profile
//! (docs/design.md, "Crew profiles and quadlets"). It shares `bree run`'s
//! profile model, not its argv: it reads no secret value, checks no host
//! path, and leaves out what exists only for the entry point or an
//! interactive run.

use std::path::{Path, PathBuf};

use crate::config::Env;
use crate::error::{Error, Result};
use crate::profile::{Kvm, MountSource, Profile, Source, Workdir};

/// One `Volume=` line, with a description for the duplicate error.
struct Vol {
    /// Already escaped for the unit file.
    src: String,
    dest: PathBuf,
    read_only: bool,
    relabel: bool,
    origin: String,
}

impl Vol {
    fn line(&self) -> String {
        let mut opts = Vec::new();
        if self.read_only {
            opts.push("ro");
        }
        if self.relabel {
            opts.push("z");
        }
        let mut v = format!(
            "Volume={}:{}",
            self.src,
            esc(&self.dest.display().to_string())
        );
        if !opts.is_empty() {
            v.push(':');
            v.push_str(&opts.join(","));
        }
        v
    }
}

/// A literal `%` is a specifier in a unit file; `%%` is the `%` itself.
/// Quadlet copies every value into `ExecStart=`, where systemd expands
/// `$NAME`, so a literal `$` is written `$$`.
fn esc(s: &str) -> String {
    s.replace('%', "%%").replace('$', "$$")
}

/// A host path for the unit: one written with `~` becomes `%h`, the home of
/// the user who runs the unit, not the exporter's.
fn host(path: &Path, tilde: bool, env: &Env, what: &str) -> Result<String> {
    if !tilde {
        return Ok(esc(&path.display().to_string()));
    }
    match path.strip_prefix(&env.home) {
        Ok(rest) if rest.as_os_str().is_empty() => Ok("%h".to_string()),
        Ok(rest) => Ok(format!("%h/{}", esc(&rest.display().to_string()))),
        Err(_) => Err(Error::new(format!(
            "{what}: {} leaves the home it starts from with `~`, so it has no %h form",
            path.display()
        ))),
    }
}

/// One word of a command line as systemd splits `Exec=`: bare when it has no
/// space, quote, backslash or control character, otherwise double-quoted,
/// with control characters as C escapes (`\n`, `\t`, `\x01`) so the word
/// stays on one line.
fn word(arg: &str) -> String {
    let plain = !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'));
    if plain {
        return esc(arg);
    }
    let mut q = String::with_capacity(arg.len() + 2);
    for c in arg.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' => q.push_str("\\n"),
            '\r' => q.push_str("\\r"),
            '\t' => q.push_str("\\t"),
            c if (c as u32) < 0x80 && c.is_control() => q.push_str(&format!("\\x{:02x}", c as u32)),
            c => q.push(c),
        }
    }
    format!("\"{}\"", esc(&q))
}

/// The `[Container]` section of `profile`, one line per key, ending in a
/// newline.
pub fn quadlet(profile: &Profile, env: &Env) -> Result<String> {
    let name = &profile.name;
    let Some(command) = &profile.command else {
        return Err(Error::new(format!(
            "export quadlet: profile {name} starts a harness; only a profile with command and state.home: image can be exported"
        )));
    };
    if profile.state_dir.is_some() {
        return Err(Error::new(format!(
            "export quadlet: profile {name} has a state directory; only a profile with command and state.home: image can be exported"
        )));
    }

    // Volumes in `bree run`'s order: the nested storage, the mounts by
    // destination, then the file secrets.
    let mut vols: Vec<Vol> = Vec::new();
    if profile.container.nested_podman {
        let volume = match &profile.container.storage_volume {
            Some(v) => v.clone(),
            None => format!("bree-{name}-containers"),
        };
        vols.push(Vol {
            src: esc(&volume),
            dest: env.home.join(".local/share/containers"),
            read_only: false,
            relabel: false,
            origin: "the nested podman storage".to_string(),
        });
    }
    let mut mounts: Vec<Vol> = Vec::new();
    for m in &profile.mounts {
        let (src, origin) = match &m.source {
            MountSource::Volume(v) => (esc(v), format!("profile volume {v}")),
            MountSource::Host(h) => {
                let origin = format!("profile mount {}", h.display());
                (host(h, m.tilde, env, &origin)?, origin)
            }
        };
        mounts.push(Vol {
            src,
            dest: m.container.clone(),
            read_only: m.read_only,
            relabel: m.relabel,
            origin,
        });
    }
    mounts.sort_by(|a, b| a.dest.cmp(&b.dest));
    vols.extend(mounts);
    for s in &profile.secrets {
        if matches!(s.source, crate::profile::SecretSrc::Broker { .. }) {
            return Err(Error::new(format!(
                "export quadlet: secret {} has source broker, which the export does not support: its token goes into the environment of a bree run",
                s.name
            )));
        }
        let names: Vec<&String> = s
            .env
            .iter()
            .chain(
                crate::profile::Harness::ALL
                    .iter()
                    .flat_map(|&h| s.harness_env.get(h)),
            )
            .collect();
        if !names.is_empty() {
            return Err(Error::new(format!(
                "export quadlet: secret {} passes its value in {}, which the export never reads; mount the file read-only instead",
                s.name,
                names
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if let (Some(mount), crate::profile::SecretSrc::File { path }) = (&s.mount, &s.source) {
            let origin = format!("secret {}", s.name);
            vols.push(Vol {
                src: host(path, s.tilde, env, &origin)?,
                dest: mount.clone(),
                read_only: true,
                relabel: false,
                origin,
            });
        }
    }
    for (i, v) in vols.iter().enumerate() {
        if let Some(first) = vols[..i].iter().find(|w| w.dest == v.dest) {
            return Err(Error::new(format!(
                "mount destination {} is used twice: by {} and by {}",
                v.dest.display(),
                first.origin,
                v.origin
            )));
        }
    }

    let c = &profile.container;
    let mut out = vec!["[Container]".to_string()];
    out.push(format!("Image={}", esc(&profile.image.reference)));
    if profile.image.source == Source::Local {
        out.push("Pull=never".to_string());
    }
    if let Some(n) = &c.name {
        out.push(format!("ContainerName={}", esc(n)));
    }
    out.push(format!("Timezone={}", esc(&c.tz)));
    out.push("UserNS=keep-id".to_string());
    if c.nested_podman || c.label_disable {
        out.push("SecurityLabelDisable=true".to_string());
    }
    match c.kvm {
        Kvm::Off => {}
        Kvm::On => out.push("AddDevice=/dev/kvm".to_string()),
        Kvm::Optional => out.push("AddDevice=-/dev/kvm".to_string()),
    }
    if c.kvm != Kvm::Off {
        out.push("Unmask=ALL".to_string());
    }
    out.push("RunInit=true".to_string());
    out.push("LogDriver=passthrough".to_string());
    out.push(format!("Network={}", c.network.name()));
    if let Workdir::Path(w) = &c.workdir {
        out.push(format!("WorkingDir={}", esc(&w.display().to_string())));
    }
    let exec: Vec<String> = command.iter().map(|a| word(a)).collect();
    out.push(format!("Exec={}", exec.join(" ")));
    out.extend(vols.iter().map(Vol::line));
    for (k, v) in &c.env.0 {
        out.push(format!("Environment={}", word(&format!("{k}={v}"))));
    }
    for n in &c.pass_env {
        out.push(format!("PodmanArgs=--env={n}"));
    }
    // `word()` escapes control characters; a path or name cannot be quoted,
    // so one there would break its line.
    if let Some(line) = out.iter().find(|l| l.chars().any(|c| c.is_control())) {
        let key = line.split('=').next().unwrap_or(line);
        return Err(Error::new(format!(
            "export quadlet: the {key}= value of profile {name} holds a control character, which a unit file cannot hold there"
        )));
    }
    let mut text = out.join("\n");
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::{TempDir, env};
    use crate::profile;

    fn export(text: &str) -> Result<String> {
        let mut e = env();
        e.relative_to = Some(PathBuf::from("/srv/checkout"));
        let dir = TempDir::new("export");
        let path = dir.0.join("p.yaml");
        std::fs::write(&path, text).unwrap();
        let p = profile::load(&path, "p", &e).unwrap();
        quadlet(&p, &e)
    }

    const BASE: &str = "command: [x]\nstate:\n  home: image\n";

    fn lines(extra: &str) -> Vec<String> {
        export(&format!("{BASE}{extra}"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn defaults() {
        assert_eq!(
            lines(""),
            [
                "[Container]",
                &format!("Image=localhost/bree-base:{}", env!("CARGO_PKG_VERSION")),
                "Timezone=local",
                "UserNS=keep-id",
                "SecurityLabelDisable=true",
                "RunInit=true",
                "LogDriver=passthrough",
                "Network=pasta",
                "Exec=x",
                "Volume=bree-p-containers:/home/alice/.local/share/containers",
                "PodmanArgs=--env=TERM",
            ]
        );
    }

    #[test]
    fn kvm_true_and_optional() {
        let on = lines("container:\n  kvm: true\n");
        assert!(on.contains(&"AddDevice=/dev/kvm".to_string()));
        assert!(on.contains(&"Unmask=ALL".to_string()));
        let opt = lines("container:\n  kvm: optional\n");
        assert!(opt.contains(&"AddDevice=-/dev/kvm".to_string()));
        assert!(opt.contains(&"Unmask=ALL".to_string()));
        assert!(
            !lines("")
                .iter()
                .any(|l| l.contains("kvm") || l == "Unmask=ALL")
        );
    }

    #[test]
    fn tilde_becomes_percent_h_and_relative_resolves() {
        let l = lines(
            "mounts:\n  - host: \"~\"\n    container: /h\n  - host: ~/a/b\n    container: /a\n    mode: ro\n    relabel: false\n  - host: rel\n    container: /r\n  - host: /abs\n    optional: true\n",
        );
        let v: Vec<&String> = l
            .iter()
            .filter(|l| l.starts_with("Volume=/") || l.starts_with("Volume=%"))
            .collect();
        assert_eq!(
            v,
            [
                "Volume=%h/a/b:/a:ro",
                "Volume=/abs:/abs:z",
                "Volume=%h:/h:z",
                "Volume=/srv/checkout/rel:/r:z",
            ]
        );
    }

    #[test]
    fn percent_is_escaped_everywhere() {
        let l = lines(
            "container:\n  name: a\n  workdir: /w%d\n  env:\n    P: 50%\n    S: two words\nmounts:\n  - host: /x%y\n    container: /c%n\n",
        );
        assert!(l.contains(&"WorkingDir=/w%%d".to_string()));
        assert!(l.contains(&"Environment=P=50%%".to_string()));
        assert!(l.contains(&"Environment=\"S=two words\"".to_string()));
        assert!(l.contains(&"Volume=/x%%y:/c%%n:z".to_string()));
    }

    #[test]
    fn exec_quotes_words_systemd_would_split() {
        let l = lines("").join("\n");
        assert!(l.contains("Exec=x\n"));
        let text = "command: [sh, -c, 'echo \"a b\" \\ 100%']\nstate:\n  home: image\n";
        let out = export(text).unwrap();
        assert!(
            out.contains("Exec=sh -c \"echo \\\"a b\\\" \\\\ 100%%\"\n"),
            "{out}"
        );
    }

    #[test]
    fn dollar_and_control_characters_are_escaped() {
        let text = "command: [sh, -c, \"echo $HOME\\na\\tb\\x01\"]\ncontainer:\n  workdir: /w$x\n  env:\n    D: x$y\n    N: \"1\\n2\"\nmounts:\n  - host: /a$b\n    container: /c\nstate:\n  home: image\n";
        let out = export(text).unwrap();
        assert!(
            out.contains("Exec=sh -c \"echo $$HOME\\na\\tb\\x01\"\n"),
            "{out}"
        );
        assert!(out.contains("WorkingDir=/w$$x\n"), "{out}");
        assert!(out.contains("Environment=D=x$$y\n"), "{out}");
        assert!(out.contains("Environment=\"N=1\\n2\"\n"), "{out}");
        assert!(out.contains("Volume=/a$$b:/c:z\n"), "{out}");
        assert_eq!(out.lines().count(), out.matches('\n').count());
    }

    #[test]
    fn control_character_in_a_path_is_an_error() {
        let text = "command: [x]\ncontainer:\n  workdir: \"/w\\nx\"\nstate:\n  home: image\n";
        let err = export(text).unwrap_err().to_string();
        assert!(err.contains("WorkingDir="), "{err}");
    }

    #[test]
    fn secrets_mount_files_and_refuse_env() {
        let l =
            lines("secrets:\n  t:\n    source: file\n    path: ~/tok\n    mount: /run/secrets/t\n");
        assert_eq!(
            l.iter().rfind(|l| l.starts_with("Volume=")).unwrap(),
            "Volume=%h/tok:/run/secrets/t:ro"
        );
        let msg = export(&format!(
            "{BASE}secrets:\n  k:\n    source: env\n    var: K\n    env: [K]\n"
        ))
        .unwrap_err()
        .0;
        assert!(msg.contains("secret k passes its value in K"), "{msg}");
    }

    #[test]
    fn broker_secret_is_refused() {
        // Validation keeps a broker secret out of a command profile, so the
        // export's own refusal is tested on a profile built by hand.
        let e = env();
        let dir = TempDir::new("export-broker");
        let path = dir.0.join("p.yaml");
        std::fs::write(&path, BASE).unwrap();
        let mut p = profile::load(&path, "p", &e).unwrap();
        p.secrets.push(profile::Secret {
            name: "b".to_string(),
            source: profile::SecretSrc::Broker {
                url: "http://x".to_string(),
                token: PathBuf::from("/t"),
            },
            env: Vec::new(),
            harness_env: Default::default(),
            mount: None,
            required: true,
            tilde: false,
        });
        let msg = quadlet(&p, &e).unwrap_err().0;
        assert!(msg.contains("secret b has source broker"), "{msg}");
    }

    #[test]
    fn harness_and_state_dir_profiles_are_refused() {
        let msg = export("harness: claude\n").unwrap_err().0;
        assert_eq!(
            msg,
            "export quadlet: profile p starts a harness; only a profile with command and state.home: image can be exported"
        );
        let msg = export("command: [x]\n").unwrap_err().0;
        assert!(msg.contains("has a state directory"), "{msg}");
    }

    #[test]
    fn duplicate_destination_is_an_error() {
        let msg = export(&format!(
            "{BASE}mounts:\n  - host: /a\n    container: /m\n  - volume: v\n    container: /m\n"
        ))
        .unwrap_err()
        .0;
        assert!(msg.contains("/m is used twice"), "{msg}");
    }
}
