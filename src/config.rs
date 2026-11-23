//! The user config (`config.yaml`), the process environment bree depends on,
//! and the resolution of config roots and profile files (docs/design.md,
//! "User config" and "Config roots and resolution").

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::yaml;

/// Everything bree reads from the process and the host, behind one struct so
/// unit tests inject it.
pub struct Env {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub user: String,
    pub vars: HashMap<String, String>,
    /// Whether `/etc/localtime` exists.
    pub localtime_exists: bool,
    pub path_exists: Box<dyn Fn(&Path) -> bool>,
    /// `--relative-to DIR`, absolute: what relative mount host paths resolve
    /// against. `None` makes them an error.
    pub relative_to: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Result<Env> {
        let vars: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        let home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::new("HOME is not set"))?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err(Error::new(format!(
                "HOME must be an absolute path, got {}",
                home.display()
            )));
        }
        let cwd = std::env::current_dir()
            .map_err(|e| Error::new(format!("cannot read the current directory: {e}")))?;
        let user = user_name(&vars)?;
        Ok(Env {
            home: normalize(&home),
            cwd: normalize(&cwd),
            user,
            vars,
            localtime_exists: Path::new("/etc/localtime").exists(),
            path_exists: Box::new(|p| p.exists()),
            relative_to: None,
        })
    }

    /// Sets `--relative-to`, resolving a relative `dir` against the current
    /// directory.
    pub fn with_relative_to(mut self, dir: Option<&Path>) -> Env {
        self.relative_to = dir.map(|d| normalize(&self.cwd.join(d)));
        self
    }

    pub fn exists(&self, path: &Path) -> bool {
        (self.path_exists)(path)
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }
}

/// `$USER`, else `$LOGNAME`, else the `/etc/passwd` entry of the uid that owns
/// `/proc/self`.
fn user_name(vars: &HashMap<String, String>) -> Result<String> {
    for key in ["USER", "LOGNAME"] {
        if let Some(v) = vars.get(key).filter(|v| !v.is_empty()) {
            return Ok(v.clone());
        }
    }
    let uid = std::fs::metadata("/proc/self")
        .map_err(|e| Error::new(format!("cannot find the user name: {e}")))?
        .uid();
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|e| Error::new(format!("cannot find the user name: {e}")))?;
    passwd
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let id: u32 = f.nth(1)?.parse().ok()?;
            (id == uid).then(|| name.to_string())
        })
        .next()
        .ok_or_else(|| {
            Error::new(format!(
                "cannot find the user name: no passwd entry for uid {uid}; set USER"
            ))
        })
}

/// Removes `.`, `..` and trailing slashes without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `~` and `~/rest` mean the home directory; anything else is unchanged.
pub fn expand_tilde(text: &str, home: &Path) -> PathBuf {
    if text == "~" {
        home.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(text)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct UserConfig {
    #[serde(default)]
    pub roots: Option<Vec<String>>,
    #[serde(default)]
    pub default_profile: Option<String>,
}

pub fn default_config_path(env: &Env) -> PathBuf {
    env.home.join(".config/bree/config.yaml")
}

/// Reads the user config. A missing default file is an empty config; a missing
/// file named with `--config` (`explicit`) is an error.
pub fn load_user_config(path: &Path, explicit: bool) -> Result<UserConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(yaml::from_str(&text, &path.display().to_string())?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            Ok(UserConfig::default())
        }
        Err(e) => Err(Error::new(format!(
            "cannot read config {}: {e}",
            path.display()
        ))),
    }
}

