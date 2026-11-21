//! The command tree of docs/design.md ("CLI"). Every command parses its
//! options here; what each one does comes with the later tasks of the plan.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BREE_COMMIT"), ")");

const INTERNAL_HELP: &str = "\
Internal commands:
  entrypoint  The base image's entry point: render the profile, then exec the command";

#[derive(Debug, Parser)]
#[command(
    name = "bree",
    version = VERSION,
    about = "Run coding agents in containers from profiles and render their configuration",
    after_help = INTERNAL_HELP,
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct GlobalOpts {
    /// Profile to use [default: default-profile from the user config]
    #[arg(short, long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Config root, repeatable; replaces the configured roots [env: BREE_ROOTS, colon-separated]
    #[arg(long = "root", global = true, value_name = "DIR")]
    pub roots: Vec<PathBuf>,

    /// Directory that relative host paths in profile mounts resolve against
    #[arg(long = "relative-to", global = true, value_name = "DIR")]
    pub relative_to: Option<PathBuf>,

    /// User config file [default: ~/.config/bree/config.yaml]
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Print what bree does to stderr
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a harness in the profile's container, in the current directory
    Run(RunArgs),
    /// Render the profile's blocks into the files each harness reads
    Render(RenderArgs),
    /// Exit 1 when rendered files are stale, writing nothing
    Check(CheckArgs),
    /// Write runtime changes of rendered files back into the blocks
    Capture(CaptureArgs),
    /// List, show and inventory profiles
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Print the tiers manifest after merging the roots
    Tiers(TiersArgs),
    /// Build, pull or inspect the base image
    #[command(subcommand)]
    Image(ImageCommand),
    /// Print a crew profile as another tool's configuration
    #[command(subcommand)]
    Export(ExportCommand),
    /// Check the environment without changing it
    Doctor,
    /// Print a shell completion script
    Completions(CompletionsArgs),
    /// The base image's entry point: render the profile, then exec the command
    #[command(hide = true)]
    Entrypoint(EntrypointArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Harness {
    Claude,
    Pi,
    Opencode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunHarness {
    Claude,
    Pi,
    Opencode,
    /// bash
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Target {
    #[default]
    Container,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Harness to start [default: the profile's harness]
    pub harness: Option<RunHarness>,

    /// Arguments passed to the harness
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,

    /// Extra bind mount for this run, HOST[:CONTAINER][:ro]
    #[arg(short, long = "mount", value_name = "SPEC")]
    pub mounts: Vec<String>,

    /// Print the podman run command, one argument per line, and exit
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Never pull or build the image; fail when it is missing
    #[arg(long)]
    pub no_pull: bool,

    /// Pass a host environment variable by name
    #[arg(long = "env", value_name = "NAME")]
    pub envs: Vec<String>,
}

/// Options shared by render and check.
#[derive(Debug, Args)]
pub struct RenderTargetArgs {
    /// Where rendered files go
    #[arg(long, value_enum, default_value_t = Target::Container)]
    pub target: Target,

    /// Write the tree under DIR instead of the target
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Append a block to one of the profile's lists (KIND: rules, skills,
    /// commands, settings.<harness>, files.<harness>)
    #[arg(long = "add", value_name = "KIND=NAME")]
    pub adds: Vec<String>,

    /// Set a template variable over the profile's vars
    #[arg(long = "var", value_name = "NAME=VALUE")]
    pub vars: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RenderArgs {
    #[command(flatten)]
    pub target: RenderTargetArgs,

    /// Render only these harnesses [default: the profile's harnesses]
    #[arg(long = "harness", value_enum, value_name = "NAME")]
    pub harnesses: Vec<Harness>,

    /// Overwrite files edited since bree wrote them (host target)
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    pub target: RenderTargetArgs,

    /// Add a unified diff per file that is not current
    #[arg(long)]
    pub diff: bool,
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// Where to read the rendered files from
    #[arg(long, value_enum, default_value_t = Target::Container)]
    pub target: Target,

    /// Settings block to merge settings changes into
    #[arg(long, value_name = "NAME")]
    pub into: Option<String>,

    /// Adopt a file the harness created as a new block
    #[arg(long = "add", value_name = "PATH")]
    pub adds: Vec<PathBuf>,

    /// Root for adopted blocks [default: the root that holds the profile]
    #[arg(long, value_name = "DIR")]
    pub to_root: Option<PathBuf>,

    /// Print the changes as diffs of the source files and write nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// List every profile in the roots
    List,
    /// Print the profile after resolving the roots
    Show {
        #[arg(long)]
        json: bool,
    },
    /// List what a render of the profile would contain
    Contents {
        /// Only these harnesses
        #[arg(long = "harness", value_enum, value_name = "NAME")]
        harnesses: Vec<Harness>,

        /// Append a block to one of the profile's lists
        #[arg(long = "add", value_name = "KIND=NAME")]
        adds: Vec<String>,

        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
pub struct TiersArgs {
    /// Print only this pool
    #[arg(long, value_name = "NAME")]
    pub pool: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Build the base image from the embedded Containerfile
    Build {
        /// Rebuild from scratch
        #[arg(long)]
        no_cache: bool,
    },
    /// Pull the profile's pinned image
    Pull,
    /// Print the wanted and local image and whether their bree matches
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ExportCommand {
    /// Print the quadlet [Container] section of a crew profile
    Quadlet,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Debug, Args)]
pub struct EntrypointArgs {
    /// Command to exec after rendering
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

impl Command {
    /// The command's name as typed, for messages.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Run(_) => "run",
            Command::Render(_) => "render",
            Command::Check(_) => "check",
            Command::Capture(_) => "capture",
            Command::Profile(ProfileCommand::List) => "profile list",
            Command::Profile(ProfileCommand::Show { .. }) => "profile show",
            Command::Profile(ProfileCommand::Contents { .. }) => "profile contents",
            Command::Tiers(_) => "tiers",
            Command::Image(ImageCommand::Build { .. }) => "image build",
            Command::Image(ImageCommand::Pull) => "image pull",
            Command::Image(ImageCommand::Status) => "image status",
            Command::Export(ExportCommand::Quadlet) => "export quadlet",
            Command::Doctor => "doctor",
            Command::Completions(_) => "completions",
            Command::Entrypoint(_) => "entrypoint",
        }
    }
}
