//! The auto mode seed of Claude Code's settings on the container target
//! (docs/design.md, "What the renderer writes"): the description of the
//! container that goes into `autoMode.environment`, written from the profile,
//! and the rule that keeps it in that list after the profile's settings blocks
//! are merged.
//!
//! Every sentence of [`entry`] states something `bree run` (src/run.rs) makes
//! true for the profile. A change to what `run` mounts, passes or sets is a
//! change to this text.

use std::path::Path;

use serde_json::{Value, json};

use crate::profile::{
    Harness, Kvm, Mount, MountSource, Network, Profile, Secret, SecretSrc, Workdir,
};

/// The name of the seed among the manifest sources of `settings.json`. It is
/// no block, like `tiers.yaml`.
pub const SOURCE: &str = "auto mode (bree)";

/// Claude Code's placeholder for its built-in entries of an `autoMode` list.
const DEFAULTS: &str = "$defaults";

/// The settings bree starts from: the auto mode opt-in accepted, and the
/// built-in entries of `autoMode.environment` followed by `entry`.
pub fn seed(entry: &str) -> Value {
    json!({
        "skipAutoPermissionPrompt": true,
        "autoMode": {"environment": [DEFAULTS, entry]},
    })
}

/// Called with the settings after every block is merged: appends `entry` to
/// `autoMode.environment` when the merge left an array there that lacks it, so
/// a block's own list keeps its entries and gets bree's after them. A key that
/// a block removed, or made something other than an array, stays as it is: that
/// is how a profile opts out.
pub fn keep_entry(settings: &mut Value, entry: &str) {
    if let Some(Value::Array(list)) = settings.pointer_mut("/autoMode/environment") {
        if !list.iter().any(|v| v.as_str() == Some(entry)) {
            list.push(Value::String(entry.to_string()));
        }
    }
}

/// The description of the container of `p`, as one string for
/// `autoMode.environment`. It names no secret value and reads no secret source,
/// and it holds nothing that changes between two renders of the same profile.
pub fn entry(p: &Profile) -> String {
    let mut s: Vec<String> = vec![
        format!(
            "Claude Code runs in a rootless podman container that bree started, for the bree profile {}.",
            p.name
        ),
        "The user namespace is keep-id: uid 1000 in the container is the host user.".to_string(),
    ];
    match &p.state_dir {
        Some(dir) => {
            s.push(format!(
                "The home directory ~ is bree's state directory of this profile ({} on the host), not the host user's home.",
                dir.display()
            ));
            s.push(
                "Files under ~ are this container's state, except under the mounts described below."
                    .to_string(),
            );
        }
        None => {
            s.push(
                "The home directory ~ is the image's own, in the container's file system, not the host user's home."
                    .to_string(),
            );
            s.push(
                "It is discarded with the container, except under the mounts described below."
                    .to_string(),
            );
        }
    }
    s.extend(p.mounts.iter().map(mount));
    s.push("The bree config roots are mounted read-only under /run/bree/roots.".to_string());
    s.push(
        "bree run -m adds more host mounts for that run, read-write unless given as ro."
            .to_string(),
    );
    s.extend(workdir(&p.container.workdir));
    s.push(network(p.container.network).to_string());
    if p.container.nested_podman {
        let volume = match &p.container.storage_volume {
            Some(v) => v.clone(),
            None => format!("bree-{}-containers", p.name),
        };
        s.push(format!(
            "Rootless podman works inside the container, with its storage in the podman volume {volume} at ~/.local/share/containers."
        ));
        s.push("Containers started there stay inside this container.".to_string());
    }
    match p.container.kvm {
        Kvm::Off => {}
        Kvm::On => {
            s.push("/dev/kvm is available, for VMs that stay inside the container.".to_string());
        }
        Kvm::Optional => s.push(
            "/dev/kvm is available when the host has it, for VMs that stay inside the container."
                .to_string(),
        ),
    }
    s.extend(secrets(&p.secrets));
    s.join(" ")
}