/// Root list precedence: `--root`, `BREE_ROOTS`, the config's `roots`, then
/// `~/.config/bree`. Every root must be an existing directory.
pub fn resolve_roots(
    root_args: &[PathBuf],
    config: &UserConfig,
    env: &Env,
) -> Result<Vec<PathBuf>> {
    let raw: Vec<PathBuf> = if !root_args.is_empty() {
        root_args
            .iter()
            .map(|p| expand_tilde(&p.to_string_lossy(), &env.home))
            .collect()
    } else if let Some(v) = env.var("BREE_ROOTS") {
        v.split(':')
            .filter(|s| !s.is_empty())
            .map(|s| expand_tilde(s, &env.home))
            .collect()
    } else if let Some(roots) = &config.roots {
        roots.iter().map(|s| expand_tilde(s, &env.home)).collect()
    } else {
        vec![env.home.join(".config/bree")]
    };
    let mut roots = Vec::new();
    for p in raw {
        let abs = if p.is_absolute() { p } else { env.cwd.join(p) };
        let abs = normalize(&abs);
        if !is_dir(&abs) {
            return Err(Error::new(format!(
                "config root {} is not a directory",
                abs.display()
            )));
        }
        roots.push(abs);
    }
    Ok(roots)
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// `-p`, else `default-profile` of the config, else an error.
pub fn profile_name(arg: Option<&str>, config: &UserConfig, config_path: &Path) -> Result<String> {
    let name = match (arg, &config.default_profile) {
        (Some(n), _) => n.to_string(),
        (None, Some(n)) => n.clone(),
        (None, None) => {
            return Err(Error::new(format!(
                "no profile: pass -p NAME or set default-profile in {}",
                config_path.display()
            )));
        }
    };
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(Error::new(format!(
            "invalid profile name `{name}`: use letters, digits, `.`, `_` and `-`, starting with a letter or digit"
        )));
    }
    Ok(name)
}

