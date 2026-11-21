//! `bree run --dry-run` and profile errors through the built binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/roots")
        .join(name)
}

/// A temp HOME with `src/proj` inside, removed on drop.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("bree-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home/src/proj")).unwrap();
        Home(dir)
    }
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }
    fn cwd(&self) -> PathBuf {
        self.0.join("home/src/proj")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bree(h: &Home, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bree"))
        .args(args)
        .current_dir(h.cwd())
        .env_clear()
        .env("HOME", h.home())
        .env("USER", "alice")
        .env("CORP_KEY", "do-not-print-me")
        .output()
        .expect("run bree")
}

#[test]
fn work_dry_run_prints_the_command_and_no_secret() {
    let h = Home::new("work");
    let root = fixtures("shared");
    let out = bree(
        &h,
        &[
            "--root",
            root.to_str().unwrap(),
            "-p",
            "work",
            "run",
            "--dry-run",
            "claude",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "podman run --rm -it --init");
    assert!(
        lines[1].starts_with("  --name bree-work-proj-"),
        "{}",
        lines[1]
    );
    assert!(lines.contains(&"  -e BREE_PROFILE=work"));
    assert!(lines.contains(&"  -e CORP_KEY"));
    assert!(lines.contains(&format!("  -v {}:/run/bree/roots/0:ro", root.display()).as_str()));
    assert!(lines.contains(&format!("  -w {}", h.cwd().display()).as_str()));
    assert_eq!(*lines.last().unwrap(), "  bree entrypoint -- claude");
    assert!(!stdout.contains("do-not-print-me"));
    assert!(!stderr.contains("do-not-print-me"));
    assert!(
        !h.home().join(".local/share/bree").exists(),
        "dry run creates nothing"
    );
}

#[test]
fn harness_command_replaces_the_started_argv() {
    let h = Home::new("harness-command");
    let root = h.home().join("root");
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(
        root.join("profiles/p.yaml"),
        "harness: opencode\nharness-command:\n  opencode: [opencode, mini]\n",
    )
    .unwrap();
    let out = bree(
        &h,
        &[
            "--root",
            root.to_str().unwrap(),
            "-p",
            "p",
            "run",
            "--dry-run",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout.lines().last().unwrap(),
        "  bree entrypoint -- opencode mini"
    );
}

#[test]
fn unknown_key_exits_2_with_file_and_line() {
    let h = Home::new("typo");
    let root = fixtures("invalid");
    let out = bree(
        &h,
        &[
            "--root",
            root.to_str().unwrap(),
            "-p",
            "typo",
            "run",
            "--dry-run",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let file = root.join("profiles/typo.yaml");
    assert!(
        stderr.starts_with(&format!(
            "bree: {}:3: container.kvmm: unknown field `kvmm`",
            file.display()
        )),
        "{stderr}"
    );
}

#[test]
fn missing_profile_name_exits_2() {
    let h = Home::new("noprofile");
    let root = fixtures("shared");
    let out = bree(&h, &["--root", root.to_str().unwrap(), "run", "--dry-run"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.starts_with("bree: no profile: pass -p NAME"),
        "{stderr}"
    );
}

// Crew profiles (docs/design.md, "Crew profiles and quadlets"): one test per
// key that section adds, each showing the podman argv the key produces.

/// Writes `yaml` as profile `p` in a config root under `h` and runs
/// `bree run --dry-run` with `args` after it. Returns (exit code, stdout,
/// stderr).
fn dry_run(h: &Home, yaml: &str, args: &[&str]) -> (Option<i32>, String, String) {
    let root = h.0.join("root");
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    std::fs::write(root.join("profiles/p.yaml"), yaml).unwrap();
    let mut all = vec![
        "--root",
        root.to_str().unwrap(),
        "-p",
        "p",
        "run",
        "--dry-run",
    ];
    all.extend(args);
    let out = bree(h, &all);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The dry-run lines of a profile that must succeed.
fn argv(h: &Home, yaml: &str) -> Vec<String> {
    let (code, stdout, stderr) = dry_run(h, yaml, &[]);
    assert_eq!(code, Some(0), "{stderr}");
    stdout.lines().map(str::to_string).collect()
}

fn has(lines: &[String], line: &str) -> bool {
    lines.iter().any(|l| l == line)
}

const HARNESS: &str = "harness: claude\ncontainer:\n  nested-podman: false\n";

#[test]
fn crew_profile_dry_run() {
    let h = Home::new("crew");
    let root = h.0.join("root");
    let yaml = "\
command: [crew-agent, code-review]
image:
  ref: localhost/crew:latest
  source: local
state:
  home: image
container:
  name: crew-code-review
  network: host
  tz: UTC
  workdir: /home/agent/code
  storage-volume: crew-code-review-storage
";
    let expected = format!(
        "\
podman run --rm -it --init
  --name crew-code-review
  --userns=keep-id:uid=1000,gid=1000
  --passwd-entry=alice:*:1000:1000::{home}:/bin/bash
  --security-opt=label=disable
  --tz=UTC
  --network=host
  --pull=never
  -e TERM
  -e BREE_PROFILE=p
  -v {root}:/run/bree/roots/0:ro
  -v crew-code-review-storage:{home}/.local/share/containers
  -w /home/agent/code
  localhost/crew:latest
  crew-agent code-review
",
        home = h.home().display(),
        root = root.display()
    );
    let (code, stdout, stderr) = dry_run(&h, yaml, &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, expected);
}

#[test]
fn image_source_local_never_pulls() {
    let h = Home::new("local");
    let lines = argv(
        &h,
        &format!("{HARNESS}image:\n  ref: localhost/crew:latest\n  source: local\n"),
    );
    assert!(has(&lines, "  --pull=never"), "{lines:?}");
    assert!(has(&lines, "  localhost/crew:latest"));
    // The default source builds, and podman may pull.
    let lines = argv(&h, HARNESS);
    assert!(!lines.iter().any(|l| l.contains("--pull")), "{lines:?}");
}

#[test]
fn command_replaces_the_entry_point() {
    let h = Home::new("command");
    let yaml = "command: [crew-agent, --once]\ncontainer:\n  nested-podman: false\n";
    let lines = argv(&h, yaml);
    assert_eq!(lines.last().unwrap(), "  crew-agent --once");
    assert!(!lines.iter().any(|l| l.contains("entrypoint")), "{lines:?}");
    // ARGS after -- go to the command; a harness argument is refused.
    let (code, stdout, stderr) = dry_run(&h, yaml, &["--", "extra"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout.lines().last().unwrap(), "  crew-agent --once extra");
    let (code, _, stderr) = dry_run(&h, yaml, &["claude"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.starts_with("bree: profile p runs its command and starts no harness"),
        "{stderr}"
    );
}

#[test]
fn command_with_a_harness_is_refused() {
    let h = Home::new("command-harness");
    let (code, stdout, stderr) = dry_run(&h, "harness: claude\ncommand: [x]\n", &[]);
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("command: command and harness exclude each other"),
        "{stderr}"
    );
}

#[test]
fn state_home_image_mounts_no_state_directory() {
    let h = Home::new("image-home");
    let state = h.home().join(".local/share/bree/state/p");
    let lines = argv(&h, "command: [x]\nstate:\n  home: image\n");
    let to_home = format!(":{}", h.home().display());
    assert!(
        !lines.iter().any(|l| l.ends_with(&to_home)),
        "nothing is mounted as the home: {lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains(".local/share/bree/state")));
    // The default does mount it.
    let lines = argv(&h, "command: [x]\n");
    assert!(has(
        &lines,
        &format!("  -v {}:{}", state.display(), h.home().display())
    ));
}

#[test]
fn state_home_image_with_rendered_config_is_refused() {
    let h = Home::new("image-home-render");
    for yaml in [
        "harness: claude\nstate:\n  home: image\n",
        "command: [x]\nrules: [core]\nstate:\n  home: image\n",
    ] {
        let (code, stdout, stderr) = dry_run(&h, yaml, &[]);
        assert_eq!(code, Some(2), "{yaml}");
        assert!(stdout.is_empty());
        assert!(
            stderr.contains(
                "state.home: state.home: image keeps the image's home, so the profile renders nothing"
            ),
            "{stderr}"
        );
    }
}

#[test]
fn container_name_is_fixed() {
    let h = Home::new("name");
    let lines = argv(&h, &format!("{HARNESS}  name: crew-x\n"));
    assert_eq!(lines[1], "  --name crew-x");
    let lines = argv(&h, HARNESS);
    assert!(
        lines[1].starts_with("  --name bree-p-proj-"),
        "{}",
        lines[1]
    );
}

#[test]
fn container_workdir_path_does_not_use_the_cwd() {
    let h = Home::new("workdir");
    let lines = argv(&h, &format!("{HARNESS}  workdir: /srv/work\n"));
    assert!(has(&lines, "  -w /srv/work"), "{lines:?}");
    let cwd = h.cwd().display().to_string();
    assert!(
        !lines.iter().any(|l| l.contains(&cwd)),
        "the cwd is neither used nor mounted: {lines:?}"
    );
    assert!(
        lines[1].starts_with("  --name bree-p-work-"),
        "{}",
        lines[1]
    );
}

#[test]
fn container_kvm_optional_follows_the_host() {
    let h = Home::new("kvm");
    let lines = argv(&h, &format!("{HARNESS}  kvm: optional\n"));
    let present = Path::new("/dev/kvm").exists();
    assert_eq!(has(&lines, "  --device=/dev/kvm"), present, "{lines:?}");
    assert_eq!(has(&lines, "  --security-opt=unmask=ALL"), present);
    let lines = argv(&h, &format!("{HARNESS}  kvm: true\n"));
    assert!(has(&lines, "  --device=/dev/kvm"));
}

#[test]
fn container_storage_volume_is_named() {
    let h = Home::new("storage");
    let home = h.home().display().to_string();
    let lines = argv(
        &h,
        "harness: claude\ncontainer:\n  storage-volume: crew-x\n",
    );
    assert!(
        has(
            &lines,
            &format!("  -v crew-x:{home}/.local/share/containers")
        ),
        "{lines:?}"
    );
    let lines = argv(&h, "harness: claude\n");
    assert!(has(
        &lines,
        &format!("  -v bree-p-containers:{home}/.local/share/containers")
    ));
}

#[test]
fn label_disable_gives_the_option_without_the_storage_volume() {
    let h = Home::new("label");
    let lines = argv(&h, &format!("{HARNESS}  label-disable: true\n"));
    assert!(has(&lines, "  --security-opt=label=disable"), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains(".local/share/containers")),
        "no nested storage volume: {lines:?}"
    );
}

#[test]
fn label_disable_and_nested_podman_pass_the_option_once() {
    let h = Home::new("label-both");
    let lines = argv(
        &h,
        "harness: claude\ncontainer:\n  nested-podman: true\n  label-disable: true\n",
    );
    let count = lines
        .iter()
        .filter(|l| l == &"  --security-opt=label=disable")
        .count();
    assert_eq!(count, 1, "{lines:?}");
    let home = h.home().display().to_string();
    assert!(
        has(
            &lines,
            &format!("  -v bree-p-containers:{home}/.local/share/containers")
        ),
        "the nested storage volume is still there: {lines:?}"
    );
}

#[test]
fn label_disable_rejects_a_non_boolean() {
    let h = Home::new("label-bad");
    let root = h.0.join("root");
    let (code, stdout, stderr) = dry_run(
        &h,
        "harness: claude\ncontainer:\n  label-disable: yes\n",
        &[],
    );
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    let file = root.join("profiles/p.yaml");
    assert!(
        stderr.starts_with(&format!(
            "bree: {}:3: container.label-disable: expected true or false, found `yes`",
            file.display()
        )),
        "{stderr}"
    );
}

// Mount keys: one test per mount kind, each showing the
// podman argv it produces.

#[test]
fn host_mount_is_relabelled_by_default() {
    let h = Home::new("relabel");
    let dir = h.0.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let lines = argv(&h, &format!("{HARNESS}mounts:\n  - host: {d}\n"));
    assert!(has(&lines, &format!("  -v {d}:{d}:z")), "{lines:?}");
    let lines = argv(
        &h,
        &format!("{HARNESS}mounts:\n  - host: {d}\n    relabel: false\n"),
    );
    assert!(has(&lines, &format!("  -v {d}:{d}")), "{lines:?}");
    // The working directory and the config roots keep their labels too.
    let cwd = h.cwd().display().to_string();
    assert!(has(&lines, &format!("  -v {cwd}:{cwd}")), "{lines:?}");
}

#[test]
fn read_only_directory_mount_has_no_mode_check() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new("ro-dir");
    let dir = h.0.join("ssh");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config"), "Host *\n").unwrap();
    // Readable by group and others, which a file secret refuses.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(dir.join("config"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let d = dir.display();
    let lines = argv(
        &h,
        &format!("{HARNESS}mounts:\n  - host: {d}\n    container: /run/ssh\n    mode: ro\n"),
    );
    assert!(has(&lines, &format!("  -v {d}:/run/ssh:ro,z")), "{lines:?}");
}

#[test]
fn volume_mount_is_named_and_not_relabelled() {
    let h = Home::new("volume");
    let lines = argv(
        &h,
        &format!("{HARNESS}mounts:\n  - volume: crew-data\n    container: /data\n    mode: ro\n"),
    );
    assert!(has(&lines, "  -v crew-data:/data:ro"), "{lines:?}");
    let (code, _, stderr) = dry_run(
        &h,
        &format!("{HARNESS}mounts:\n  - volume: crew-data\n"),
        &[],
    );
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("mounts[0]: a volume needs container"),
        "{stderr}"
    );
}

#[test]
fn relative_mount_resolves_against_relative_to() {
    let h = Home::new("relative");
    let checkout = h.0.join("checkout");
    std::fs::create_dir_all(checkout.join("secrets/ssh")).unwrap();
    let yaml =
        format!("{HARNESS}mounts:\n  - host: secrets/ssh\n    container: /run/ssh\n    mode: ro\n");
    let (code, stdout, stderr) = dry_run(&h, &yaml, &["--relative-to", checkout.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stderr}");
    let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
    assert!(
        has(
            &lines,
            &format!("  -v {}/secrets/ssh:/run/ssh:ro,z", checkout.display())
        ),
        "{lines:?}"
    );
    // The entry point loads the profile again, so it gets the option too.
    assert_eq!(
        *lines.last().unwrap(),
        format!(
            "  bree --relative-to {} entrypoint -- claude",
            checkout.display()
        )
    );
}

#[test]
fn relative_mount_without_relative_to_is_an_error() {
    let h = Home::new("relative-missing");
    let (code, stdout, stderr) = dry_run(
        &h,
        &format!("{HARNESS}mounts:\n  - host: secrets/ssh\n"),
        &[],
    );
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains(
            "mounts[0].host: `secrets/ssh` is relative: make it absolute, start it with `~` or pass --relative-to DIR"
        ),
        "{stderr}"
    );
}

#[test]
fn relative_to_is_given_at_most_once() {
    let h = Home::new("relative-twice");
    let (code, _, stderr) = dry_run(&h, HARNESS, &["--relative-to", "/a", "--relative-to", "/b"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("--relative-to <DIR>' cannot be used multiple times"),
        "{stderr}"
    );
}

// The broker secret source: a fixture profile, the token file checks, the
// stored-login refusal and the other-harness error, all through the binary.

const TOKEN: &str = "tok-do-not-print-0123";

/// Runs `bree run --dry-run` on the broker fixture with a token file in the
/// temp HOME, mode `mode` (`None`: no file).
fn broker_run(h: &Home, mode: Option<u32>, args: &[&str]) -> (Option<i32>, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = h.home().join(".config/bree");
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(mode) = mode {
        let f = dir.join("broker-token");
        std::fs::write(&f, TOKEN).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let root = fixtures("broker");
    let mut all = vec![
        "--root",
        root.to_str().unwrap(),
        "-p",
        "broker",
        "run",
        "--dry-run",
    ];
    all.extend(args);
    let out = bree(h, &all);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn broker_dry_run_sets_the_claude_variables_and_no_token() {
    let h = Home::new("broker");
    let (code, stdout, stderr) = broker_run(&h, Some(0o600), &["claude"]);
    assert_eq!(code, Some(0), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.contains(&"  -e ANTHROPIC_BASE_URL=http://127.0.0.1:8088/anthropic"));
    assert!(lines.contains(&"  -e ANTHROPIC_AUTH_TOKEN=praxis-substitute:anthropic"));
    assert!(lines.contains(&"  -e ANTHROPIC_CUSTOM_HEADERS"));
    assert!(lines.contains(&"  -e BROKER_TOKEN"));
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    assert!(!stdout.contains("ANTHROPIC_API_KEY"));
    assert!(!stdout.contains("CLAUDE_CODE_OAUTH_TOKEN"));
}

#[test]
fn broker_token_file_must_exist_and_be_private() {
    let h = Home::new("broker-mode");
    let (code, _, stderr) = broker_run(&h, None, &["claude"]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("broker token file"), "{stderr}");
    assert!(stderr.contains("is not available"), "{stderr}");
    let (code, _, stderr) = broker_run(&h, Some(0o644), &["claude"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("readable by group or others (mode 644)"),
        "{stderr}"
    );
    assert!(!stderr.contains(TOKEN));
}

#[test]
fn broker_refuses_a_stored_login_in_the_state_directory() {
    let h = Home::new("broker-login");
    let claude = h.home().join(".local/share/bree/state/broker/.claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join(".credentials.json"), "{}").unwrap();
    let (code, stdout, stderr) = broker_run(&h, Some(0o600), &["claude"]);
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains(".credentials.json") && stderr.contains("move it away"),
        "{stderr}"
    );
}

#[test]
fn broker_refuses_other_harnesses() {
    let h = Home::new("broker-harness");
    for harness in ["pi", "opencode", "shell"] {
        let (code, _, stderr) = broker_run(&h, Some(0o600), &[harness]);
        assert_eq!(code, Some(2), "{harness}");
        assert!(stderr.contains("only the claude harness"), "{stderr}");
    }
    let (code, _, stderr) = dry_run(
        &h,
        "harness: pi\nsecrets:\n  b:\n    source: broker\n    url: http://x\n    token: /t\n    route: anthropic\n",
        &[],
    );
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("source broker works with the claude harness only"),
        "{stderr}"
    );
}
