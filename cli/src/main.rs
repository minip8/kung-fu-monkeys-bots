//! `kfm`, the Kung Fu Monkeys bot ladder's command line tool (cli/README.md;
//! the engine's architecture §8 "The CLI"). It talks to the ladder over the
//! HTTP API of docs/ladder-api.md only, builds with the recipe of
//! docs/build.md, and plays with the engine's released `kfm_arena`.

mod api;
mod arena;
mod commands;
mod config;
mod error;
mod login;
mod pack;
mod recipe;
mod table;
mod wasm;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The Kung Fu Monkeys bot ladder from the command line: log in, build a bot
/// with the ladder's recipe, test it against the reference bot, submit it,
/// and follow its matches and replays.
#[derive(Parser)]
#[command(name = "kfm", version, propagate_version = true)]
#[command(after_help = "\
The five-minute path:
  cp -r examples/starter mybot && cd mybot
  kfm login
  kfm build                      # out/bot.wasm, in the ladder's build image
  kfm test                       # 10 ranked-shape matches against ref:hard
  kfm submit --wait --activate   # built and validated by the ladder, then ranked

Configuration: kfm's config.toml (see `kfm whoami`); KFM_SERVER and KFM_TOKEN override it.
Exit codes: 0 ok, 1 an error (the ladder's: \"kfm: <message> (<code>)\"), 2 usage,
3 a submission that failed or was rejected under --wait.")]
pub struct Cli {
    /// The ladder's URL [default: the config file's server, else https://bots.kfm.minip8.com]
    #[arg(long, global = true, env = "KFM_SERVER", value_name = "URL")]
    pub server: Option<String>,
    /// Print the ladder's (or kfm_arena's) raw JSON instead of a summary
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Log in through the browser and store a `submit` token
    Login {
        /// Print the URL instead of opening a browser, and wait
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the stored token and forget it
    Logout,
    /// Who the stored token belongs to, and their teams
    Whoami,
    /// Build a bot directory into a checked module with the ladder's recipe
    Build(BuildArgs),
    /// Play a module against the reference bot with the released kfm_arena
    Test(TestArgs),
    /// Upload a bot directory's source to a team
    Submit(SubmitArgs),
    /// A team's submissions
    #[command(subcommand)]
    Bots(BotsCmd),
    /// Ranked matches
    #[command(subcommand)]
    Matches(MatchesCmd),
    /// The board of a season
    Ladder {
        /// A season's name [default: the current one, else the last]
        #[arg(long)]
        season: Option<String>,
        /// The page, from 1 (50 teams a page)
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// The seasons, the current one marked
    Seasons,
    /// A match's replay
    #[command(subcommand)]
    Replay(ReplayCmd),
}

#[derive(Args)]
pub struct BuildArgs {
    /// The bot directory: every .c, .cc and .cpp file directly in it is compiled
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// The module [default: DIR/out/bot.wasm]
    #[arg(short, long, value_name = "OUT.wasm")]
    pub out: Option<PathBuf>,
    /// Build with a local clang (CLANG, clang on PATH, EMSDK) instead of the build image
    #[arg(long)]
    pub local: bool,
    /// The build image [default: ghcr.io/minip8/kfm-bot-build:<this kfm's release>]
    #[arg(long, env = "KFM_BUILD_IMAGE", conflicts_with = "local")]
    pub image: Option<String>,
}

#[derive(Args)]
pub struct TestArgs {
    /// The module
    #[arg(default_value = "out/bot.wasm")]
    pub module: PathBuf,
    /// The opponent: ref:easy, ref:medium, ref:hard or a module's path
    #[arg(long, default_value = "ref:hard", value_name = "SPEC")]
    pub against: String,
    /// Matches to play (each pair of them plays both sides)
    #[arg(long, default_value_t = 10)]
    pub matches: u32,
    /// The content mode [default: contest; none with --ffa]
    #[arg(long)]
    pub mode: Option<String>,
    /// A free-for-all of the module and three opponents instead of the ranked 5v5
    #[arg(long)]
    pub ffa: bool,
    /// The engine release whose kfm_arena plays [default: the ladder's when logged in, else this kfm's pin]
    #[arg(long, value_name = "TAG")]
    pub engine_tag: Option<String>,
    /// A local kfm_arena to play with instead of the released one
    #[arg(long, env = "KFM_ARENA", value_name = "PATH")]
    pub arena: Option<PathBuf>,
    /// The first match's seed (kfm_arena's default otherwise)
    #[arg(long)]
    pub seed: Option<u64>,
    /// A tick cap per match
    #[arg(long)]
    pub ticks: Option<u32>,
}

#[derive(Args)]
pub struct SubmitArgs {
    /// The bot directory to upload (its sources; out/, .git/, .kfm/ and *.wasm stay behind)
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// The team, by name or id [default: your only team]
    #[arg(long)]
    pub team: Option<String>,
    /// The directory inside DIR that holds the sources the recipe compiles
    #[arg(long, default_value = ".", value_name = "SUBDIR")]
    pub entry: String,
    /// The ladder language
    #[arg(long, default_value = "cpp", value_parser = ["cpp", "c"])]
    pub language: String,
    /// Make it the team's active bot once it is ready (implies --wait)
    #[arg(long)]
    pub activate: bool,
    /// Wait until it is ready, failed or rejected (exit 3 unless ready)
    #[arg(long)]
    pub wait: bool,
}

#[derive(Subcommand)]
pub enum BotsCmd {
    /// A team's submissions, newest first
    List {
        /// The team, by name or id [default: your only team]
        #[arg(long)]
        team: Option<String>,
    },
    /// Make a ready submission its team's active bot
    Activate {
        /// The submission's id
        submission: String,
        /// The team [default: the submission's]
        #[arg(long)]
        team: Option<String>,
    },
    /// A submission's build and validation log
    Log {
        /// The submission's id
        submission: String,
    },
}

#[derive(Subcommand)]
pub enum MatchesCmd {
    /// Recorded games, newest first (25 a page)
    List {
        /// Only this team's: a team id, or the name of one of your teams
        #[arg(long)]
        team: Option<String>,
        /// Only this season's (a name)
        #[arg(long)]
        season: Option<String>,
        /// The page, from 1
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// One game: sides, scores, faults, the replay and the viewer
    Show {
        /// The match's id
        id: String,
    },
}

#[derive(Subcommand)]
pub enum ReplayCmd {
    /// Download a match's .kfmr
    Get {
        /// The match's id
        id: String,
        /// Where to write it [default: <id>.kfmr]
        #[arg(short, long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Open a match's replay in its own release's browser client
    Open {
        /// The match's id
        id: String,
        /// Print the URL only
        #[arg(long)]
        no_browser: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = commands::run(cli) {
        eprintln!("kfm: {err}");
        std::process::exit(err.exit_code());
    }
}
