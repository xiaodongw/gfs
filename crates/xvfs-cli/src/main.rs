//! The `xvfs` command-line interface.
//!
//! Two families of command, and the split matters:
//!
//! * **Workspace commands** — `mount`, `unmount`, `inspect`, `health`,
//!   `refresh` — act on a *mounted workspace*. They start `xvfsd` or talk to a
//!   running one over its control socket, and they are what an orchestrator and
//!   an agent use.
//! * **Snapshot commands** — `resolve`, `ls`, `cat` — read the server directly,
//!   with no mount involved. They are how the API is demonstrated and debugged.
//! * **`lease`** — creates, renews, and releases a retention lease by hand. A
//!   mount does this for itself; the subcommand exists so M1's lease machine can
//!   be exercised without one, which is what `scripts/dev-stack.sh` does.
//!
//! # Where things live
//!
//! Given `--workspace /path/to/ws`, the state directory defaults to
//! `/path/to/ws.xvfs`. Every workspace command can therefore find a running
//! daemon from the workspace path alone, which is the only thing a job knows.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use xvfs_fuse::control::{self, Request, Response};
use xvfs_fuse::state::MountState;
use xvfs_proto::v1;

#[derive(Parser, Debug)]
#[command(name = "xvfs", version, about = "Agent-oriented virtual Git workspace")]
struct Cli {
  /// The server's gRPC endpoint.
  #[arg(long, env = "XVFS_ENDPOINT", default_value = "http://127.0.0.1:8431")]
  endpoint: String,

