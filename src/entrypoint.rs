//! `bree entrypoint`: the base image's entry point (docs/design.md, "The
//! container" and "Container target"). It renders the profile named by
//! `BREE_PROFILE` from the roots `bree run` mounts under `/run/bree/roots/`
//! into the container home, then replaces itself with the command.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::render::io_err;

/// Where `bree run` mounts the config roots, as `<n>` for the n-th root.
pub const ROOTS_DIR: &str = "/run/bree/roots";

/// The config roots under `dir`: every entry named by a number, in numeric
/// order, so root 10 comes after root 9. Anything else there is an error,
/// since only `bree run` puts roots there.
pub fn mounted_roots(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        Error::new(format!(
            "cannot read the config roots in {}: {e} (bree run mounts them there)",
            dir.display()
        ))
    })?;
    let mut roots: Vec<(u32, PathBuf)> = Vec::new();
    for e in entries {
        let e = e.map_err(io_err(dir))?;
        let path = e.path();
        let n = e
            .file_name()
            .to_str()
            .filter(|s| !s.starts_with('0') || s.len() == 1)
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| {
                Error::new(format!(
                    "{} is not a config root: names under {} are numbers",
                    path.display(),
                    dir.display()
                ))
            })?;
        if !path.is_dir() {
            return Err(Error::new(format!(
                "config root {} is not a directory",
                path.display()
            )));
        }
        roots.push((n, path));
    }
    if roots.is_empty() {
        return Err(Error::new(format!(
            "no config roots in {} (bree run mounts them there)",
            dir.display()
        )));
    }
    roots.sort();
    Ok(roots.into_iter().map(|(_, p)| p).collect())
}

/// The profile: `-p`, else `BREE_PROFILE`, which `bree run` sets.
pub fn profile_name(arg: Option<&str>, env_value: Option<&str>) -> Result<String> {
    arg.or(env_value.filter(|v| !v.is_empty()))
        .map(str::to_string)
        .ok_or_else(|| Error::new("entrypoint: BREE_PROFILE is not set (bree run sets it)"))
}

/// Replaces this process with `command`, with `CLAUDE_CONFIG_DIR` set to
/// `~/.claude` unless the environment already sets it (design, "What the
/// renderer writes"). Only returns on failure.
pub fn exec(command: &[String], home: &Path, claude_config_dir: Option<&str>) -> Error {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    if claude_config_dir.is_none_or(str::is_empty) {
        cmd.env("CLAUDE_CONFIG_DIR", home.join(".claude"));
    }
    let err = cmd.exec();
    Error::new(format!("entrypoint: cannot run {}: {err}", command[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::TempDir;

    #[test]
    fn roots_are_in_numeric_order() {
        let t = TempDir::new("entrypoint-roots");
        for n in ["0", "1", "10", "2"] {
            std::fs::create_dir(t.0.join(n)).unwrap();
        }
        let names: Vec<String> = mounted_roots(&t.0)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["0", "1", "2", "10"]);
    }

    #[test]
    fn a_name_that_is_not_a_number_is_an_error() {
        let t = TempDir::new("entrypoint-name");
        std::fs::create_dir(t.0.join("0")).unwrap();
        std::fs::create_dir(t.0.join("shared")).unwrap();
        let e = mounted_roots(&t.0).unwrap_err();
        assert!(e.0.contains("shared is not a config root"), "{e}");
        std::fs::remove_dir(t.0.join("shared")).unwrap();
        std::fs::create_dir(t.0.join("01")).unwrap();
        assert!(mounted_roots(&t.0).is_err());
    }

    #[test]
    fn no_roots_is_an_error() {
        let t = TempDir::new("entrypoint-empty");
        let e = mounted_roots(&t.0).unwrap_err();
        assert!(e.0.contains("no config roots"), "{e}");
        let e = mounted_roots(&t.0.join("missing")).unwrap_err();
        assert!(e.0.contains("cannot read the config roots"), "{e}");
    }

    #[test]
    fn a_file_is_not_a_root() {
        let t = TempDir::new("entrypoint-file");
        std::fs::write(t.0.join("0"), "").unwrap();
        let e = mounted_roots(&t.0).unwrap_err();
        assert!(e.0.contains("is not a directory"), "{e}");
    }

    #[test]
    fn profile_comes_from_the_flag_then_the_environment() {
        assert_eq!(profile_name(Some("work"), Some("home")).unwrap(), "work");
        assert_eq!(profile_name(None, Some("home")).unwrap(), "home");
        assert!(profile_name(None, Some("")).is_err());
        let e = profile_name(None, None).unwrap_err();
        assert!(e.0.contains("BREE_PROFILE is not set"), "{e}");
    }
}
