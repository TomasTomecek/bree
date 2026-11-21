//! `bree image build|status` and the build `bree run` does first (docs/design.md,
//! "bree image build|pull|status" and "Base image").

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::profile::{Image, Source};
use crate::render::hex;

/// The base image's build context, compiled in: `image/` without the binary.
const CONTAINERFILE: &str = include_str!("../image/Containerfile");
const FILES: [(&str, &str); 3] = [
    ("Containerfile", CONTAINERFILE),
    (
        "containers/containers.conf",
        include_str!("../image/containers/containers.conf"),
    ),
    (
        "containers/storage.conf",
        include_str!("../image/containers/storage.conf"),
    ),
];

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LABEL_VERSION: &str = "org.opencontainers.image.version";
const LABEL_SHA: &str = "bree.containerfile-sha256";
const COMMIT: &str = env!("BREE_COMMIT");
const LABEL_EXE: &str = "bree.exe-sha256";
const LABEL_COMMIT: &str = "bree.commit";

/// The labels that say which bree and Containerfile built an image. The
/// version and the commit are for people; the executable's hash is what
/// catches a new build that kept both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub version: Option<String>,
    pub sha256: Option<String>,
    pub exe_sha256: Option<String>,
    pub commit: Option<String>,
}

/// The labels of an image built by this binary from an executable whose
/// SHA-256 is `exe_sha256`.
fn labels_for(exe_sha256: String) -> Labels {
    Labels {
        version: Some(VERSION.to_string()),
        sha256: Some(hex(&Sha256::digest(CONTAINERFILE.as_bytes()))),
        exe_sha256: Some(exe_sha256),
        commit: Some(COMMIT.to_string()),
    }
}

/// The running executable. `/proc/self/exe` opens the file this process runs
/// even after `cargo install` or an RPM update replaced the path, so the hash
/// and the copy in the image are of the bree that is running.
fn exe_path() -> Result<PathBuf> {
    let proc = PathBuf::from("/proc/self/exe");
    if proc.exists() {
        return Ok(proc);
    }
    std::env::current_exe()
        .map_err(|e| Error::new(format!("image: cannot find the bree executable: {e}")))
}

fn file_sha256(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::new(format!("image: cannot read {}: {e}", path.display())))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

/// What this binary would build.
pub fn wanted_labels() -> Result<Labels> {
    Ok(labels_for(file_sha256(&exe_path()?)?))
}

/// Reads the labels from `podman image inspect --format '{{json .Labels}}'`.
fn parse_labels(json: &str) -> Labels {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let get = |k: &str| v.get(k).and_then(|s| s.as_str()).map(str::to_string);
    Labels {
        version: get(LABEL_VERSION),
        sha256: get(LABEL_SHA),
        exe_sha256: get(LABEL_EXE),
        commit: get(LABEL_COMMIT),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Missing,
    /// Present, with labels that differ from this binary's.
    Stale,
    Current,
}

pub fn state(local: Option<&Labels>, wanted: &Labels) -> State {
    match local {
        None => State::Missing,
        Some(l) if l == wanted => State::Current,
        Some(_) => State::Stale,
    }
}

/// Whether `bree run` builds before it starts: only for `source: build`, when
/// the image is missing or its labels differ, and never with `--no-pull`.
/// A `pull` image that is missing cannot be fetched yet. A `local` image is
/// used as it is, whatever its labels.
pub fn needs_build(image: &Image, state: &State, no_pull: bool) -> Result<bool> {
    if no_pull || *state == State::Current || image.source == Source::Local {
        return Ok(false);
    }
    match image.source {
        Source::Local => Ok(false),
        Source::Build => Ok(true),
        Source::Pull if *state == State::Missing => Err(Error::new(
            "image pull: not implemented; build the image or run with image.source: build",
        )),
        Source::Pull => Ok(false),
    }
}

/// Writes the embedded files and a copy of `exe` as `bree` into `dir`.
fn write_context(dir: &Path, exe: &Path) -> Result<()> {
    let fail = |what: &str, e: std::io::Error| Error::new(format!("image build: {what}: {e}"));
    for (name, text) in FILES {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap_or(dir)).map_err(|e| fail(name, e))?;
        std::fs::write(&path, text).map_err(|e| fail(name, e))?;
    }
    std::fs::copy(exe, dir.join("bree")).map_err(|e| fail("copy the bree executable", e))?;
    Ok(())
}

