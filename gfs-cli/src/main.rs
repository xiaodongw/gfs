//! The `gfs` command-line interface.
//!
//! Two families of command, and the split matters:
//!
//! * **Workspace commands** — `mount`, `unmount`, `inspect`, `health`,
//!   `refresh` — act on a *mounted workspace*, over the control socket beside it.
//!   They are what an orchestrator and an agent use.
//! * **`daemon`** — the `gfs-fuse` host process itself, which serves every mount
//!   on the machine (ADR 0008). `mount` and `clone` start it on demand and ask it
//!   for a workspace; nothing else in this file knows it exists.
//! * **Snapshot commands** — `resolve`, `ls`, `cat` — read the server directly,
//!   with no mount involved. They are how the API is demonstrated and debugged.
//! * **Tool substitutes** — `rg`, `find`, `log` — answer from the server's index
//!   and the overlay journal the questions `rg`, `find` and `git log` would
//!   answer by walking the mount. They parse their own argv rather than using
//!   `clap`, because refusing an unsupported flag *by name* is part of what they
//!   promise; see `gfs_cli::rg`, `::find` and `::log`.
//! * **`lease`** — creates, renews, and releases a retention lease by hand. A
//!   mount does this for itself; the subcommand exists so M1's lease machine can
//!   be exercised without one, which is what `scripts/dev-stack.sh` does.
//!
//! # Where things live
//!
//! Given `--workspace /path/to/ws`, the state directory defaults to
//! `/path/to/ws.gfs`. Every workspace command can therefore find a running
//! daemon from the workspace path alone, which is the only thing a job knows.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gfs_cli::search_output;
use gfs_cli::workspace::{call, call_host, locate, state_dir_for};
use gfs_mount::control::{Request, Response};
use gfs_proto::v1;

#[derive(Parser, Debug)]
#[command(name = "gfs", version, about = "Agent-oriented virtual Git workspace")]
struct Cli {
  /// The server's gRPC endpoint.
  #[arg(long, env = "GFS_ENDPOINT", default_value = "http://127.0.0.1:8431")]
  endpoint: String,

