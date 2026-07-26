//! The `xvfs` command-line interface.
//!
//! M1 gives it the commands that exercise the repository and snapshot API, which is
//! what makes the local development stack demonstrable rather than merely running.
//! `mount` here creates a *lease*, not a filesystem: `unmount`, `inspect`, `status`,
//! `diff`, and `search` arrive with M2-M4 and are absent rather than stubbed, so
//! `--help` does not advertise something that would fail.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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

  /// Create a mount lease, pinning a commit for the life of a job.
  Mount {
    #[arg(long)]
    repo: String,
    #[arg(long, default_value = "HEAD")]
    rev: String,
  },

  /// Renew a mount lease.
  Renew {
    #[arg(long)]
    mount_id: String,
    #[arg(long, hide_env_values = true)]
    capability: String,
  },

  /// Release a mount lease.
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

    Command::Mount { repo, rev } => {
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
      // The pinned commit, shown because it is the thing that matters: the branch
      // name was only a selector, and PLAN.md M2.1 requires the CLI to show it.
      println!("commit     {}", resp.commit_oid);
      println!("ref        {}", resp.ref_name.as_deref().unwrap_or("-"));
      if let Some(t) = resp.lease_expiry {
        println!("expires    {}", t.secs);
      }
      println!("heartbeat  {}s", resp.heartbeat_interval_seconds);
      // Last, so copying the fields above does not drag the credential along.
      println!("capability {}", resp.mount_capability);
    }

    Command::Renew {
      mount_id,
      capability,
    } => {
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

    Command::Release {
      mount_id,
      capability,
    } => {
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