/// `--no-cache` also pulls the base again: it is for new Fedora packages.
fn build_args(reference: &str, no_cache: bool, ctx: &Path, l: Labels) -> Vec<String> {
    let mut a = vec!["build".to_string()];
    if no_cache {
        a.extend(["--no-cache".to_string(), "--pull=always".to_string()]);
    }
    for (k, v) in [
        (LABEL_VERSION, l.version),
        (LABEL_SHA, l.sha256),
        (LABEL_EXE, l.exe_sha256),
        (LABEL_COMMIT, l.commit),
    ] {
        a.push("--label".to_string());
        a.push(format!("{k}={}", v.unwrap_or_default()));
    }
    a.extend(["-t".to_string(), reference.to_string()]);
    a.push(ctx.display().to_string());
    a
}

/// A new, empty build context directory under `base`, mode 0700. It is
/// created exclusively, so a path another user made first (a directory or a
/// symlink) is skipped, never reused: nobody else can swap the Containerfile
/// or the binary before podman reads them.
fn context_dir(base: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let pid = std::process::id();
    let mut last = None;
    for n in 0..100 {
        let dir = base.join(format!("bree-image-{pid}-{n}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => {
                return Err(Error::new(format!("image build: {}: {e}", dir.display())));
            }
        }
    }
    Err(Error::new(format!(
        "image build: no free build directory under {}: {}",
        base.display(),
        last.map_or_else(String::new, |e| e.to_string())
    )))
}

/// `bree image build`: build `reference` from the embedded Containerfile.
/// The executable's label is the hash of the copy in the context, so it
/// names exactly the `bree` that goes into the image.
pub fn build(reference: &str, no_cache: bool) -> Result<()> {
    let exe = exe_path()?;
    let ctx = context_dir(&std::env::temp_dir())?;
    let status = write_context(&ctx, &exe).and_then(|()| {
        let labels = labels_for(file_sha256(&ctx.join("bree"))?);
        eprintln!("bree: building {reference}");
        Command::new("podman")
            .args(build_args(reference, no_cache, &ctx, labels))
            .status()
            .map_err(|e| Error::new(format!("image build: cannot run podman: {e}")))
    });
    let _ = std::fs::remove_dir_all(&ctx);
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!("image build: podman {status}")))
    }
}

/// The labels of a local image, or `None` when it is not there.
pub fn local_labels(reference: &str) -> Result<Option<Labels>> {
    let out = Command::new("podman")
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .Labels}}",
            reference,
        ])
        .output()
        .map_err(|e| Error::new(format!("cannot run podman: {e}")))?;
    Ok(out
        .status
        .success()
        .then(|| parse_labels(&String::from_utf8_lossy(&out.stdout))))
}

/// `bree run`: build the image first when it is missing or its labels differ.
pub fn ensure(image: &Image, no_pull: bool) -> Result<()> {
    if no_pull || image.source == Source::Local {
        return Ok(());
    }
    let st = state(local_labels(&image.reference)?.as_ref(), &wanted_labels()?);
    if needs_build(image, &st, no_pull)? {
        build(&image.reference, false)?;
    }
    Ok(())
}

fn show(l: &Labels) -> String {
    let short = |s: &Option<String>| {
        s.as_deref()
            .map_or("-", |s| &s[..s.len().min(12)])
            .to_string()
    };
    format!(
        "version {}  containerfile {}  exe {}  commit {}",
        l.version.as_deref().unwrap_or("-"),
        short(&l.sha256),
        short(&l.exe_sha256),
        l.commit.as_deref().unwrap_or("-")
    )
}