  /// The server's HTTP endpoint, used for blob reads.
  #[arg(
    long,
    env = "GFS_HTTP_ENDPOINT",
    default_value = "http://127.0.0.1:8430"
  )]
  http_endpoint: String,

  /// Bearer token.
  #[arg(long, env = "GFS_TOKEN", hide_env_values = true, default_value = "")]
  token: String,

  /// Where the `gfs-fuse` host listens. One host serves every mount on the
  /// machine; `gfs clone` and `gfs mount` start it on demand.
  #[arg(long, env = "GFS_HOST_SOCKET", global = true)]
  host_socket: Option<PathBuf>,

  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
  /// Print version and pinned build information.
  Version,

  /// Resolve a revision selector to a pinned commit.
  Resolve {
    /// Defaults to the repository of the workspace you are standing in.
    #[arg(long)]
    repo: Option<String>,
    /// A branch, tag, object ID or abbreviation of at least 7 characters,
    /// optionally followed by an ancestry walk: `HEAD~3`, `main^`, `abc1234^2`.
    rev: String,
    #[arg(long)]
    workspace: Option<PathBuf>,
  },

  /// List one directory of a snapshot.
  ///
  /// The path column is **root-relative**, in every position: listing the root
  /// prints `src`, listing `src` prints `src/flask`. At the root the two happen
  /// to coincide, which is what makes the rule easy to misread as "basename at
  /// the top, full path below" — recursing on that reading builds `src/src/…`.
  /// Root-relative is what `gfs cat` takes, so the output of one is the input of
  /// the other.
  Ls {
    /// Defaults to the repository of the workspace you are standing in.
    #[arg(long)]
    repo: Option<String>,
    /// Defaults to that workspace's pinned commit.
    #[arg(long)]
    rev: Option<String>,
    /// Path from the snapshot root. Empty lists the root. A path that is not a
    /// directory in this commit is an error, not an empty listing.
    #[arg(default_value = "")]
    path: String,
    #[arg(long, default_value_t = 1000)]
    page_size: u32,
    #[arg(long)]
    workspace: Option<PathBuf>,
  },

  /// Print one file's raw bytes.
  ///
  /// Raw: DESIGN.md section 12 documents that GFS applies no `.gitattributes`
  /// conversion, so these are the bytes the object database holds and not
  /// necessarily the bytes `git checkout` would write.
  Cat {
    /// Defaults to the repository of the workspace you are standing in.
    #[arg(long)]
    repo: Option<String>,
    /// Defaults to that workspace's pinned commit.
    #[arg(long)]
    rev: Option<String>,
    path: String,
    #[arg(long)]
    workspace: Option<PathBuf>,
  },

  /// Cache a repository on the gateway and mount it, the way `git clone` does.
  ///
  /// `gfs clone <url>` mounts at the directory `git clone` would have created;
  /// `gfs clone <url> <dir>` mounts at `<dir>`. Cloning a URL the gateway
  /// already holds is a **sync**, not a second copy: the gateway keeps one clone
  /// per upstream and every mount is a view onto it, so the second caller pays
  /// for a fetch rather than for a repository.
  Clone {
    /// The upstream URL, as `git clone` takes it.
    url: String,
    /// Where to mount. Defaults to the name `git clone` would choose.
    directory: Option<PathBuf>,
    /// Mount this revision instead of the upstream's default branch.
    #[arg(long)]
    rev: Option<String>,
    /// A credential for the upstream, used for this call and never stored.
    #[arg(long, env = "GFS_UPSTREAM_CREDENTIAL", hide_env_values = true)]
    credential: Option<String>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long)]
    allow_other: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024)]
    cache_quota: u64,
    #[arg(long, default_value_t = 1024 * 1024 * 1024)]
    overlay_quota: u64,
    #[arg(long)]
    foreground: bool,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
  },

  /// Mount a pinned commit as a workspace.
  Mount {
    #[arg(long)]
    repo: String,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    /// The path the job will use.
    #[arg(long)]
    workspace: PathBuf,
    /// Defaults to `<workspace>.gfs`.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Defaults to `$XDG_CACHE_HOME/gfs`.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Allow a different UID to read the mount. Needs `user_allow_other` in
    /// `/etc/fuse.conf` (ADR 0003), a privileged one-time host action.
    #[arg(long)]
    allow_other: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024)]
    cache_quota: u64,
    #[arg(long, default_value_t = 1024 * 1024 * 1024)]
    overlay_quota: u64,
    /// Run the daemon in this terminal instead of in the background.
    #[arg(long)]
    foreground: bool,
    /// How long to wait for the workspace to become usable.
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
  },

  /// Release the lease, unmount, and drop the workspace from its host.
  ///
  /// The host stays up: it may be serving other workspaces, and it is the thing
  /// the next `gfs clone` reuses.
  Unmount {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// The `gfs-fuse` host process, and the mounts it is serving.
  Daemon {
    #[command(subcommand)]
    command: DaemonCommand,
  },

  /// Everything about a mounted workspace.
  Inspect {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
  },

  /// Daemon and lease health. Exits non-zero when the lease is not renewing.
  Health {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
  },

  /// Re-resolve the revision and publish a new mount generation.
  ///
  /// Only for a clean workspace. Old file and directory handles keep reading the
  /// old generation until they close.
  Refresh {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// What the workspace has changed, from the overlay journal.
  ///
  /// Derived from the journal and not from a scan, so the cost is proportional to
  /// the edit set rather than to the repository (DESIGN.md section 8.5).
  Status {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Machine-readable output, for an orchestrator.
    #[arg(long)]
    json: bool,
  },

  /// Search the merged workspace: the pinned commit plus local edits.
  ///
  /// The server searches its index of the pinned commit and never hydrates the
  /// mount; the daemon searches only the files the overlay has changed. Exit
  /// codes follow ADR 0004: 0 matches, 1 none, 2 the search did not complete,
  /// 3 truncated, 4 a coverage gap under `--require-exhaustive`.
  Search {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// The pattern. A regex unless `--fixed-strings`.
    pattern: String,
    /// Limit the search to this path prefix.
    #[arg(default_value = "")]
    path: String,
    /// Treat the pattern as a literal string.
    #[arg(long, short = 'F')]
    fixed_strings: bool,
    #[arg(long, short = 'i')]
    ignore_case: bool,
    /// Include only paths matching this glob. Repeatable.
    #[arg(long, short = 'g')]
    glob: Vec<String>,
    /// Exclude paths matching this glob. Repeatable.
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long, short = 'B', default_value_t = 0)]
    before_context: u32,
    #[arg(long, short = 'A', default_value_t = 0)]
    after_context: u32,
    #[arg(long, default_value_t = 0)]
    max_results: u32,
    /// Cut a returned line to this many bytes. 0 uses the daemon's default,
    /// which is also the maximum. `rg --max-columns`, except that the line's
    /// first bytes are kept and marked rather than the line being suppressed.
    #[arg(long, default_value_t = 0)]
    max_columns: u32,
    /// Search files the workspace's ignore rules would skip.
    #[arg(long)]
    no_ignore: bool,
    /// Machine-readable output.
    #[arg(long)]
    json: bool,
    /// Turn any coverage gap into a failure (exit 4) rather than a warning.
    #[arg(long)]
    require_exhaustive: bool,
  },

  /// The workspace's change set as a patch, or the diff between two revisions.
  #[command(disable_help_flag = true)]
  Diff {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
  },

  /// One commit and what it changed, answered by the server.
  #[command(disable_help_flag = true)]
  Show {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
  },

  /// Who last changed each line of a file, answered by the server.
  #[command(disable_help_flag = true)]
  Blame {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
  },

  /// Write an atomic, checksummed export bundle.
  ///
  /// The bundle carries the pinned base commit, so a downstream applier can
  /// reject or three-way merge stale work rather than clobbering a branch that
  /// moved while the job ran.
  Export {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// The bundle directory. Replaced atomically if it already exists.
    #[arg(long)]
    bundle: PathBuf,
  },

  /// Install the `git` shim as `git` in a directory, and print that directory.
  ///
  /// The shim is a `PATH` measure, not a security boundary (DESIGN.md section
  /// 8.6): a tool that invokes Git by absolute path bypasses it and sees the
  /// documented limitations of the raw synthesized surface. Prepending the
  /// printed directory to `PATH` is what makes it effective, and M6.1 verifies
  /// that precedence inside the real agent image.
  InstallShim {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Defaults to `<state-dir>/bin`.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
  },

  /// Search the workspace with ripgrep's flags, without hydrating it.
  ///
  /// The same daemon call as `search`, spelled the way `rg` spells it. A flag
  /// `rg` has but this does not implement is refused by name; `--hydrate` runs
  /// real `rg` over the mount and accepts the cost.
  #[command(disable_help_flag = true)]
  Rg {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
  },

  /// Move this view to another branch, creating it with `-c`.
  ///
  /// The branch is created on the **gateway**, not locally: the gateway's mirror
  /// is the clone and this workspace is a view onto it. Unpushed branches live
  /// in the reserved ref namespace, because a branch under `refs/heads/` that
  /// upstream does not have is deleted by the next mirror fetch.
  Switch {
    /// The branch to move to.
    branch: String,
    /// Create the branch first, the way `git switch -c` does.
    #[arg(short = 'c', long)]
    create: bool,
    /// What the new branch starts from. Defaults to the current view.
    #[arg(long)]
    start_point: Option<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// Commit the workspace's changes to the gateway.
  ///
  /// The commit is made in the gateway's mirror, which *is* the clone, and this
  /// view then re-pins to it. There is no local-only commit and no staging area:
  /// the overlay journal already knows exactly what changed, and the commit is
  /// durable the moment it is made.
  Commit {
    /// The commit message.
    #[arg(short = 'm', long)]
    message: String,
    #[arg(long, env = "GIT_AUTHOR_NAME")]
    author_name: Option<String>,
    #[arg(long, env = "GIT_AUTHOR_EMAIL")]
    author_email: Option<String>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// Push a work branch to the real Git server.
  ///
  /// The gateway pushes outward with **your** credential, the way `git push`
  /// would, so upstream sees you and not the service.
  Push {
    /// The branch to push. Defaults to the one this view is on.
    branch: Option<String>,
    /// The upstream branch to create or update. Defaults to the same name.
    #[arg(long)]
    remote_branch: Option<String>,
    #[arg(long, env = "GFS_UPSTREAM_CREDENTIAL", hide_env_values = true)]
    credential: Option<String>,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// Retention leases, without a mount. Used by the development stack.
  #[command(subcommand)]
  Lease(LeaseCommand),
}

#[derive(Subcommand, Debug)]
enum LeaseCommand {
  /// Create a lease, pinning a commit.
  Create {
    #[arg(long)]
    repo: String,
    #[arg(long, default_value = "HEAD")]
    rev: String,
  },
  Renew {
    #[arg(long)]
    mount_id: String,
    #[arg(long, hide_env_values = true)]
    capability: String,
  },
  Release {
    #[arg(long)]
    mount_id: String,
    #[arg(long, hide_env_values = true)]
    capability: String,
  },
}

#[derive(Subcommand, Debug)]
enum DaemonCommand {
  /// Which host is running, and what it is serving. Exits non-zero when none is.
  Status {
    #[arg(long)]
    json: bool,
  },
  /// Unmount every workspace this host serves, then stop it.
  Stop,
}

type Client = v1::snapshot_service_client::SnapshotServiceClient<tonic::transport::Channel>;
type RepoClient = v1::repository_service_client::RepositoryServiceClient<tonic::transport::Channel>;

async fn channel(cli: &Cli) -> Result<tonic::transport::Channel> {
  tonic::transport::Endpoint::from_shared(cli.endpoint.clone())
    .context("invalid endpoint")?
    .connect()
    .await
    .with_context(|| format!("connecting to {}", cli.endpoint))
}

async fn connect(cli: &Cli) -> Result<Client> {
  Ok(Client::new(channel(cli).await?))
}

/// The write half of the API: clone, branch, commit, push.
async fn connect_repository(cli: &Cli) -> Result<RepoClient> {
  Ok(RepoClient::new(channel(cli).await?))
}

/// Attach the bearer token to a request.
fn authed<T>(cli: &Cli, message: T) -> Result<tonic::Request<T>> {
  let mut request = tonic::Request::new(message);
  if !cli.token.is_empty() {
    request.metadata_mut().insert(
      "authorization",
      format!("Bearer {}", cli.token)
        .parse()
        .context("token is not a valid header value")?,
    );
  }
  Ok(request)
}

/// Resolve a selector once, so every later call names the commit.
///
/// DESIGN.md section 6.2: a branch name is only a selector. Resolving once per
/// command means a branch that moves mid-listing cannot mix two generations into one
/// output.
async fn resolve(cli: &Cli, client: &mut Client, repo: &str, rev: &str) -> Result<String> {
  Ok(
    client
      .resolve_revision(authed(
        cli,
        v1::ResolveRevisionRequest {
          repository_id: repo.to_owned(),
          revision_selector: rev.to_owned(),
        },
      )?)
      .await?
      .into_inner()
      .commit_oid,
  )
}

/// What repository and revision a snapshot command should read.
///
/// `ls`, `cat` and `resolve` talk to the server directly and used to *require*
/// `--repo`, which meant an agent standing inside a mount had to find and retype
/// the repository id that `gfs inspect` was printing two lines away — while
/// `status`, `diff` and `log` all inferred it. The rule is now the same
/// everywhere: an explicit flag wins, and its absence means "the workspace I am
/// standing in".
///
/// The revision half matters as much, and is the subtler half. On the server
/// `HEAD` is the repository's **default branch**; in a workspace it means the
/// **pinned commit**, and after `gfs switch -c` those are different commits. So
/// when the repository was inferred from a workspace, a bare `HEAD` — or the
/// base of an expression like `HEAD~3` — is replaced by the pin before the
/// request goes out. With an explicit `--repo` nothing is substituted: the
/// caller named a repository that may not be this one, and quietly answering
/// about a different commit than the one they wrote is the failure mode this
/// exists to prevent.
fn snapshot_target(
  repo: &Option<String>,
  rev: &Option<String>,
  workspace: &Option<PathBuf>,
) -> Result<(String, String)> {
  if let (Some(repo), Some(rev)) = (repo, rev) {
    return Ok((repo.clone(), rev.clone()));
  }
  let report = inspect_here(workspace)?;
  let rev = match rev {
    None => report.commit.clone(),
    Some(rev) if repo.is_none() => substitute_pin(rev, &report.commit),
    Some(rev) => rev.clone(),
  };
  Ok((repo.clone().unwrap_or(report.repository_id), rev))
}

/// Rewrite a leading `HEAD` to the pinned commit, keeping any `~n`/`^n` suffix.
///
/// The same rule the daemon applies for `gfs log` and `gfs show`, applied here
/// so the direct-to-server commands agree with them.
fn substitute_pin(rev: &str, pin: &str) -> String {
  let split = rev.find(['~', '^']).unwrap_or(rev.len());
  let (base, suffix) = rev.split_at(split);
  if base == "HEAD" {
    format!("{pin}{suffix}")
  } else {
    rev.to_owned()
  }
}

fn inspect_here(workspace: &Option<PathBuf>) -> Result<gfs_mount::control::MountReport> {
  let (_workspace, state_dir) = locate(workspace, &None).context(
    "no --repo was given and this is not a GFS workspace, so there is nothing to \
     infer it from",
  )?;
  match call(&state_dir, &Request::Inspect)? {
    Response::Inspect(report) => Ok(*report),
    _ => bail!("the daemon answered an inspect request with something else"),
  }
}

/// `<workspace>.gfs` unless told otherwise.
fn default_cache_dir() -> PathBuf {
  std::env::var_os("XDG_CACHE_HOME")
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    .unwrap_or_else(std::env::temp_dir)
    .join("gfs")
}

/// Locate `gfs-fuse`.
///
/// Next to this binary first: a development build and an installed package both
/// put the two together, and picking up a *different* daemon from `PATH` than the
/// CLI it shipped with is a version-skew bug that only appears at runtime.
fn daemon_binary() -> PathBuf {
  sibling_binary("gfs-fuse", "GFS_DAEMON")
}

fn shim_binary() -> PathBuf {
  sibling_binary("gfs-git-shim", "GFS_GIT_SHIM")
}

/// The daemon's plan as the wire's changes.
///
/// Paths and content travel base64url over the control socket because that is a
/// JSON protocol and neither a Git path nor a blob is required to be UTF-8.
fn planned_changes(plan: &gfs_mount::control::CommitPlan) -> Result<Vec<v1::FileChange>> {
  use gfs_mount::control::PlannedChange;
  use gfs_types::path::b64url_decode;

  plan
    .changes
    .iter()
    .map(|change| {
      Ok(match change {
        PlannedChange::Upsert {
          path,
          mode,
          content_b64url,
        } => v1::FileChange {
          path: b64url_decode(path).context("decoding a changed path")?,
          // `Modified` for both create and replace: a tree upsert is one
          // operation, and the server does not need to know which it was.
          kind: v1::ChangeKind::Modified as i32,
          mode: *mode,
          content: b64url_decode(content_b64url).context("decoding changed content")?,
        },
        PlannedChange::Delete { path } => v1::FileChange {
          path: b64url_decode(path).context("decoding a deleted path")?,
          kind: v1::ChangeKind::Deleted as i32,
          mode: 0,
          content: Vec::new(),
        },
      })
    })
    .collect()
}

fn decode_paths(encoded: &[String]) -> Result<Vec<Vec<u8>>> {
  encoded
    .iter()
    .map(|p| gfs_types::path::b64url_decode(p).context("decoding a deleted directory"))
    .collect()
}

/// Run one of the argv-parsing subcommands and leave with its exit code.
///
/// Never returns through `Result`, for the reason `search` does not either:
/// anyhow exits 1 for an error, and ADR 0004 gives 1 the meaning "complete, and
/// the thing genuinely is not there". Reporting an unreachable daemon that way
/// would tell an agent a symbol does not exist. Every failure here is instead
/// exit 2, "no answer".
fn run_tool(name: &str, result: Result<i32>) -> ! {
  match result {
    Ok(code) => std::process::exit(code),
    Err(e) => {
      eprintln!("gfs {name}: {e:#}");
      std::process::exit(2);
    }
  }
}

fn sibling_binary(name: &str, override_var: &str) -> PathBuf {
  if let Some(explicit) = std::env::var_os(override_var) {
    return PathBuf::from(explicit);
  }
  if let Ok(current) = std::env::current_exe() {
    if let Some(sibling) = current.parent().map(|d| d.join(name)) {
      if sibling.is_file() {
        return sibling;
      }
    }
  }
  PathBuf::from(name)
}

/// Where this invocation's host daemon listens.
///
/// `--foreground` gets a private socket inside the state directory rather than
/// the shared one. That is what makes the flag mean what it says: a debugging run
/// that attached itself to whatever host happened to be running would neither show
/// that host's logs in this terminal nor stop it on Ctrl-C.
fn host_socket(cli: &Cli, state_dir: &std::path::Path, foreground: bool) -> PathBuf {
  if foreground {
    return state_dir.join("host.sock");
  }
  cli
    .host_socket
    .clone()
    .unwrap_or_else(gfs_mount::host::default_socket)
}

/// Start a host on `socket`.
///
/// In the background its stderr goes to a log file, because the host outlives
/// this process and none of its descriptors may stay tied to this terminal: a
/// backgrounded daemon holding the inherited stderr keeps the write end of a pipe
/// open, so `gfs clone | tee` never sees EOF and appears to hang long after the
/// command finished. In the foreground the terminal *is* the log, which is the
/// whole reason to ask for it.
fn spawn_host(
  cli: &Cli,
  socket: &std::path::Path,
  foreground: bool,
) -> Result<std::process::Child> {
  let mut command = std::process::Command::new(daemon_binary());
  command
    .arg("--socket")
    .arg(socket)
    .arg("--endpoint")
    .arg(&cli.endpoint)
    .arg("--http-endpoint")
    .arg(&cli.http_endpoint)
    .arg("--token")
    .arg(&cli.token);

  if !foreground {
    let log_path = host_log_path(socket);
    if let Some(parent) = log_path.parent() {
      std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let log = std::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&log_path)
      .with_context(|| format!("opening {}", log_path.display()))?;
    command
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::from(log));
  }

  command.spawn().with_context(|| {
    format!(
      "starting {} (set GFS_DAEMON if it is elsewhere)",
      daemon_binary().display()
    )
  })
}

/// Wait for a just-started host to bind its socket.
///
/// A child that exited is checked against the socket once more before it is
/// called a failure: racing callers are expected, and the loser of the `flock`
/// exits cleanly while the winner is listening.
fn await_host(
  child: &mut std::process::Child,
  socket: &std::path::Path,
  timeout_seconds: u64,
) -> Result<()> {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
  loop {
    if gfs_mount::control::is_live(socket) {
      return Ok(());
    }
    if let Some(status) = child.try_wait().context("waiting for gfs-fuse")? {
      if gfs_mount::control::is_live(socket) {
        return Ok(());
      }
      bail!(
        "gfs-fuse exited before the host was listening ({status}); see {}",
        host_log_path(socket).display()
      );
    }
    if std::time::Instant::now() >= deadline {
      let _ = child.kill();
      bail!("the GFS host was not listening within {timeout_seconds}s");
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
  }
}

/// Start a background host unless one is already listening.
fn ensure_host(cli: &Cli, socket: &std::path::Path, timeout_seconds: u64) -> Result<()> {
  if gfs_mount::control::is_live(socket) {
    return Ok(());
  }
  let mut child = spawn_host(cli, socket, false)?;
  await_host(&mut child, socket, timeout_seconds)
}

/// Beside the socket, because that is what identifies a host.
fn host_log_path(socket: &std::path::Path) -> PathBuf {
  socket.with_extension("log")
}

fn do_mount(cli: &Cli, args: MountArgs) -> Result<()> {
  let state_dir = state_dir_for(&args.workspace, &args.state_dir);
  let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
  std::fs::create_dir_all(&state_dir)
    .with_context(|| format!("creating {}", state_dir.display()))?;

  let socket = host_socket(cli, &state_dir, args.foreground);
  // In the foreground the host is this command's child, so its exit is what ends
  // the wait at the bottom. In the background it is shared and may already exist.
  let mut foreground_host = if args.foreground {
    let mut child = spawn_host(cli, &socket, true)?;
    await_host(&mut child, &socket, args.timeout_seconds)?;
    Some(child)
  } else {
    ensure_host(cli, &socket, args.timeout_seconds)?;
    None
  };

  let request = gfs_mount::control::MountRequest {
    state_format_version: gfs_types::STATE_FORMAT_VERSION,
    workspace: args.workspace.clone(),
    state_dir: state_dir.clone(),
    cache_dir,
    repository_id: args.repo.clone(),
    revision_selector: args.rev.clone(),
    cache_quota_bytes: args.cache_quota,
    overlay_quota_bytes: args.overlay_quota,
    allow_other: args.allow_other,
    fuse_threads: None,
    grpc_endpoint: Some(cli.endpoint.clone()),
    http_endpoint: Some(cli.http_endpoint.clone()),
    token: Some(cli.token.clone()),
  };
  let response = call_host(
    &socket,
    &gfs_mount::control::HostRequest::CreateMount(Box::new(request)),
  )?;
  let gfs_mount::control::HostResponse::Mounted(report) = response else {
    bail!("the host answered a mount request with something else");
  };
  print_report(&report);
  println!("host       {}", socket.display());
  println!("log        {}", host_log_path(&socket).display());

  if let Some(child) = &mut foreground_host {
    // Ctrl-C reaches the host through the process group, and the host's SIGINT
    // handler unmounts before it exits. Waiting on the child rather than polling
    // the socket means this command's exit status is the host's.
    println!("\nthe workspace is ready; press Ctrl-C to unmount and stop the host");
    let status = child.wait().context("waiting for gfs-fuse")?;
    if !status.success() {
      bail!("gfs-fuse exited with {status}");
    }
  }
  Ok(())
}

struct MountArgs {
  repo: String,
  rev: String,
  workspace: PathBuf,
  state_dir: Option<PathBuf>,
  cache_dir: Option<PathBuf>,
  allow_other: bool,
  cache_quota: u64,
  overlay_quota: u64,
  foreground: bool,
  timeout_seconds: u64,
}

fn print_report(report: &gfs_mount::control::MountReport) {
  println!("workspace  {}", report.workspace);
  println!("repository {}", report.repository_id);
  // The pinned commit, shown because it is the thing that matters: the branch
  // name was only a selector, and PLAN.md M2.1 requires the CLI to show it.
  println!("commit     {}", report.commit);
  println!("ref        {}", report.ref_name.as_deref().unwrap_or("-"));
  println!("mount      {}", report.mount_id);
  println!("generation {}", report.generation);
  println!("publication {}", report.publication);
  println!("state      {}", report.state_dir);
  println!("owner uid  {}", report.owner_uid);
  println!(
    "snapshot   {}.{:09}",
    report.snapshot_time.secs, report.snapshot_time.nanos
  );
  println!("lease      {:?}", report.health.state);
  println!(
    "expires    {} (grace to {})",
    report.health.lease_expiry.secs, report.health.grace_deadline.secs
  );
  println!("read-only  {}", report.read_only);
  println!(
    "hydration  {} blobs, {} bytes, {} cache hits",
    report.cache.fetches, report.cache.bytes_fetched, report.cache.hits
  );
  // The budget line names the remedy, because the EDQUOT the job saw could not
  // (a FUSE reply is an errno, with no room for prose -- ADR 0009).
  if report.budget.limit_bytes > 0 {
    println!(
      "budget     {} of {} bytes used ({} unique blobs), {} refusals",
      report.budget.charged_bytes,
      report.budget.limit_bytes,
      report.budget.unique_blobs,
      report.budget.refusals
    );
    if report.budget.refusals > 0 {
      println!(
        "           reads failed with EDQUOT: prefer `gfs rg` over scanning the \
         tree (`git ls-files` and `git log` are free), or raise the host's \
         --hydration-budget"
      );
    }
  } else {
    println!("budget     unlimited");
  }
  // The overlay line answers the two questions an operator actually has about a
  // running job: has it changed anything, and how close is it to its quota.
  println!(
    "overlay    {} changed paths ({} deleted), {} of {} bytes used",
    report.overlay.entries - report.overlay.whiteouts,
    report.overlay.whiteouts,
    report.overlay.local_bytes,
    report.overlay.quota_bytes
  );
}

/// One `gfs ls` line. The path column is **root-relative** in every position,
/// and escaped, because a Git path is bytes and need not be UTF-8.
fn print_ls_entry(mode: u32, size: u64, oid: &str, path: &[u8]) {
  println!(
    "{:06o} {:>10} {}  {}",
    mode,
    size,
    oid,
    gfs_types::BytePath::new(path.to_vec()).escaped()
  );
}

/// Decode a base64url field the control socket carried.
fn decode(encoded: &str) -> Result<Vec<u8>> {
  gfs_types::path::b64url_decode(encoded)
    .map_err(|e| anyhow::anyhow!("the daemon returned an undecodable value: {}", e.message))
}

/// `git status --short`'s shape, because that is what an agent reading this has
/// already learned to parse.
fn print_status(report: &gfs_mount::control::StatusReport) {
  use std::io::Write;
  // The work branch wins when there is one. After `gfs switch` the view is
  // pinned to a bare commit -- the reserved namespace cannot be named as a
  // revision -- so `ref_name` is `None`, and reporting "Detached" there would
  // tell a caller their branch had been lost when it is exactly where they put
  // it.
  match (&report.work_branch, &report.ref_name) {
    (Some(branch), _) => println!("On {} at {}", branch, report.base_commit),
    (None, Some(name)) => println!("On {} at {}", name, report.base_commit),
    (None, None) => println!("Detached at {}", report.base_commit),
  }
  if report.status.is_clean() {
    println!("nothing to commit, working tree clean");
    return;
  }
  let stdout = std::io::stdout();
  let mut out = stdout.lock();
  for change in &report.status.changes {
    let _ = write!(out, "{} ", change.kind.porcelain() as char);
    if let Some(from) = &change.from {
      let _ = out.write_all(from.as_bytes());
      let _ = out.write_all(b" -> ");
    }
    let _ = out.write_all(change.path.as_bytes());
    let _ = out.write_all(b"\n");
  }
  for path in &report.status.directory_deletions {
    // Named separately because the patch cannot carry them: the journal knows
    // the directory went but not which base files were under it.
    let _ = write!(out, "D  ");
    let _ = out.write_all(path.as_bytes());
    let _ = out.write_all(b"/ (directory; not expanded)\n");
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  let cli = Cli::parse();

  match &cli.command {
    Command::Version => {
      println!("gfs {}", env!("CARGO_PKG_VERSION"));
      println!("api-version {}", gfs_types::API_VERSION);
      println!("state-format-version {}", gfs_types::STATE_FORMAT_VERSION);
    }

    Command::Resolve {
      repo,
      rev,
      workspace,
    } => {
      let (repo, rev) = snapshot_target(repo, &Some(rev.clone()), workspace)?;
      let mut client = connect(&cli).await?;
      let resp = client
        .resolve_revision(authed(
          &cli,
          v1::ResolveRevisionRequest {
            repository_id: repo.clone(),
            revision_selector: rev.clone(),
          },
        )?)
        .await?
        .into_inner();
      println!("commit     {}", resp.commit_oid);
      println!("tree       {}", resp.tree_oid);
      println!("ref        {}", resp.ref_name.as_deref().unwrap_or("-"));
      println!("refversion {}", resp.ref_version);
      if let Some(t) = resp.snapshot_time {
        println!("snapshot   {}.{:09}", t.secs, t.nanos);
      }
    }

    Command::Ls {
      repo,
      rev,
      path,
      page_size,
      workspace,
    } => {
      // Through the daemon when the repository was inferred, because only the
      // mount's capability opens a commit on an unpushed work branch. With an
      // explicit `--repo` this stays the direct-to-server call it is documented
      // to be.
      if repo.is_none() {
        let (_workspace, state_dir) = locate(workspace, &None)?;
        let Response::Ls(report) = call(
          &state_dir,
          &Request::Ls {
            rev: rev.clone(),
            path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
            page_size: *page_size,
          },
        )?
        else {
          bail!("the daemon answered a list request with something else");
        };
        for entry in &report.entries {
          print_ls_entry(
            entry.mode,
            entry.size,
            &entry.oid,
            &decode(&entry.path_b64url)?,
          );
        }
        return Ok(());
      }

      let (repo, rev) = snapshot_target(repo, rev, workspace)?;
      let mut client = connect(&cli).await?;
      let commit = resolve(&cli, &mut client, &repo, &rev).await?;

      let mut token = Vec::new();
      loop {
        let page = client
          .list_directory(authed(
            &cli,
            v1::ListDirectoryRequest {
              repository_id: repo.to_owned(),
              commit_oid: commit.clone(),
              path: path.as_bytes().to_vec(),
              page_token: token.clone(),
              page_size: *page_size,
              authorization: None,
              want_blob_tickets: false,
            },
          )?)
          .await?
          .into_inner();
        for e in &page.entries {
          print_ls_entry(e.mode, e.size, &e.oid, &e.path);
        }
        if page.next_page_token.is_empty() {
          break;
        }
        token = page.next_page_token;
      }
    }

    Command::Cat {
      repo,
      rev,
      path,
      workspace,
    } => {
      // Through the daemon when the repository was inferred; see `Ls`.
      if repo.is_none() {
        let (_workspace, state_dir) = locate(workspace, &None)?;
        let Response::Cat { content_b64url, .. } = call(
          &state_dir,
          &Request::Cat {
            rev: rev.clone(),
            path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
          },
        )?
        else {
          bail!("the daemon answered a cat request with something else");
        };
        // Written as bytes: file content is not guaranteed UTF-8, and a lossy
        // conversion would corrupt a binary file.
        std::io::Write::write_all(&mut std::io::stdout(), &decode(&content_b64url)?)?;
        return Ok(());
      }

      let (repo, rev) = snapshot_target(repo, rev, workspace)?;
      let mut client = connect(&cli).await?;
      let commit = resolve(&cli, &mut client, &repo, &rev).await?;
      let entry = client
        .get_entry(authed(
          &cli,
          v1::GetEntryRequest {
            repository_id: repo.clone(),
            commit_oid: commit,
            path: path.as_bytes().to_vec(),
            authorization: None,
            want_blob_ticket: true,
          },
        )?)
        .await?
        .into_inner()
        .entry
        .context("server returned no entry")?;

      // Read over the HTTP blob endpoint rather than gRPC, because that is the path a
      // real client uses: it is the one that supports ranges and ETag revalidation.
      let ticket = entry
        .blob_ticket
        .context("server issued no blob ticket for this entry")?;
      let url = format!(
        "{}/v1/repos/{repo}/blobs/{}?ticket={ticket}",
        cli.http_endpoint.trim_end_matches('/'),
        entry.oid
      );
      let bytes = http_get(&url, &cli.token).await?;
      use std::io::Write;
      // Written as bytes, not as a string: file content is not guaranteed UTF-8, and
      // `println!` of a lossy conversion would corrupt a binary file.
      std::io::stdout().write_all(&bytes)?;
    }

    Command::Clone {
      url,
      directory,
      rev,
      credential,
      state_dir,
      cache_dir,
      allow_other,
      cache_quota,
      overlay_quota,
      foreground,
      timeout_seconds,
    } => {
      let mut client = connect_repository(&cli).await?;
      let response = client
        .clone_repository(authed(
          &cli,
          v1::CloneRepositoryRequest {
            upstream_url: url.clone(),
            credential: credential.clone().unwrap_or_default(),
          },
        )?)
        .await?
        .into_inner();

      // The gateway's name for the directory is a fallback, not an override: a
      // caller who named one meant it.
      let workspace = directory
        .clone()
        .unwrap_or_else(|| PathBuf::from(&response.directory));
      if response.created {
        eprintln!(
          "cloned {} into the gateway as {}",
          url, response.repository_id
        );
      } else {
        eprintln!(
          "{} is already on the gateway as {}; synced",
          url, response.repository_id
        );
      }

      let rev = rev
        .clone()
        .unwrap_or_else(|| response.default_branch.clone());
      tokio::task::block_in_place(|| {
        do_mount(
          &cli,
          MountArgs {
            repo: response.repository_id.clone(),
            rev,
            workspace,
            state_dir: state_dir.clone(),
            cache_dir: cache_dir.clone(),
            allow_other: *allow_other,
            cache_quota: *cache_quota,
            overlay_quota: *overlay_quota,
            foreground: *foreground,
            timeout_seconds: *timeout_seconds,
          },
        )
      })?;
    }

    Command::Mount {
      repo,
      rev,
      workspace,
      state_dir,
      cache_dir,
      allow_other,
      cache_quota,
      overlay_quota,
      foreground,
      timeout_seconds,
    } => {
      // Blocking work, deliberately: starting a child process and waiting for a
      // file to appear gains nothing from a runtime, and doing it inside one
      // would block a worker thread anyway.
      tokio::task::block_in_place(|| {
        do_mount(
          &cli,
          MountArgs {
            repo: repo.clone(),
            rev: rev.clone(),
            workspace: workspace.clone(),
            state_dir: state_dir.clone(),
            cache_dir: cache_dir.clone(),
            allow_other: *allow_other,
            cache_quota: *cache_quota,
            overlay_quota: *overlay_quota,
            foreground: *foreground,
            timeout_seconds: *timeout_seconds,
          },
        )
      })?;
    }

    Command::Unmount {
      workspace,
      state_dir,
    } => {
      let (workspace, state_dir) = locate(&workspace.clone(), state_dir)?;
      match call(&state_dir, &Request::Unmount)? {
        Response::Unmounted => println!("unmounted {}", workspace.display()),
        other => bail!("unexpected daemon response: {other:?}"),
      }
    }

    Command::Daemon { command } => {
      use gfs_mount::control::{HostRequest, HostResponse};

      let socket = cli
        .host_socket
        .clone()
        .unwrap_or_else(gfs_mount::host::default_socket);
      match command {
        DaemonCommand::Status { json } => {
          // Not started on demand: "is a host running" is the question, and
          // starting one to answer it would make the answer always yes.
          if !gfs_mount::control::is_live(&socket) {
            bail!("no GFS host is listening on {}", socket.display());
          }
          let HostResponse::Info(info) = call_host(&socket, &HostRequest::Info)? else {
            bail!("the host answered an info request with something else");
          };
          let HostResponse::Mounts { mounts } = call_host(&socket, &HostRequest::ListMounts)?
          else {
            bail!("the host answered a list request with something else");
          };
          if *json {
            println!(
              "{}",
              serde_json::to_string_pretty(&serde_json::json!({
                "host": info,
                "mounts": mounts,
              }))?
            );
          } else {
            println!("socket     {}", info.socket.display());
            println!("pid        {}", info.pid);
            println!("version    {}", info.version);
            println!("endpoint   {}", info.grpc_endpoint);
            println!("log        {}", host_log_path(&socket).display());
            println!("mounts     {}", mounts.len());
            for mount in &mounts {
              println!(
                "  {}  {}  {}  gen {}  {:?}",
                mount.workspace.display(),
                mount.repository_id,
                mount.commit,
                mount.generation,
                mount.health
              );
            }
          }
        }
        DaemonCommand::Stop => {
          if !gfs_mount::control::is_live(&socket) {
            bail!("no GFS host is listening on {}", socket.display());
          }
          let HostResponse::ShuttingDown = call_host(&socket, &HostRequest::Shutdown)? else {
            bail!("the host answered a shutdown request with something else");
          };
          println!("stopping the host on {}", socket.display());
        }
      }
    }

    Command::Inspect {
      workspace,
      state_dir,
      json,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Inspect(report) = call(&state_dir, &Request::Inspect)? else {
        bail!("the daemon answered an inspect request with something else");
      };
      if *json {
        println!("{}", serde_json::to_string_pretty(&report)?);
      } else {
        print_report(&report);
      }
    }

    Command::Health {
      workspace,
      state_dir,
      json,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Health(health) = call(&state_dir, &Request::Health)? else {
        bail!("the daemon answered a health request with something else");
      };
      if *json {
        println!("{}", serde_json::to_string_pretty(&health)?);
      } else {
        println!("state      {:?}", health.state);
        println!("failures   {}", health.consecutive_failures);
        println!("expires    {}", health.lease_expiry.secs);
        println!("grace to   {}", health.grace_deadline.secs);
        println!("heartbeat  {}s", health.heartbeat_interval_secs);
        if let Some(error) = &health.last_error {
          println!("last error {error}");
        }
      }
      // A non-zero exit so a probe or a shell script does not have to parse the
      // output to learn that a lease is failing to renew.
      if !health.is_healthy() {
        std::process::exit(1);
      }
    }

    Command::Refresh {
      workspace,
      state_dir,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Refresh(report) = call(&state_dir, &Request::Refresh)? else {
        bail!("the daemon answered a refresh request with something else");
      };
      println!(
        "generation {} -> {}",
        report.previous_generation, report.generation
      );
      println!("commit     {}", report.commit);
      if report.unchanged {
        println!("           (unchanged; the selector still resolves to the same commit)");
      }
    }

    Command::Status {
      workspace,
      state_dir,
      json,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Status(report) = call(&state_dir, &Request::Status)? else {
        bail!("the daemon answered a status request with something else");
      };
      if *json {
        println!("{}", serde_json::to_string_pretty(&report)?);
      } else {
        print_status(&report);
      }
      // Non-zero for a dirty workspace would collide with `git status`'s own
      // convention of exiting 0 either way, so this exits 0 and says so in the
      // output instead.
    }

    Command::Search {
      workspace,
      state_dir,
      pattern,
      path,
      fixed_strings,
      ignore_case,
      glob,
      exclude,
      before_context,
      after_context,
      max_results,
      max_columns,
      no_ignore,
      json,
      require_exhaustive,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let request = gfs_mount::search::SearchRequest {
        pattern: pattern.clone(),
        literal: *fixed_strings,
        case_insensitive: *ignore_case,
        scope: path.as_bytes().to_vec(),
        include_globs: glob.clone(),
        exclude_globs: exclude.clone(),
        context_before: *before_context,
        context_after: *after_context,
        max_results: *max_results,
        max_line_bytes: *max_columns,
        search_ignored: *no_ignore,
      };
      // A failed search must not leave by the `Result` path. `main` returns
      // `Result`, and anyhow exits 1 for an error — which ADR 0004 defines as
      // "complete, and the pattern genuinely does not occur". A server that is
      // unreachable, unauthorized, or still building an index would therefore
      // tell an agent the symbol does not exist. Exit 2 is the code for "no
      // answer", and every failure here is one.
      let report = match call(&state_dir, &Request::Search(Box::new(request))) {
        Ok(Response::Search(report)) => report,
        Ok(_) => {
          eprintln!("gfs: the daemon answered a search request with something else");
          std::process::exit(2);
        }
        Err(e) => {
          eprintln!("gfs: {e:#}");
          std::process::exit(2);
        }
      };

      if *json {
        println!("{}", search_output::to_json(&report)?);
      } else {
        let mut out = std::io::stdout().lock();
        search_output::print_text(&report, &mut out)?;
        std::io::Write::flush(&mut out)?;
        search_output::print_diagnostics(&report, &mut std::io::stderr())?;
      }
      // The exit code *is* the contract, so it is set explicitly rather than
      // left to `Result`'s 0-or-1. An agent that cannot tell exit 1 from exit 3
      // concludes a symbol does not exist when the query was merely cut short.
      std::process::exit(search_output::exit_code(&report, *require_exhaustive));
    }

    Command::Export {
      workspace,
      state_dir,
      bundle,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let bundle = std::path::absolute(bundle)?;
      let Response::Export(report) = call(&state_dir, &Request::Export { bundle })? else {
        bail!("the daemon answered an export request with something else");
      };
      println!("bundle     {}", report.bundle.display());
      println!("changes    {}", report.changes);
      println!("patch      {} bytes", report.patch_bytes);
      println!("content    {} bytes", report.content_bytes);
      println!("manifest   sha256:{}", report.manifest_sha256);
    }

    Command::InstallShim {
      workspace,
      state_dir,
      bin_dir,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let bin_dir = bin_dir.clone().unwrap_or_else(|| state_dir.join("bin"));
      std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("creating {}", bin_dir.display()))?;
      // Absolute, because the printed directory is meant to be prepended to
      // `PATH`, and a relative entry stops resolving the moment anything changes
      // directory -- which an agent does constantly.
      let bin_dir = std::path::absolute(&bin_dir).unwrap_or(bin_dir);

      let shim = shim_binary();
      if !shim.is_file() {
        bail!(
          "cannot find {} (set GFS_GIT_SHIM to its path)",
          shim.display()
        );
      }
      let shim = std::path::absolute(&shim).unwrap_or(shim);
      let link = bin_dir.join("git");
      // Replaced rather than skipped when it exists: an upgraded package must
      // not leave the previous release's shim in place.
      let _ = std::fs::remove_file(&link);
      std::os::unix::fs::symlink(&shim, &link)
        .with_context(|| format!("linking {}", link.display()))?;

      println!("{}", bin_dir.display());
      eprintln!("prepend that directory to PATH so `git` resolves to the shim first");
    }

    Command::Switch {
      branch,
      create,
      start_point,
      workspace,
      state_dir,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      // The repository this view is on, read from the daemon rather than asked
      // for: a caller standing in a workspace should not have to name it again.
      let Response::Inspect(report) = call(&state_dir, &Request::Inspect)? else {
        bail!("the daemon answered an inspect request with something else");
      };

      // Ask about the workspace *before* creating anything on the gateway.
      //
      // `CreateBranch` is durable and the daemon's own clean-workspace check
      // comes after it, so a `switch -c` over a dirty workspace used to create
      // the branch and then refuse the switch. The caller was left on the old
      // commit with a branch that existed, and every retry after cleaning up
      // failed with "already exists" — a state neither `switch` nor `switch -c`
      // could get out of.
      //
      // The daemon still makes the same check, and it is the authority: this one
      // races nothing important, because losing the race only means creating a
      // branch the switch then declines, which is where we started. What it buys
      // is that the *ordinary* dirty-workspace path no longer leaves a ref
      // behind.
      if *create {
        let Response::Status(status) = call(&state_dir, &Request::Status)? else {
          bail!("the daemon answered a status request with something else");
        };
        if !status.status.is_clean() {
          bail!(
            "the workspace has local changes; commit or discard them before switching \
             (no branch was created)"
          );
        }
      }

      let mut client = connect_repository(&cli).await?;
      let created = if *create {
        Some(
          client
            .create_branch(authed(
              &cli,
              v1::CreateBranchRequest {
                repository_id: report.repository_id.clone(),
                branch: branch.clone(),
                // Defaults to what this view is pinned to, so `-c` branches
                // from where the caller is standing, as `git switch -c` does.
                start_point: start_point.clone().unwrap_or_else(|| report.commit.clone()),
                authorization: None,
              },
            )?)
            .await?
            .into_inner(),
        )
      } else {
        None
      };

      // The view is re-pinned to a *commit*, not to the branch name: ADR 0006
      // forbids naming the reserved namespace as a revision, and a work branch
      // lives there. The branch travels alongside so reports can name it.
      let (selector, work_branch) = match &created {
        Some(response) => (response.commit_oid.clone(), Some(branch.clone())),
        // Without `-c` this is an ordinary revision, so the daemon resolves it
        // itself and the view is not on a work branch.
        None => (branch.clone(), None),
      };

      let Response::Refresh(refresh) = call(
        &state_dir,
        &Request::Switch {
          selector,
          branch: work_branch,
        },
      )?
      else {
        bail!("the daemon answered a switch request with something else");
      };
      if *create {
        println!("switched to a new branch {branch}");
      } else {
        println!("switched to {branch}");
      }
      println!("commit     {}", refresh.commit);
      println!("generation {}", refresh.generation);
    }

    Command::Commit {
      message,
      author_name,
      author_email,
      workspace,
      state_dir,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Inspect(report) = call(&state_dir, &Request::Inspect)? else {
        bail!("the daemon answered an inspect request with something else");
      };
      let Response::CommitPlan(plan) = call(&state_dir, &Request::CommitPlan)? else {
        bail!("the daemon answered a commit-plan request with something else");
      };
      let Some(branch) = plan.work_branch.clone() else {
        bail!(
          "this workspace is not on a work branch, so there is nowhere to commit. \
           Run `gfs switch -c <branch>` first."
        );
      };

      let mut client = connect_repository(&cli).await?;
      let response = client
        .commit_changes(authed(
          &cli,
          v1::CommitChangesRequest {
            repository_id: report.repository_id.clone(),
            base_commit_oid: plan.base_commit.clone(),
            branch,
            message: message.clone(),
            author_name: author_name.clone().unwrap_or_default(),
            author_email: author_email.clone().unwrap_or_default(),
            changes: planned_changes(&plan)?,
            authorization: None,
            deleted_directories: decode_paths(&plan.deleted_directories)?,
          },
        )?)
        .await?
        .into_inner();

      // Only now does the view move. If this call fails the commit still exists
      // on the gateway -- it is durable the moment it is made -- and `gfs switch`
      // to the branch recovers, so nothing is lost by the two steps not being
      // one transaction.
      let Response::Refresh(refresh) = call(
        &state_dir,
        &Request::AdoptCommit {
          commit: response.commit_oid.clone(),
        },
      )?
      else {
        bail!("the daemon answered an adopt request with something else");
      };

      println!("committed  {}", response.commit_oid);
      println!("tree       {}", response.tree_oid);
      println!("branch     {}", response.ref_name);
      println!("generation {}", refresh.generation);
    }

    Command::Push {
      branch,
      remote_branch,
      credential,
      force,
      workspace,
      state_dir,
    } => {
      let (_workspace, state_dir) = locate(workspace, state_dir)?;
      let Response::Inspect(report) = call(&state_dir, &Request::Inspect)? else {
        bail!("the daemon answered an inspect request with something else");
      };
      // The daemon knows which work branch this view is on; requiring the caller
      // to repeat it is the one thing `git push` does not do.
      let branch = match branch.clone() {
        Some(name) => name,
        None => report.work_branch.clone().ok_or_else(|| {
          anyhow::anyhow!(
            "this workspace is not on a work branch; name the branch to push, \
             or run `gfs switch -c <branch>` first"
          )
        })?,
      };

      let mut client = connect_repository(&cli).await?;
      let response = client
        .push_branch(authed(
          &cli,
          v1::PushBranchRequest {
            repository_id: report.repository_id.clone(),
            branch: branch.clone(),
            remote_branch: remote_branch.clone().unwrap_or_default(),
            credential: credential.clone().unwrap_or_default(),
            force: *force,
            authorization: None,
          },
        )?)
        .await?
        .into_inner();

      println!("pushed {} -> {}", branch, response.remote_ref);
      if !response.summary.trim().is_empty() {
        eprintln!("{}", response.summary.trim());
      }
    }

    // These six never come back: their exit code *is* their answer, and the
    // `Result` path cannot carry it. See `run_tool`.
    Command::Rg { args } => run_tool("rg", gfs_cli::rg::run(args)),
    Command::Show { args } => run_tool("show", gfs_cli::show::run(args)),
    Command::Diff { args } => run_tool("diff", gfs_cli::revdiff::run(args)),
    Command::Blame { args } => run_tool("blame", gfs_cli::blame::run(args)),

    Command::Lease(LeaseCommand::Create { repo, rev }) => {
      let mut client = connect(&cli).await?;
      let resp = client
        .create_mount(authed(
          &cli,
          v1::CreateMountRequest {
            repository_id: repo.clone(),
            revision_selector: rev.clone(),
            requested_ttl_seconds: 0,
          },
        )?)
        .await?
        .into_inner();
      println!("mount      {}", resp.mount_id);
      println!("commit     {}", resp.commit_oid);
      println!("ref        {}", resp.ref_name.as_deref().unwrap_or("-"));
      if let Some(t) = resp.lease_expiry {
        println!("expires    {}", t.secs);
      }
      println!("heartbeat  {}s", resp.heartbeat_interval_seconds);
      // Last, so copying the fields above does not drag the credential along.
      println!("capability {}", resp.mount_capability);
    }

    Command::Lease(LeaseCommand::Renew {
      mount_id,
      capability,
    }) => {
      let mut client = connect(&cli).await?;
      let resp = client
        .renew_mount(authed(
          &cli,
          v1::RenewMountRequest {
            mount_id: mount_id.clone(),
            mount_capability: capability.clone(),
          },
        )?)
        .await?
        .into_inner();
      if let Some(t) = resp.lease_expiry {
        println!("expires    {}", t.secs);
      }
      println!("capability {}", resp.mount_capability);
    }

    Command::Lease(LeaseCommand::Release {
      mount_id,
      capability,
    }) => {
      let mut client = connect(&cli).await?;
      client
        .release_mount(authed(
          &cli,
          v1::ReleaseMountRequest {
            mount_id: mount_id.clone(),
            mount_capability: capability.clone(),
          },
        )?)
        .await?;
      println!("released {mount_id}");
    }
  }
  Ok(())
}

/// A minimal HTTP GET.
///
/// Hand-rolled over `hyper` rather than pulling in a full HTTP client: the CLI makes
/// exactly one kind of request, and the blob endpoint's contract is a bearer token
/// and a body.
async fn http_get(url: &str, token: &str) -> Result<Vec<u8>> {
  use http_body_util::BodyExt;
  use hyper_util::rt::TokioIo;

  let uri: http::Uri = url.parse().context("invalid blob url")?;
  let authority = uri.authority().context("blob url has no host")?.to_string();
  let stream = tokio::net::TcpStream::connect(&authority)
    .await
    .with_context(|| format!("connecting to {authority}"))?;
  let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
  tokio::spawn(conn);

  let mut builder = http::Request::builder().uri(&uri).header("host", authority);
  if !token.is_empty() {
    builder = builder.header("authorization", format!("Bearer {token}"));
  }
  let response = sender.send_request(builder.body(String::new())?).await?;
  let (parts, body) = response.into_parts();
  let bytes = body.collect().await?.to_bytes().to_vec();
  if !parts.status.is_success() {
    anyhow::bail!(
      "blob request failed with {}: {}",
      parts.status,
      String::from_utf8_lossy(&bytes).trim()
    );
  }
  Ok(bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_cli_accepts_the_workspace_command_grammar() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
  }

  #[test]
  fn head_becomes_the_pin_and_nothing_else_does() {
    // The silent-wrong-answer case: on the server `HEAD` is the default branch,
    // and after `gfs switch -c` that is not where the caller is standing.
    let pin = "sha1:abc";
    assert_eq!(substitute_pin("HEAD", pin), "sha1:abc");
    assert_eq!(substitute_pin("HEAD~3", pin), "sha1:abc~3");
    assert_eq!(substitute_pin("HEAD^2", pin), "sha1:abc^2");
    // A branch or tag is left alone, including one whose name merely starts with
    // the same letters.
    assert_eq!(substitute_pin("main", pin), "main");
    assert_eq!(substitute_pin("HEADER~1", pin), "HEADER~1");
    assert_eq!(substitute_pin("sha1:def", pin), "sha1:def");
  }
}
