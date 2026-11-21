//! `bree export quadlet` through the built binary. The golden files in
//! `tests/fixtures/export/` are the `[Container]` sections bree should print
//! for the example crew profiles `inbox-watch` and `hourly-check`, with the
//! checkout at /home/alice/src/project and the `Volume=` lines in bree's order.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CHECKOUT: &str = "/home/alice/src/project";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Runs bree as the crew image's user, whose home is /home/alice, from a
/// directory that is not under it.
fn bree(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bree"))
        .arg("--root")
        .arg(root)
        .args(args)
        .current_dir(std::env::temp_dir())
        .env_clear()
        .env("HOME", "/home/alice")
        .env("USER", "alice")
        .output()
        .expect("run bree")
}

fn golden(unit: &str) {
    let root = fixtures().join("roots/crew");
    let out = bree(
        &root,
        &["--relative-to", CHECKOUT, "-p", unit, "export", "quadlet"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stderr, "");
    let expected =
        std::fs::read_to_string(fixtures().join(format!("export/{unit}.container"))).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
}

#[test]
fn inbox_watch_matches_the_unit() {
    golden("inbox-watch");
}

#[test]
fn hourly_check_matches_the_unit() {
    golden("hourly-check");
}

#[test]
fn a_profile_with_a_harness_is_refused() {
    let out = bree(
        &fixtures().join("roots/shared"),
        &["-p", "work", "export", "quadlet"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "bree: export quadlet: profile work starts a harness; only a profile with command and state.home: image can be exported\n"
    );
}

#[test]
fn relative_mounts_need_relative_to() {
    let out = bree(
        &fixtures().join("roots/crew"),
        &["-p", "inbox-watch", "export", "quadlet"],
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pass --relative-to DIR"), "{stderr}");
}

/// Runs `bree export quadlet` for a profile `p` written into a fresh temp
/// root, and returns the printed lines. The root is removed afterwards.
fn export_lines(tag: &str, yaml: &str) -> Vec<String> {
    let root = std::env::temp_dir().join(format!("bree-export-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(root.join("profiles/p.yaml"), yaml).unwrap();
    let out = bree(&root, &["-p", "p", "export", "quadlet"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert!(out.status.success(), "{stderr}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

const CREW: &str = "command: [x]\nstate:\n  home: image\n";

#[test]
fn label_disable_exports_the_option_without_the_storage_volume() {
    let lines = export_lines(
        "label",
        &format!("{CREW}container:\n  nested-podman: false\n  label-disable: true\n"),
    );
    assert!(
        lines.contains(&"SecurityLabelDisable=true".to_string()),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(".local/share/containers")),
        "no nested storage volume: {lines:?}"
    );
}

#[test]
fn label_disable_and_nested_podman_export_the_option_once() {
    let lines = export_lines(
        "label-both",
        &format!("{CREW}container:\n  nested-podman: true\n  label-disable: true\n"),
    );
    let count = lines
        .iter()
        .filter(|l| *l == "SecurityLabelDisable=true")
        .count();
    assert_eq!(count, 1, "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains(".local/share/containers")),
        "the nested storage volume is still there: {lines:?}"
    );
}