/// The text of `bree image status`.
pub fn status_text(image: &Image, local: Option<&Labels>, wanted: &Labels) -> String {
    let source = match image.source {
        Source::Build => "build",
        Source::Pull => "pull",
        Source::Local => "local",
    };
    let st = state(local, wanted);
    let mut out = format!("image   {}  ({source})\n", image.reference);
    match local {
        None => out.push_str("local   missing\n"),
        Some(l) => out.push_str(&format!("local   {}\n", show(l))),
    }
    out.push_str(&format!("bree    {}\n", show(wanted)));
    out.push_str(match st {
        State::Missing => "labels  no image\n",
        State::Stale => "labels  differ\n",
        State::Current => "labels  match\n",
    });
    out
}

/// `bree image status`: exit 0 when the image is there and its labels match.
pub fn status(image: &Image) -> Result<ExitCode> {
    let local = local_labels(&image.reference)?;
    let wanted = wanted_labels()?;
    print!("{}", status_text(image, local.as_ref(), &wanted));
    Ok(if state(local.as_ref(), &wanted) == State::Current {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::TempDir;

    fn image(source: Source) -> Image {
        Image {
            reference: "localhost/bree-base:0.1.0".to_string(),
            source,
        }
    }

    #[test]
    fn sha256_label_is_the_containerfile_hash() {
        let l = labels_for("e".repeat(64));
        assert_eq!(l.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(l.commit.as_deref(), Some(env!("BREE_COMMIT")));
        assert_eq!(l.exe_sha256, Some("e".repeat(64)));
        let sha = l.sha256.unwrap();
        assert_eq!(sha.len(), 64);
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha, hex(&Sha256::digest(CONTAINERFILE.as_bytes())));
    }

    #[test]
    fn build_context_is_image_dir_plus_the_executable() {
        let t = TempDir::new("image-ctx");
        let exe = t.0.join("fake-bree");
        std::fs::write(&exe, "binary").unwrap();
        let ctx = t.0.join("ctx");
        write_context(&ctx, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(ctx.join("bree")).unwrap(), "binary");
        for (name, text) in FILES {
            assert_eq!(std::fs::read_to_string(ctx.join(name)).unwrap(), text);
        }
        let on_disk =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/image/Containerfile"));
        assert_eq!(on_disk.unwrap(), CONTAINERFILE);
        assert!(CONTAINERFILE.contains("COPY --chmod=0755 bree /usr/local/bin/bree"));
        assert_eq!(
            file_sha256(&ctx.join("bree")).unwrap(),
            hex(&Sha256::digest(b"binary"))
        );
    }

    /// The label `bree run` compares is the running executable's hash, and
    /// a new build of the same version and commit gets another one (#40).
    #[test]
    fn exe_label_is_the_running_executable() {
        let w = wanted_labels().unwrap();
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(w.exe_sha256, Some(hex(&Sha256::digest(&exe))));
        let rebuilt = Labels {
            exe_sha256: Some("0".repeat(64)),
            ..w.clone()
        };
        assert_eq!(state(Some(&rebuilt), &w), State::Stale);
    }

    #[test]
    fn context_dir_is_new_private_and_skips_planted_paths() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("image-ctxdir");
        let pid = std::process::id();
        let planted_dir = t.0.join(format!("bree-image-{pid}-0"));
        std::fs::create_dir(&planted_dir).unwrap();
        std::fs::write(planted_dir.join("Containerfile"), "FROM evil").unwrap();
        let target = t.0.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, t.0.join(format!("bree-image-{pid}-1"))).unwrap();

        let ctx = context_dir(&t.0).unwrap();
        assert_eq!(ctx, t.0.join(format!("bree-image-{pid}-2")));
        let meta = std::fs::symlink_metadata(&ctx).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&ctx).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(planted_dir.join("Containerfile")).unwrap(),
            "FROM evil"
        );
    }

    #[test]
    fn build_args_carry_all_labels() {
        let l = labels_for("e".repeat(64));
        let args = build_args(
            "localhost/bree-base:0.1.0",
            false,
            Path::new("/ctx"),
            l.clone(),
        );
        assert_eq!(
            args,
            [
                "build",
                "--label",
                &format!("{LABEL_VERSION}={}", l.version.unwrap()),
                "--label",
                &format!("{LABEL_SHA}={}", l.sha256.unwrap()),
                "--label",
                &format!("{LABEL_EXE}={}", "e".repeat(64)),
                "--label",
                &format!("{LABEL_COMMIT}={}", env!("BREE_COMMIT")),
                "-t",
                "localhost/bree-base:0.1.0",
                "/ctx"
            ]
        );
        let args = build_args("x", true, Path::new("/ctx"), labels_for(String::new()));
        assert_eq!(args[..3], ["build", "--no-cache", "--pull=always"]);
    }

    #[test]
    fn labels_come_from_podman_json() {
        let w = labels_for("e".repeat(64));
        let json = format!(
            r#"{{"{LABEL_VERSION}":"{}","{LABEL_SHA}":"{}","{LABEL_EXE}":"{}","{LABEL_COMMIT}":"{}","other":"x"}}"#,
            w.version.as_ref().unwrap(),
            w.sha256.as_ref().unwrap(),
            w.exe_sha256.as_ref().unwrap(),
            w.commit.as_ref().unwrap()
        );
        assert_eq!(state(Some(&parse_labels(&json)), &w), State::Current);
        assert_eq!(
            parse_labels("null"),
            Labels {
                version: None,
                sha256: None,
                exe_sha256: None,
                commit: None
            }
        );
        assert_eq!(state(Some(&parse_labels("null")), &w), State::Stale);
        // An image from before the executable labels: same version and
        // Containerfile, so only the missing labels make it stale (#40).
        let before = Labels {
            exe_sha256: None,
            commit: None,
            ..w.clone()
        };
        assert_eq!(state(Some(&before), &w), State::Stale);
        let old = Labels {
            version: Some("0.0.1".into()),
            ..w.clone()
        };
        assert_eq!(state(Some(&old), &w), State::Stale);
        assert_eq!(state(None, &w), State::Missing);
    }

    #[test]
    fn the_decision_to_build() {
        let build = image(Source::Build);
        let pull = image(Source::Pull);
        for (st, want) in [
            (State::Missing, true),
            (State::Stale, true),
            (State::Current, false),
        ] {
            assert_eq!(needs_build(&build, &st, false), Ok(want));
            assert_eq!(needs_build(&build, &st, true), Ok(false));
        }
        assert!(needs_build(&pull, &State::Missing, false).is_err());
        assert_eq!(needs_build(&pull, &State::Missing, true), Ok(false));
        assert_eq!(needs_build(&pull, &State::Stale, false), Ok(false));
        let local = image(Source::Local);
        for st in [State::Missing, State::Stale, State::Current] {
            assert_eq!(needs_build(&local, &st, false), Ok(false));
        }
    }

    #[test]
    fn status_text_says_what_differs() {
        let img = image(Source::Build);
        let w = labels_for("1a2b3c4d5e6f7a8b".to_string());
        let text = status_text(&img, None, &w);
        assert!(text.starts_with("image   localhost/bree-base:0.1.0  (build)\nlocal   missing\n"));
        let old = Labels {
            version: Some("0.0.1".into()),
            sha256: None,
            exe_sha256: None,
            commit: None,
        };
        let text = status_text(&img, Some(&old), &w);
        assert!(
            text.contains("local   version 0.0.1  containerfile -  exe -  commit -\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "  exe 1a2b3c4d5e6f  commit {}\n",
                env!("BREE_COMMIT")
            )),
            "{text}"
        );
        assert!(text.ends_with("labels  differ\n"));
        let text = status_text(&img, Some(&w), &w);
        assert!(text.ends_with("labels  match\n"), "{text}");
    }
}