/// One mount of the profile: where it comes from, where it is, whether it can
/// be written, and, for a host path that can, that the changes are real.
fn mount(m: &Mount) -> String {
    let mode = if m.read_only {
        "read-only"
    } else {
        "read-write"
    };
    match &m.source {
        MountSource::Host(host) => {
            let optional = if m.optional { " (optional)" } else { "" };
            let real = if m.read_only {
                ""
            } else {
                " Changes there are real changes on the host."
            };
            format!(
                "The host path {} is mounted at {}, {mode}{optional}.{real}",
                host.display(),
                m.container.display()
            )
        }
        MountSource::Volume(name) => format!(
            "The podman volume {name} is mounted at {}, {mode}.",
            m.container.display()
        ),
    }
}

/// What the working directory is, since `bree run` adds the directory it was
/// started in (`cwd`), which a render cannot know.
fn workdir(w: &Workdir) -> Vec<String> {
    let real = "Changes there are real changes on the host, unless a read-only mount holds it.";
    match w {
        Workdir::Cwd => vec![
            "The working directory is the host directory bree run was started in, inside one of the mounts above or else mounted read-write at the same path.".to_string(),
            real.to_string(),
        ],
        Workdir::Mount => vec![
            "The working directory is the host directory bree run was started in, which is inside one of the mounts above.".to_string(),
            real.to_string(),
        ],
        Workdir::Path(path) => vec![format!(
            "The working directory is {}, set by the profile. The host's current directory is not mounted.",
            path.display()
        )],
    }
}

fn network(n: Network) -> &'static str {
    match n {
        Network::Pasta => {
            "The network is pasta: the container has its own network namespace with outbound internet access, and the host's local network is reachable."
        }
        Network::Host => {
            "The container shares the host's network namespace, so services listening on the host's localhost are reachable."
        }
        Network::None => "The container has no network.",
    }
}