/// Finds `<root>/profiles/<name>.yaml` in exactly one root.
pub fn find_profile(roots: &[PathBuf], name: &str) -> Result<PathBuf> {
    let found: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.join("profiles").join(format!("{name}.yaml")))
        .filter(|p| p.is_file())
        .collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::new(format!(
            "profile `{name}` not found in the roots: {}",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        [a, b, ..] => Err(Error::new(format!(
            "profile `{name}` is defined twice: {} and {}",
            a.display(),
            b.display()
        ))),
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("bree-test-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An environment for tests: home /home/alice, cwd /home/alice/src/shared, user
    /// alice, `/etc/localtime` present, every path existing except the host's
    /// gws login, which a test adds when it wants the automatic mounts.
    pub fn env() -> Env {
        Env {
            home: PathBuf::from("/home/alice"),
            cwd: PathBuf::from("/home/alice/src/shared"),
            user: "alice".to_string(),
            vars: HashMap::new(),
            localtime_exists: true,
            path_exists: Box::new(|p| !p.starts_with("/home/alice/.config/gws")),
            relative_to: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{TempDir, env};
    use super::*;

    fn dirs(t: &TempDir, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| {
                let p = t.0.join(n);
                std::fs::create_dir_all(&p).unwrap();
                p
            })
            .collect()
    }

    #[test]
    fn root_args_beat_everything() {
        let t = TempDir::new("roots");
        let d = dirs(&t, &["a", "b", "c", "d"]);
        let mut e = env();
        e.vars
            .insert("BREE_ROOTS".into(), d[1].display().to_string());
        let cfg = UserConfig {
            roots: Some(vec![d[2].display().to_string()]),
            default_profile: None,
        };
        let got = resolve_roots(&[d[0].clone()], &cfg, &e).unwrap();
        assert_eq!(got, vec![d[0].clone()]);
    }

    #[test]
    fn bree_roots_beats_config_and_ignores_empty_entries() {
        let t = TempDir::new("roots");
        let d = dirs(&t, &["a", "b", "c"]);
        let mut e = env();
        e.vars.insert(
            "BREE_ROOTS".into(),
            format!(":{}::{}:", d[0].display(), d[1].display()),
        );
        let cfg = UserConfig {
            roots: Some(vec![d[2].display().to_string()]),
            default_profile: None,
        };
        let got = resolve_roots(&[], &cfg, &e).unwrap();
        assert_eq!(got, vec![d[0].clone(), d[1].clone()]);
    }

    #[test]
    fn config_roots_beat_the_default_and_expand_tilde() {
        let t = TempDir::new("roots");
        let d = dirs(&t, &["a"]);
        let mut e = env();
        e.home = t.0.clone();
        let cfg = UserConfig {
            roots: Some(vec!["~/a".to_string()]),
            default_profile: None,
        };
        let got = resolve_roots(&[], &cfg, &e).unwrap();
        assert_eq!(got, vec![d[0].clone()]);
    }

    #[test]
    fn default_root_is_the_config_dir_and_must_exist() {
        let t = TempDir::new("roots");
        let mut e = env();
        e.home = t.0.clone();
        let err = resolve_roots(&[], &UserConfig::default(), &e).unwrap_err();
        assert!(err.0.contains(".config/bree is not a directory"), "{err}");
        dirs(&t, &[".config/bree"]);
        let got = resolve_roots(&[], &UserConfig::default(), &e).unwrap();
        assert_eq!(got, vec![t.0.join(".config/bree")]);
    }

    #[test]
    fn relative_root_resolves_against_the_cwd() {
        let t = TempDir::new("roots");
        dirs(&t, &["rel/x"]);
        let mut e = env();
        e.cwd = t.0.join("rel");
        let got = resolve_roots(&[PathBuf::from("./x")], &UserConfig::default(), &e).unwrap();
        assert_eq!(got, vec![t.0.join("rel/x")]);
    }

    #[test]
    fn user_config_rejects_unknown_keys() {
        let t = TempDir::new("cfg");
        let f = t.0.join("config.yaml");
        std::fs::write(&f, "roots: [/a]\ndefault-profil: x\n").unwrap();
        let err = load_user_config(&f, true).unwrap_err();
        assert!(err.0.contains(":2: default-profil: unknown field"), "{err}");
    }

    #[test]
    fn missing_config_is_fine_unless_given_explicitly() {
        let t = TempDir::new("cfg");
        let f = t.0.join("none.yaml");
        assert!(load_user_config(&f, false).unwrap().roots.is_none());
        assert!(load_user_config(&f, true).is_err());
    }

    #[test]
    fn profile_name_comes_from_the_flag_then_the_config() {
        let cfg = UserConfig {
            roots: None,
            default_profile: Some("home".into()),
        };
        let p = Path::new("/c.yaml");
        assert_eq!(profile_name(Some("work"), &cfg, p).unwrap(), "work");
        assert_eq!(profile_name(None, &cfg, p).unwrap(), "home");
    }

    #[test]
    fn no_profile_name_is_an_error() {
        let err = profile_name(None, &UserConfig::default(), Path::new("/c.yaml")).unwrap_err();
        assert_eq!(
            err.0,
            "no profile: pass -p NAME or set default-profile in /c.yaml"
        );
    }

    #[test]
    fn bad_profile_names_are_rejected() {
        let cfg = UserConfig::default();
        for bad in ["", "-x", ".x", "a/b", "a b", "../x"] {
            assert!(
                profile_name(Some(bad), &cfg, Path::new("c")).is_err(),
                "{bad}"
            );
        }
        for good in ["work", "a.b_c-d", "0x"] {
            assert!(
                profile_name(Some(good), &cfg, Path::new("c")).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn profile_is_found_in_one_root() {
        let t = TempDir::new("prof");
        let d = dirs(&t, &["a/profiles", "b/profiles"]);
        std::fs::write(d[1].join("x.yaml"), "harness: claude\n").unwrap();
        let roots = vec![t.0.join("a"), t.0.join("b")];
        assert_eq!(find_profile(&roots, "x").unwrap(), d[1].join("x.yaml"));
    }

    #[test]
    fn profile_in_two_roots_is_an_error_naming_both() {
        let t = TempDir::new("prof");
        let d = dirs(&t, &["a/profiles", "b/profiles"]);
        std::fs::write(d[0].join("x.yaml"), "").unwrap();
        std::fs::write(d[1].join("x.yaml"), "").unwrap();
        let roots = vec![t.0.join("a"), t.0.join("b")];
        let err = find_profile(&roots, "x").unwrap_err();
        assert!(
            err.0.contains("a/profiles/x.yaml") && err.0.contains("b/profiles/x.yaml"),
            "{err}"
        );
    }

    #[test]
    fn missing_profile_lists_the_roots_searched() {
        let t = TempDir::new("prof");
        dirs(&t, &["a/profiles"]);
        let roots = vec![t.0.join("a")];
        let err = find_profile(&roots, "x").unwrap_err();
        assert!(
            err.0.starts_with("profile `x` not found in the roots: "),
            "{err}"
        );
        assert!(err.0.contains(&t.0.join("a").display().to_string()));
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize(Path::new("/a/./b/../c/")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
    }
}
