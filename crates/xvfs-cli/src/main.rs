//! The `xvfs` command-line interface.
//!
//! M1 gives it the commands that exercise the repository and snapshot API;
//! `mount`, `unmount`, `inspect`, `status`, `diff`, and `search` arrive with
//! M2-M4. This phase (M1.1a) establishes the binary and the version surface.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "xvfs", version, about = "Agent-oriented virtual Git workspace")]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
  /// Print version and build information, including the pinned libgit2 and
  /// stock Git versions the local and CI environments must agree on.
  Version,
}

fn main() -> anyhow::Result<()> {
  let cli = Cli::parse();
  match cli.command {
    Command::Version => {
      println!("xvfs {}", env!("CARGO_PKG_VERSION"));
      println!("api-version {}", xvfs_types::API_VERSION);
      println!("state-format-version {}", xvfs_types::STATE_FORMAT_VERSION);
    }
  }
  Ok(())
}
