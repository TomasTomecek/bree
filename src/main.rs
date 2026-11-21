mod automode;
mod check;
mod cli;
mod config;
mod contents;
mod entrypoint;
mod error;
mod export;
mod host;
mod image;
mod profile;
mod render;
mod role;
mod run;
mod tiers;
mod yaml;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use cli::{
    CheckArgs, Cli, Command, EntrypointArgs, ExportCommand, GlobalOpts, ImageCommand,
    ProfileCommand, RenderArgs, RunArgs, Target,
};
use config::Env;
use error::Error;

/// Exit status for a usage or configuration error (docs/design.md, "CLI").
/// clap uses the same status for its own usage errors.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Run(args) => run_command(&cli.global, args),
        Command::Render(args) => render_command(&cli.global, args),
        Command::Check(args) => check_command(&cli.global, args),
        Command::Profile(ProfileCommand::Contents {
            harnesses,
            adds,
            json,
        }) => contents_command(&cli.global, harnesses, adds, *json),
        Command::Image(ImageCommand::Build { no_cache }) => {
            image_build_command(&cli.global, *no_cache)
        }
        Command::Image(ImageCommand::Status) => image_status_command(&cli.global),
        Command::Export(ExportCommand::Quadlet) => export_quadlet_command(&cli.global),
        Command::Entrypoint(args) => entrypoint_command(&cli.global, args),
        other => {
            eprintln!("bree: {}: not implemented", other.name());
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bree: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// What every command needs: the process environment, the roots and the
/// profile with the file it came from.
struct Loaded {
    env: Env,
    roots: Vec<PathBuf>,
    profile: profile::Profile,
    profile_path: PathBuf,
}

fn load(global: &GlobalOpts) -> Result<Loaded, Error> {
    let env = Env::from_process()?.with_relative_to(global.relative_to.as_deref());
    let explicit = global.config.is_some();
    let config_path = global
        .config
        .clone()
        .unwrap_or_else(|| config::default_config_path(&env));
    let user_config = config::load_user_config(&config_path, explicit)?;
    let roots = config::resolve_roots(&global.roots, &user_config, &env)?;
    let name = config::profile_name(global.profile.as_deref(), &user_config, &config_path)?;
    let profile_path = config::find_profile(&roots, &name)?;
    let profile = profile::load(&profile_path, &name, &env)?;
    Ok(Loaded {
        env,
        roots,
        profile,
        profile_path,
    })
}

/// `bree run`: resolve the roots and the profile, build the podman command,
/// then print it (`--dry-run`) or replace this process with podman.
fn run_command(global: &GlobalOpts, args: &RunArgs) -> Result<ExitCode, Error> {
    let Loaded {
        env,
        roots,
        profile,
        ..
    } = load(global)?;
    let opts = run::RunOpts {
        harness: args.harness,
        args: &args.args,
        mounts: &args.mounts,
        no_pull: args.no_pull,
        envs: &args.envs,
    };
    let inv = run::build(
        &profile,
        &roots,
        &opts,
        &env,
        &run::random_suffix(),
        &run::read_secret,
    )?;
    if args.dry_run {
        for w in &inv.warnings {
            eprintln!("{w}");
        }
        print!("{}", inv.render());
        return Ok(ExitCode::SUCCESS);
    }
    image::ensure(&profile.image, args.no_pull)?;
    Err(run::exec(&inv, global.verbose))
}

/// A profile with `state.home: image` has no state directory to render into.
fn no_state_dir() -> Error {
    Error::new(
        "the profile keeps the image's home (state.home: image) and has no state directory; pass --out DIR",
    )
}

/// `bree image build`: build the profile's image reference.
fn image_build_command(global: &GlobalOpts, no_cache: bool) -> Result<ExitCode, Error> {
    let Loaded { profile, .. } = load(global)?;
    if profile.image.source == profile::Source::Local {
        return Err(Error::new(format!(
            "image build: {} has image.source: local, which bree never builds",
            profile.image.reference
        )));
    }
    image::build(&profile.image.reference, no_cache)?;
    Ok(ExitCode::SUCCESS)
}

/// `bree export quadlet`: print the crew profile's `[Container]` section.
fn export_quadlet_command(global: &GlobalOpts) -> Result<ExitCode, Error> {
    let Loaded { env, profile, .. } = load(global)?;
    print!("{}", export::quadlet(&profile, &env)?);
    Ok(ExitCode::SUCCESS)
}

/// `bree image status`: exit 1 unless the local image is current.
fn image_status_command(global: &GlobalOpts) -> Result<ExitCode, Error> {
    let Loaded { profile, .. } = load(global)?;
    image::status(&profile.image)
}

impl From<cli::Harness> for profile::Harness {
    fn from(h: cli::Harness) -> Self {
        match h {
            cli::Harness::Claude => profile::Harness::Claude,
            cli::Harness::Pi => profile::Harness::Pi,
            cli::Harness::Opencode => profile::Harness::Opencode,
        }
    }
}

/// Whether the command works on the host's own harness directories: the host
/// target without `--out`, which keeps the plain semantics of a directory.
fn is_host(target: &cli::RenderTargetArgs) -> bool {
    target.out.is_none() && target.target == Target::Host
}

/// Whether the render is for the container target. `--out` does not change
/// the target: `--target host --out DIR` renders the host's files, and they
/// get no seed that describes the container.
fn is_container(target: &cli::RenderTargetArgs) -> bool {
    target.target == Target::Container
}

/// `bree render`: render in memory, then write the tree under `--out`, the
/// host's harness directories with the managed-copy rule, or, for the
/// container target, the profile's state directory.
fn render_command(global: &GlobalOpts, args: &RenderArgs) -> Result<ExitCode, Error> {
    let loaded = load(global)?;
    // The container target is the profile's state directory, the home that
    // `bree entrypoint` renders into at every start.
    let harnesses: Vec<profile::Harness> = args.harnesses.iter().map(|&h| h.into()).collect();
    let opts = render::Options {
        harnesses: &harnesses,
        adds: &args.target.adds,
        vars: &args.target.vars,
        container: is_container(&args.target),
    };
    let r = render::render(&loaded.profile, &loaded.profile_path, &loaded.roots, &opts)?;
    if global.verbose {
        for h in &r.harnesses {
            for u in &h.used {
                eprintln!(
                    "{}: {} {} ({})",
                    h.harness.name(),
                    u.kind.name(),
                    u.block,
                    u.root
                );
            }
        }
    }
    if is_host(&args.target) {
        let ignore = check::load_ignore(&loaded.roots)?;
        return Ok(
            match host::render(
                &r,
                &loaded.env.home,
                &ignore,
                args.force,
                !args.harnesses.is_empty(),
            )? {
                host::Outcome::Written(w) => {
                    print!("{}", w.text());
                    ExitCode::SUCCESS
                }
                host::Outcome::Conflicts(c) => {
                    print!("{}", c.text());
                    ExitCode::from(1)
                }
            },
        );
    }
    let out = args
        .target
        .out
        .clone()
        .or_else(|| loaded.profile.state_dir.clone())
        .ok_or_else(no_state_dir)?;
    let w = render::write(&r, &out, &out)?;
    print!("{}", w.text());
    Ok(ExitCode::SUCCESS)
}

/// `bree check`: render in memory and compare with the files on disk. The
/// base directory is `--out`, the host's home for the host target, or the
/// profile's state directory.
fn check_command(global: &GlobalOpts, args: &CheckArgs) -> Result<ExitCode, Error> {
    let loaded = load(global)?;
    let opts = render::Options {
        harnesses: &[],
        adds: &args.target.adds,
        vars: &args.target.vars,
        container: is_container(&args.target),
    };
    let r = render::render(&loaded.profile, &loaded.profile_path, &loaded.roots, &opts)?;
    let ignore = check::load_ignore(&loaded.roots)?;
    let report = if is_host(&args.target) {
        host::check(&r, &loaded.env.home, &ignore)?
    } else {
        let base = args
            .target
            .out
            .clone()
            .or_else(|| loaded.profile.state_dir.clone())
            .ok_or_else(no_state_dir)?;
        check::check(&r, &base, &ignore)?
    };
    print!("{}", check::format(&report, args.diff));
    Ok(if report.rows.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// `bree profile contents`: the inventory of the same in-memory render.
fn contents_command(
    global: &GlobalOpts,
    harnesses: &[cli::Harness],
    adds: &[String],
    json: bool,
) -> Result<ExitCode, Error> {
    let loaded = load(global)?;
    let harnesses: Vec<profile::Harness> = harnesses.iter().map(|&h| h.into()).collect();
    // The inventory is of what the container gets.
    let opts = render::Options {
        harnesses: &harnesses,
        adds,
        vars: &[],
        container: true,
    };
    let r = render::render(&loaded.profile, &loaded.profile_path, &loaded.roots, &opts)?;
    let inventories = contents::inventory(&r);
    if json {
        print!("{}", contents::json(&r.profile, &inventories));
    } else {
        print!("{}", contents::text(&inventories));
    }
    Ok(ExitCode::SUCCESS)
}

/// `bree entrypoint`: render the profile into the container home, then exec
/// the command. The roots are the ones `bree run` mounted under
/// `/run/bree/roots/`, or `--root` (for tests); the user config is not read,
/// since the container has none. Without a command it only renders.
fn entrypoint_command(global: &GlobalOpts, args: &EntrypointArgs) -> Result<ExitCode, Error> {
    let env = config::Env::from_process()?.with_relative_to(global.relative_to.as_deref());
    let roots = if global.roots.is_empty() {
        entrypoint::mounted_roots(std::path::Path::new(entrypoint::ROOTS_DIR))?
    } else {
        config::resolve_roots(&global.roots, &config::UserConfig::default(), &env)?
    };
    let name = entrypoint::profile_name(global.profile.as_deref(), env.var("BREE_PROFILE"))?;
    let name = config::profile_name(
        Some(&name),
        &config::UserConfig::default(),
        std::path::Path::new(""),
    )?;
    let profile_path = config::find_profile(&roots, &name)?;
    let profile = profile::load(&profile_path, &name, &env)?;
    let opts = render::Options {
        harnesses: &[],
        adds: &[],
        vars: &[],
        container: true,
    };
    let r = render::render(&profile, &profile_path, &roots, &opts)?;
    let w = render::write(&r, &env.home, &env.home)?;
    if global.verbose {
        eprint!("{}", render::align(&w.rows));
    }
    eprintln!(
        "bree: rendered profile {name}: {} written, {} current, {} removed",
        w.written, w.current, w.removed
    );
    if args.command.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    Err(entrypoint::exec(
        &args.command,
        &env.home,
        env.var("CLAUDE_CONFIG_DIR"),
    ))
}

#[cfg(test)]
mod tests {
    use super::cli::{Cli, Command, RunHarness, Target};
    use clap::{CommandFactory, Parser};

    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_takes_harness_and_args_after_double_dash() {
        let cli = Cli::try_parse_from([
            "bree",
            "-p",
            "work",
            "run",
            "--dry-run",
            "-m",
            "/srv",
            "claude",
            "--",
            "--continue",
        ])
        .unwrap();
        assert_eq!(cli.global.profile.as_deref(), Some("work"));
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.harness, Some(RunHarness::Claude));
        assert!(run.dry_run);
        assert_eq!(run.mounts, ["/srv"]);
        assert_eq!(run.args, ["--continue"]);
    }

    #[test]
    fn global_options_work_after_the_command() {
        let cli = Cli::try_parse_from([
            "bree", "check", "--root", "/a", "--root", "/b", "-p", "home", "--target", "host",
        ])
        .unwrap();
        assert_eq!(cli.global.roots.len(), 2);
        assert_eq!(cli.global.profile.as_deref(), Some("home"));
        let Command::Check(check) = cli.command else {
            panic!("expected check");
        };
        assert_eq!(check.target.target, Target::Host);
    }

    #[test]
    fn unknown_harness_is_rejected() {
        assert!(Cli::try_parse_from(["bree", "run", "vim"]).is_err());
        assert!(Cli::try_parse_from(["bree", "render", "--harness", "shell"]).is_err());
    }
}