/// The secrets by the names they reach the container under, one sentence each,
/// then that no value is written. A variable that several secrets name is
/// listed with the first, as `bree run` passes it once. A secret that gives
/// Claude Code nothing is left out.
fn secrets(all: &[Secret]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for s in all {
        let broker: &[&str] = match s.source {
            SecretSrc::Broker { .. } => &[
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_CUSTOM_HEADERS",
            ],
            _ => &[],
        };
        let mut vars: Vec<&str> = Vec::new();
        let named = s.env.iter().chain(s.harness_env.get(Harness::Claude));
        for n in broker.iter().copied().chain(named.map(String::as_str)) {
            if !seen.contains(&n) && !vars.contains(&n) {
                vars.push(n);
            }
        }
        seen.extend(&vars);
        let file: Option<&Path> = match (&s.mount, &s.source) {
            (Some(mount), SecretSrc::File { .. }) => Some(mount),
            _ => None,
        };
        let mut how = Vec::new();
        if !vars.is_empty() {
            let noun = if vars.len() == 1 {
                "variable"
            } else {
                "variables"
            };
            how.push(format!("the environment {noun} {}", join_and(&vars)));
        }
        if let Some(path) = file {
            how.push(format!("the read-only file {}", path.display()));
        }
        if how.is_empty() {
            continue;
        }
        let when = if s.required {
            ""
        } else {
            ", when its source is available"
        };
        out.push(format!(
            "A credential reaches the container as {}{when}.",
            how.join(" and ")
        ));
    }
    if !out.is_empty() {
        out.push("The values of credentials are never written here.".to_string());
    }
    out
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testutil::{TempDir, env};

    fn profile(text: &str) -> Profile {
        let t = TempDir::new("automode");
        let file = t.0.join("p.yaml");
        std::fs::write(&file, text).unwrap();
        crate::profile::load(&file, "p", &env()).unwrap()
    }

    /// The `work` and `home` profiles of the design, in the fixture root.
    fn fixture(name: &str) -> Profile {
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/roots/shared/profiles")
            .join(format!("{name}.yaml"));
        crate::profile::load(&file, name, &env()).unwrap()
    }

    /// The sentences of an entry, for a readable failure.
    fn sentences(entry: &str) -> Vec<&str> {
        entry.split(". ").collect()
    }

    #[test]
    fn a_pasta_profile_with_nested_podman_kvm_two_mounts_and_a_secret() {
        let p = profile(
            "harness: claude
container:
  network: pasta
  nested-podman: true
  kvm: true
mounts:
  - host: ~/src
  - host: ~/.gitconfig
    mode: ro
    optional: true
secrets:
  corp:
    source: env
    var: CORP_KEY
    env: [CORP_KEY]
    required: false
    harness-env:
      claude: [ANTHROPIC_API_KEY]
      pi: [OPENAI_API_KEY]
",
        );
        let want = "Claude Code runs in a rootless podman container that bree started, for the bree profile p. \
The user namespace is keep-id: uid 1000 in the container is the host user. \
The home directory ~ is bree's state directory of this profile (/home/alice/.local/share/bree/state/p on the host), not the host user's home. \
Files under ~ are this container's state, except under the mounts described below. \
The host path /home/alice/src is mounted at /home/alice/src, read-write. Changes there are real changes on the host. \
The host path /home/alice/.gitconfig is mounted at /home/alice/.gitconfig, read-only (optional). \
The bree config roots are mounted read-only under /run/bree/roots. \
bree run -m adds more host mounts for that run, read-write unless given as ro. \
The working directory is the host directory bree run was started in, inside one of the mounts above or else mounted read-write at the same path. \
Changes there are real changes on the host, unless a read-only mount holds it. \
The network is pasta: the container has its own network namespace with outbound internet access, and the host's local network is reachable. \
Rootless podman works inside the container, with its storage in the podman volume bree-p-containers at ~/.local/share/containers. \
Containers started there stay inside this container. \
/dev/kvm is available, for VMs that stay inside the container. \
A credential reaches the container as the environment variables CORP_KEY and ANTHROPIC_API_KEY, when its source is available. \
The values of credentials are never written here.";
        assert_eq!(sentences(&entry(&p)), sentences(want));
        assert_eq!(entry(&p), want);
    }

    #[test]
    fn a_host_network_profile_with_nothing_else() {
        let p = profile(
            "harness: claude
container:
  network: host
  nested-podman: false
",
        );
        let want = "Claude Code runs in a rootless podman container that bree started, for the bree profile p. \
The user namespace is keep-id: uid 1000 in the container is the host user. \
The home directory ~ is bree's state directory of this profile (/home/alice/.local/share/bree/state/p on the host), not the host user's home. \
Files under ~ are this container's state, except under the mounts described below. \
The bree config roots are mounted read-only under /run/bree/roots. \
bree run -m adds more host mounts for that run, read-write unless given as ro. \
The working directory is the host directory bree run was started in, inside one of the mounts above or else mounted read-write at the same path. \
Changes there are real changes on the host, unless a read-only mount holds it. \
The container shares the host's network namespace, so services listening on the host's localhost are reachable.";
        assert_eq!(sentences(&entry(&p)), sentences(want));
        assert_eq!(entry(&p), want);
    }

    #[test]
    fn a_profile_without_a_state_directory_keeps_the_images_home() {
        let p = profile(
            "command: [crew-agent]
state:
  home: image
container:
  network: none
  nested-podman: false
  workdir: /home/agent/code
mounts:
  - host: /srv/crew/code
    container: /home/agent/code
",
        );
        assert_eq!(p.state_dir, None);
        let want = "Claude Code runs in a rootless podman container that bree started, for the bree profile p. \
The user namespace is keep-id: uid 1000 in the container is the host user. \
The home directory ~ is the image's own, in the container's file system, not the host user's home. \
It is discarded with the container, except under the mounts described below. \
The host path /srv/crew/code is mounted at /home/agent/code, read-write. Changes there are real changes on the host. \
The bree config roots are mounted read-only under /run/bree/roots. \
bree run -m adds more host mounts for that run, read-write unless given as ro. \
The working directory is /home/agent/code, set by the profile. The host's current directory is not mounted. \
The container has no network.";
        assert_eq!(sentences(&entry(&p)), sentences(want));
        assert_eq!(entry(&p), want);
    }

    #[test]
    fn the_design_profiles_describe_their_own_mounts_and_secrets() {
        let work = entry(&fixture("work"));
        assert!(
            work.contains("for the bree profile work."),
            "{}",
            sentences(&work).join("\n")
        );
        assert!(work.contains("(/home/alice/.local/share/bree/state/work on the host)"));
        assert!(
            work.contains(
                "The host path /home/alice/src is mounted at /home/alice/src, read-write."
            )
        );
        assert!(work.contains(
            "/home/alice/.gitconfig is mounted at /home/alice/.gitconfig, read-only (optional)."
        ));
        assert!(work.contains(
            "the environment variables CORP_KEY and ANTHROPIC_API_KEY, when its source is available."
        ));
        assert!(!work.contains("/dev/kvm"));
        let home = entry(&fixture("home"));
        assert!(home.contains("/dev/kvm is available, for VMs that stay inside the container."));
        assert!(
            home.contains("the podman volume bree-home-containers at ~/.local/share/containers")
        );
        assert!(!home.contains("credential"));
    }

    #[test]
    fn the_working_directory_follows_container_workdir() {
        let cwd = entry(&profile("harness: claude\n"));
        assert!(
            cwd.contains("or else mounted read-write at the same path."),
            "{cwd}"
        );
        let mounted = entry(&profile("harness: claude\ncontainer:\n  workdir: mount\n"));
        assert!(
            mounted.contains("which is inside one of the mounts above."),
            "{mounted}"
        );
        assert!(
            !mounted.contains("mounted read-write at the same path"),
            "{mounted}"
        );
        let fixed = entry(&profile("harness: claude\ncontainer:\n  workdir: /srv/w\n"));
        assert!(
            fixed.contains("The working directory is /srv/w, set by the profile. The host's current directory is not mounted."),
            "{fixed}"
        );
        assert!(!fixed.contains("Changes there are real"), "{fixed}");
    }

    #[test]
    fn nested_podman_names_its_storage_volume_and_kvm_optional_says_so() {
        let e = entry(&profile(
            "harness: claude\ncontainer:\n  storage-volume: scratch\n  kvm: optional\n",
        ));
        assert!(
            e.contains("in the podman volume scratch at ~/.local/share/containers."),
            "{e}"
        );
        assert!(
            e.ends_with("/dev/kvm is available when the host has it, for VMs that stay inside the container."),
            "{e}"
        );
        let e = entry(&profile(
            "harness: claude\ncontainer:\n  nested-podman: false\n",
        ));
        assert!(
            !e.contains("Rootless podman works inside") && !e.contains("/dev/kvm"),
            "{e}"
        );
    }

    #[test]
    fn volumes_and_read_only_host_mounts_are_described_by_their_kind() {
        let e = entry(&profile(
            "harness: claude
mounts:
  - volume: data
    container: /data
  - volume: ref
    container: /ref
    mode: ro
  - host: /srv/x
    container: /x
    mode: ro
",
        ));
        assert!(
            e.contains("The podman volume data is mounted at /data, read-write. "),
            "{e}"
        );
        assert!(
            e.contains("The podman volume ref is mounted at /ref, read-only. "),
            "{e}"
        );
        assert!(
            e.contains("The host path /srv/x is mounted at /x, read-only. "),
            "{e}"
        );
        // Only a read-write host mount says its changes are real, besides the working directory.
        assert_eq!(e.matches("Changes there are real").count(), 1, "{e}");
    }

    #[test]
    fn secrets_are_named_by_variable_and_path_and_never_by_value() {
        let e = entry(&profile(
            "harness: claude
container:
  nested-podman: false
secrets:
  forge:
    source: file
    path: ~/.secrets/forge
    env: [FORGE_TOKEN]
    mount: /run/secrets/forge-token
  again:
    source: env
    var: HOME_ONLY
    env: [FORGE_TOKEN, ANOTHER]
  pi-only:
    source: env
    var: PI_KEY
    harness-env:
      pi: [OPENAI_API_KEY]
",
        ));
        assert!(
            e.contains("A credential reaches the container as the environment variable FORGE_TOKEN and the read-only file /run/secrets/forge-token. "),
            "{e}"
        );
        // FORGE_TOKEN is passed once, with the first secret that names it.
        assert!(e.contains("as the environment variable ANOTHER. "), "{e}");
        assert!(
            !e.contains("OPENAI_API_KEY") && !e.contains("PI_KEY"),
            "{e}"
        );
        assert!(
            !e.contains(".secrets") && !e.contains("HOME_ONLY"),
            "no source is named: {e}"
        );
        assert!(
            e.ends_with("The values of credentials are never written here."),
            "{e}"
        );
    }

    #[test]
    fn a_broker_secret_names_the_variables_bree_run_sets_for_it() {
        let e = entry(&profile(
            "harness: claude
secrets:
  broker:
    source: broker
    url: http://broker.example:8080
    token: ~/.secrets/broker-token
    route: anthropic
    env: [EXTRA]
",
        ));
        assert!(
            e.contains("the environment variables ANTHROPIC_BASE_URL, ANTHROPIC_AUTH_TOKEN, ANTHROPIC_CUSTOM_HEADERS and EXTRA."),
            "{e}"
        );
        assert!(
            !e.contains("broker.example") && !e.contains("broker-token"),
            "{e}"
        );
    }

    #[test]
    fn the_entry_is_the_same_every_time() {
        let p = fixture("work");
        assert_eq!(entry(&p), entry(&p));
        assert_eq!(entry(&p), entry(&fixture("work")));
    }

    #[test]
    fn the_seed_has_the_opt_in_and_the_defaults_before_the_entry() {
        let v = seed("E");
        assert_eq!(
            v,
            json!({"skipAutoPermissionPrompt": true, "autoMode": {"environment": ["$defaults", "E"]}})
        );
        let keys: Vec<_> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["skipAutoPermissionPrompt", "autoMode"]);
    }

    fn kept(settings: Value, entry: &str) -> Value {
        let mut v = settings;
        keep_entry(&mut v, entry);
        v
    }

    #[test]
    fn keep_entry_appends_to_an_array_that_lacks_it() {
        assert_eq!(
            kept(
                json!({"autoMode": {"environment": ["org", "$defaults"]}}),
                "E"
            ),
            json!({"autoMode": {"environment": ["org", "$defaults", "E"]}})
        );
        assert_eq!(
            kept(json!({"autoMode": {"environment": []}}), "E"),
            json!({"autoMode": {"environment": ["E"]}})
        );
        // Not twice, wherever it is.
        let seeded = seed("E");
        assert_eq!(kept(seeded.clone(), "E"), seeded);
        let first = json!({"autoMode": {"environment": ["E", "org"]}});
        assert_eq!(kept(first.clone(), "E"), first);
    }

    #[test]
    fn keep_entry_leaves_an_opt_out_and_everything_else_alone() {
        for same in [
            json!({}),
            json!({"autoMode": {}}),
            json!({"autoMode": null}),
            json!({"autoMode": "off"}),
            json!({"autoMode": {"environment": null}}),
            json!({"autoMode": {"environment": "text"}}),
            json!({"autoMode": {"environment": {"a": 1}}}),
            json!({"autoMode": {"allow": ["x"], "soft_deny": ["y"]}}),
        ] {
            assert_eq!(kept(same.clone(), "E"), same);
        }
    }
}