  /// The server's HTTP endpoint, used for blob reads.
  #[arg(
    long,
    env = "XVFS_HTTP_ENDPOINT",
    default_value = "http://127.0.0.1:8430"
  )]
  http_endpoint: String,

  /// Bearer token.
  #[arg(long, env = "XVFS_TOKEN", hide_env_values = true, default_value = "")]
  token: String,

  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
  /// Print version and pinned build information.
  Version,

  /// Resolve a revision selector to a pinned commit.
  Resolve {
    #[arg(long)]
    repo: String,
    /// A branch, tag, full object ID, or an abbreviation of at least 7 characters.
    /// Revision expressions such as `HEAD~1` are not accepted.
    rev: String,
  },

  /// List one directory of a snapshot.
  Ls {
    #[arg(long)]
    repo: String,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    /// Path from the snapshot root. Empty lists the root.
    #[arg(default_value = "")]
    path: String,
    #[arg(long, default_value_t = 1000)]
    page_size: u32,
  },

  /// Print one file's raw bytes.
  ///
  /// Raw: DESIGN.md section 12 documents that XVFS applies no `.gitattributes`
  /// conversion, so these are the bytes the object database holds and not
  /// necessarily the bytes `git checkout` would write.
  Cat {
    #[arg(long)]
    repo: String,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    path: String,
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
    /// Defaults to `<workspace>.xvfs`.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Defaults to `$XDG_CACHE_HOME/xvfs`.
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

  /// Release the lease, unmount, and stop the daemon.
  Unmount {
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
  },

  /// Everything about a mounted workspace.
  Inspect {
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
  },

  /// Daemon and lease health. Exits non-zero when the lease is not renewing.
  Health {
    #[arg(long)]
    workspace: PathBuf,
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
    workspace: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
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
    workspace: PathBuf,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Defaults to `<state-dir>/bin`.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
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

type Client = v1::snapshot_service_client::SnapshotServiceClient<tonic::transport::Channel>;

async fn connect(cli: &Cli) -> Result<Client> {
  let channel = tonic::transport::Endpoint::from_shared(cli.endpoint.clone())
    .context("invalid endpoint")?
    .connect()
    .await
    .with_context(|| format!("connecting to {}", cli.endpoint))?;
  Ok(Client::new(channel))
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

/// `<workspace>.xvfs` unless told otherwise.
fn state_dir_for(workspace: &Path, explicit: &Option<PathBuf>) -> PathBuf {
  explicit.clone().unwrap_or_else(|| {
    let mut name = workspace.as_os_str().to_os_string();
    name.push(".xvfs");
    PathBuf::from(name)
  })
}

fn default_cache_dir() -> PathBuf {
  std::env::var_os("XDG_CACHE_HOME")
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    .unwrap_or_else(std::env::temp_dir)
    .join("xvfs")
}

/// Locate `xvfsd`.
///
/// Next to this binary first: a development build and an installed package both
/// put the two together, and picking up a *different* daemon from `PATH` than the
/// CLI it shipped with is a version-skew bug that only appears at runtime.
fn daemon_binary() -> PathBuf {
  sibling_binary("xvfsd", "XVFS_DAEMON")
}

fn shim_binary() -> PathBuf {
  sibling_binary("xvfs-git-shim", "XVFS_GIT_SHIM")
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

fn call(state_dir: &Path, request: &Request) -> Result<Response> {
  let socket = MountState::control_socket(state_dir);
  let response = control::call(&socket, request)
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;
  response
    .into_result()
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))
}

#[allow(clippy::too_many_arguments)]
fn do_mount(cli: &Cli, args: MountArgs) -> Result<()> {
  let state_dir = state_dir_for(&args.workspace, &args.state_dir);
  let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
  std::fs::create_dir_all(&state_dir)
    .with_context(|| format!("creating {}", state_dir.display()))?;
  let ready = state_dir.join("ready");
  let _ = std::fs::remove_file(&ready);

  let mut command = std::process::Command::new(daemon_binary());
  command
    .arg("--state-dir")
    .arg(&state_dir)
    .arg("--workspace")
    .arg(&args.workspace)
    .arg("--cache-dir")
    .arg(&cache_dir)
    .arg("--endpoint")
    .arg(&cli.endpoint)
    .arg("--http-endpoint")
    .arg(&cli.http_endpoint)
    .arg("--token")
    .arg(&cli.token)
    .arg("--repo")
    .arg(&args.repo)
    .arg("--rev")
    .arg(&args.rev)
    .arg("--cache-quota")
    .arg(args.cache_quota.to_string())
    .arg("--overlay-quota")
    .arg(args.overlay_quota.to_string());
  if args.allow_other {
    command.arg("--allow-other");
  }

  if args.foreground {
    let status = command.status().context("running xvfsd")?;
    if !status.success() {
      bail!("xvfsd exited with {status}");
    }
    return Ok(());
  }

  command.arg("--ready-file").arg(&ready);
  // The daemon outlives this process, so none of its descriptors may stay tied
  // to this terminal. This is not tidiness: a backgrounded daemon holding the
  // inherited stderr keeps the write end of a pipe open, so `xvfs mount | tee`
  // never sees EOF and appears to hang long after the command finished.
  //
  // A supervisor that wants the logs elsewhere reads this file or runs
  // `--foreground`.
  let log_path = state_dir.join("xvfsd.log");
  let log = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(&log_path)
    .with_context(|| format!("opening {}", log_path.display()))?;
  command
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::from(log));
  let mut child = command.spawn().with_context(|| {
    format!(
      "starting {} (set XVFS_DAEMON if it is elsewhere)",
      daemon_binary().display()
    )
  })?;

  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(args.timeout_seconds);
  loop {
    if ready.is_file() {
      break;
    }
    // A daemon that exited is a failure to report now, not a timeout to wait out.
    if let Some(status) = child.try_wait().context("waiting for xvfsd")? {
      bail!("xvfsd exited before the workspace was ready ({status})");
    }
    if std::time::Instant::now() >= deadline {
      let _ = child.kill();
      bail!(
        "the workspace was not ready within {}s",
        args.timeout_seconds
      );
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
  }

  let Response::Inspect(report) = call(&state_dir, &Request::Inspect)? else {
    bail!("the daemon answered an inspect request with something else");
  };
  print_report(&report);
  println!("log        {}", log_path.display());
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

fn print_report(report: &xvfs_fuse::control::MountReport) {
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
  if !report.retiring_generations.is_empty() {
    println!("retiring   {:?}", report.retiring_generations);
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  let cli = Cli::parse();

  match &cli.command {
    Command::Version => {
      println!("xvfs {}", env!("CARGO_PKG_VERSION"));
      println!("api-version {}", xvfs_types::API_VERSION);
      println!("state-format-version {}", xvfs_types::STATE_FORMAT_VERSION);
    }

    Command::Resolve { repo, rev } => {
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
    } => {
      let mut client = connect(&cli).await?;
      let commit = resolve(&cli, &mut client, repo, rev).await?;

      let mut token = Vec::new();
      loop {
        let page = client
          .list_directory(authed(
            &cli,
            v1::ListDirectoryRequest {
              repository_id: repo.clone(),
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
          // The escaped form, because a Git path is bytes and need not be UTF-8.
          let display = xvfs_types::BytePath::new(e.path.clone());
          println!(
            "{:06o} {:>10} {}  {}",
            e.mode,
            e.size,
            e.oid,
            display.escaped()
          );
        }
        if page.next_page_token.is_empty() {
          break;
        }
        token = page.next_page_token;
      }
    }

    Command::Cat { repo, rev, path } => {
      let mut client = connect(&cli).await?;
      let commit = resolve(&cli, &mut client, repo, rev).await?;
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
      let state_dir = state_dir_for(workspace, state_dir);
      match call(&state_dir, &Request::Unmount)? {
        Response::Unmounted => println!("unmounted {}", workspace.display()),
        other => bail!("unexpected daemon response: {other:?}"),
      }
    }

    Command::Inspect {
      workspace,
      state_dir,
      json,
    } => {
      let state_dir = state_dir_for(workspace, state_dir);
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
      let state_dir = state_dir_for(workspace, state_dir);
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
      let state_dir = state_dir_for(workspace, state_dir);
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

    Command::InstallShim {
      workspace,
      state_dir,
      bin_dir,
    } => {
      let state_dir = state_dir_for(workspace, state_dir);
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
          "cannot find {} (set XVFS_GIT_SHIM to its path)",
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
  fn the_state_directory_is_derivable_from_the_workspace_alone() {
    // The property every workspace command depends on: a job knows its workspace
    // path and nothing else, and must still be able to find its daemon.
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &None),
      PathBuf::from("/jobs/42/ws.xvfs")
    );
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &Some(PathBuf::from("/elsewhere"))),
      PathBuf::from("/elsewhere")
    );
  }

  #[test]
  fn the_cli_accepts_the_workspace_command_grammar() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
  }
}
