//! Runs the built binary: help, version and the "not implemented" stubs.

use std::process::{Command, Output};

fn bree(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bree"))
        .args(args)
        .output()
        .expect("run bree")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn help_lists_the_commands_of_the_design() {
    let out = bree(&["--help"]);
    assert!(out.status.success());
    let help = stdout(&out);
    for cmd in [
        "run",
        "render",
        "check",
        "capture",
        "profile",
        "tiers",
        "image",
        "doctor",
        "completions",
    ] {
        assert!(
            help.lines().any(|l| l.trim_start().starts_with(cmd)),
            "{cmd} missing from --help:\n{help}"
        );
    }
    // entrypoint is listed apart, under "Internal commands".
    let internal = help
        .split_once("Internal commands:")
        .expect("internal commands section")
        .1;
    assert!(internal.contains("entrypoint"), "{help}");
}

#[test]
fn subcommand_help_lists_its_commands() {
    let profile = stdout(&bree(&["profile", "--help"]));
    for cmd in ["list", "show", "contents"] {
        assert!(profile.contains(cmd), "{profile}");
    }
    let image = stdout(&bree(&["image", "--help"]));
    for cmd in ["build", "pull", "status"] {
        assert!(image.contains(cmd), "{image}");
    }
}

#[test]
fn version_has_package_version_and_commit() {
    let out = bree(&["--version"]);
    assert!(out.status.success());
    let v = stdout(&out);
    let prefix = format!("bree {} (", env!("CARGO_PKG_VERSION"));
    assert!(v.starts_with(&prefix) && v.trim_end().ends_with(')'), "{v}");
}

#[test]
fn every_command_says_not_implemented() {
    let cases: &[&[&str]] = &[
        &["capture", "--dry-run"],
        &["profile", "list"],
        &["profile", "show", "--json"],
        &["tiers"],
        &["image", "pull"],
        &["doctor"],
        &["completions", "bash"],
    ];
    for args in cases {
        let out = bree(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not implemented"), "{args:?}: {err}");
    }
}

#[test]
fn usage_error_exits_2() {
    let out = bree(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
}
