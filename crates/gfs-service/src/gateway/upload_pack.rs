//! The sandboxed `git upload-pack` subprocess contract.
//!
//! Everything the gateway is allowed to decide about the child is decided here:
//! the executable, its arguments, its working directory, its environment, its
//! Git configuration, and its resource limits. Nothing user-supplied reaches any
//! of those except through the typed validation in this module.
//!
//! PLAN.md M5.3 and DESIGN.md section 7.2 both require that, and the reason is
//! direct: `upload-pack` reads configuration that can re-enable hidden refs,
//! hooks, and filters, so the only safe posture is to build its configuration
//! from scratch on every invocation and never inherit anything.
//!
//! # Three traps measured in M0.3
//!
//! Each fails in a way that does not point at its cause, so each is written down
//! at the line that avoids it:
//!
//! * `uploadpackfilter.blob:none.allow` -- the subsection name contains a colon.
//!   The dotted spelling is silently ignored and upload-pack then rejects
//!   `blob:none` as unsupported.
//! * `GIT_EXEC_PATH` must be set explicitly after clearing the environment, or
//!   the child cannot fork `git-pack-objects` -- and only once a real client has
//!   got past the advertisement.
//! * `uploadpack.packObjectsHook` must be left *unset*, not set to the empty
//!   string, which Git treats as a command and fails with `cannot run :`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::revision::RESERVED_REF_PREFIX;

use super::pkt::{self, Packet};

/// The partial-clone filters GFS is willing to serve.
///
/// DESIGN.md section 7.2 sets the initial target at exactly `blob:none`. Git's
/// own `uploadpackfilter.<family>.allow` granularity is coarser than that --
/// allowing the `blob` family permits `blob:limit=<n>` too -- so the exact
/// requested filter is validated in the gateway *in addition* to configuring
/// Git. Neither check is redundant: the configuration governs what is
/// advertised, the validation governs what is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPolicy {
  /// No filtering advertised or accepted.
  Disabled,
  /// Exactly `filter=blob:none`, nothing else in the blob family.
  BlobNoneOnly,
}

impl FilterPolicy {
  pub fn permits(&self, spec: &str) -> bool {
    match self {
      FilterPolicy::Disabled => false,
      FilterPolicy::BlobNoneOnly => spec == "blob:none",
    }
  }
}

/// Resource bounds applied to one upload-pack invocation.
///
/// # Why memory is not here
///
/// PLAN.md M5.3 lists a memory limit. It is deliberately absent, and the reason
/// is measured rather than aesthetic: `upload-pack` mmaps the repository's
/// packfiles, and mapped pack bytes count against `RLIMIT_AS`. The M0.1 corpus's
/// worst case has a 4.5 GiB pack, so any address-space limit small enough to be
/// a meaningful backstop is also small enough to break clones of the exact
/// repositories GFS exists to serve. Memory is bounded by the container's
/// cgroup instead -- the same mechanism ADR 0003 already relies on for the
/// deployment model -- and the report records that as a decision, not a gap.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
  /// `RLIMIT_CPU`, in seconds. Bounds a pathological pack generation without
  /// bounding how long a slow client may take to read the result.
  pub cpu_seconds: Option<u64>,
  /// Maximum wall-clock time for the whole invocation.
  pub wall_clock: std::time::Duration,
  /// Maximum time with no output before the child is killed. Separate from
  /// `wall_clock` because a large clone is legitimately slow but never silent:
  /// upload-pack emits sideband progress throughout.
  pub inactivity: std::time::Duration,
  /// Maximum bytes the child may write to stdout across one invocation.
  pub max_output_bytes: u64,
  /// Maximum bytes of stderr retained for diagnostics.
  pub max_stderr_bytes: usize,
}

impl Default for ResourceLimits {
  fn default() -> Self {
    ResourceLimits {
      cpu_seconds: Some(600),
      wall_clock: std::time::Duration::from_secs(3600),
      inactivity: std::time::Duration::from_secs(120),
      // A clone of the worst case in the M0.1 corpus transfers about 5 GiB.
      max_output_bytes: 32 * 1024 * 1024 * 1024,
      max_stderr_bytes: 8 * 1024,
    }
  }
}

/// Everything about the child that is policy rather than mechanism.
#[derive(Debug, Clone)]
pub struct UploadPackPolicy {
  pub filter: FilterPolicy,
  /// Ref prefixes that must never be advertised or fetchable.
  pub hidden_ref_prefixes: Vec<String>,
  /// Maximum request body accepted before decompression.
  pub max_body_bytes: usize,
  /// Maximum bytes a gzip request body may expand to, and the maximum ratio.
  pub max_decompressed_bytes: usize,
  pub max_decompression_ratio: usize,
  /// Maximum concurrent upload-pack children across the process.
  pub max_concurrent_processes: usize,
  pub limits: ResourceLimits,
}

impl Default for UploadPackPolicy {
  fn default() -> Self {
    UploadPackPolicy {
      filter: FilterPolicy::BlobNoneOnly,
      hidden_ref_prefixes: vec![RESERVED_REF_PREFIX.to_owned()],
      max_body_bytes: 16 * 1024 * 1024,
      max_decompressed_bytes: 128 * 1024 * 1024,
      max_decompression_ratio: 100,
      max_concurrent_processes: 32,
      limits: ResourceLimits::default(),
    }
  }
}

/// Protocol version negotiated from the `Git-Protocol` request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitProtocol {
  /// v0 and v1 are the same wire format here; v1 differs only in that the
  /// client asked for a version and got the v0 advertisement.
  V0,
  V2,
}

impl GitProtocol {
  /// Parse and allow-list the header before it becomes `GIT_PROTOCOL`.
  ///
  /// The header is a colon-separated list of `key[=value]` items and is fully
  /// attacker-controlled, so it is never forwarded verbatim: only a recognized
  /// `version=N` produces an environment variable, and the value written is
  /// reconstructed by GFS rather than copied out of the request.
  pub fn from_header(header: Option<&str>) -> Self {
    let Some(header) = header else {
      return GitProtocol::V0;
    };
    // A header longer than any legitimate one is not parsed at all.
    if header.len() > 256 {
      return GitProtocol::V0;
    }
    for item in header.split(':') {
      if let Some(value) = item.trim().strip_prefix("version=") {
        if value.trim() == "2" {
          return GitProtocol::V2;
        }
      }
    }
    GitProtocol::V0
  }

  pub fn env_value(&self) -> Option<&'static str> {
    match self {
      GitProtocol::V0 => None,
      GitProtocol::V2 => Some("version=2"),
    }
  }
}

/// Build the fixed argument list that configures a hardened upload-pack.
///
/// Returned as data rather than applied directly so a test can assert on the
/// exact configuration. That is the only way to notice a silent regression in
/// something as consequential as `uploadpack.allowAnySHA1InWant`, whose failure
/// mode is a server that quietly answers more than it should.
pub fn protected_config(policy: &UploadPackPolicy) -> Vec<String> {
  let mut config: Vec<String> = Vec::new();
  let mut push = |key: &str, value: &str| {
    config.push("-c".to_owned());
    config.push(format!("{key}={value}"));
  };

  match policy.filter {
    FilterPolicy::Disabled => {
      push("uploadpack.allowFilter", "false");
    }
    FilterPolicy::BlobNoneOnly => {
      push("uploadpack.allowFilter", "true");
      // Deny by default, then allow exactly one filter.
      push("uploadpackfilter.allow", "false");
      push("uploadpackfilter.blob:none.allow", "true");
      // Named explicitly rather than left to the deny-by-default line, so a
      // future change to `uploadpackfilter.allow` cannot quietly enable them.
      // These are Git's *config* names for each family, which are not always
      // the filter's wire spelling: the `tree:<depth>` family is configured as
      // `tree`, so `uploadpackfilter.tree:depth.allow` is an ignored no-op that
      // looks like a denial. Measured in M0.3, not assumed.
      push("uploadpackfilter.blob:limit.allow", "false");
      push("uploadpackfilter.tree.allow", "false");
      push("uploadpackfilter.sparse:oid.allow", "false");
      push("uploadpackfilter.object:type.allow", "false");
      push("uploadpackfilter.combine.allow", "false");
    }
  }

  // Never serve an object the client did not learn about from an advertisement.
  // ADR 0002 records that protocol v2 ignores these; they are still set, because
  // v0 honours them and a future Git that fixes v2 should find them already
  // correct.
  push("uploadpack.allowAnySHA1InWant", "false");
  push("uploadpack.allowReachableSHA1InWant", "false");
  push("uploadpack.allowTipSHA1InWant", "false");

  // The internal lease namespace is not a published ref and must not leak.
  // `transfer.hideRefs` is a list and a repository's own config can append a
  // negating `!` entry; command-line `-c` values are read last and therefore
  // win, and `super::pkt::AdvertisementScanner` is the check that survives
  // being wrong about that.
  for prefix in &policy.hidden_ref_prefixes {
    push("transfer.hideRefs", prefix);
  }

  // No hooks. `uploadpack.packObjectsHook` is deliberately NOT set to the empty
  // string: Git treats an empty value as a command and fails at pack generation
  // with `cannot run :`. Leaving it unset is what disables it, and the cleared
  // environment plus `GIT_CONFIG_NOSYSTEM` is what stops a repository from
  // setting it.
  push("core.hooksPath", "/dev/null");
  // Refuse to serve objects that fail an integrity check, rather than streaming
  // corruption to a client that will only discover it at `git fsck`.
  push("transfer.fsckObjects", "true");
  // Advertising a URL for a fetch the client must make itself would let a
  // repository redirect a clone at an arbitrary host.
  push("uploadpack.allowFilterURL", "false");

  config
}

/// The environment an upload-pack child is allowed to see.
///
/// Built by allow-list, never inherited: an inherited `GIT_CONFIG_*`,
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES`, or `GIT_PROTOCOL` would each defeat part
/// of the sandbox, and the first two are trivially set by whoever launches the
/// server.
pub fn protected_env(protocol: GitProtocol) -> Vec<(String, String)> {
  let exec_path = git_exec_path();
  let mut env = vec![
    ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
    ("GIT_CONFIG_SYSTEM".to_owned(), "/dev/null".to_owned()),
    ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
    ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
    ("HOME".to_owned(), "/nonexistent".to_owned()),
    // upload-pack forks helpers such as `git-pack-objects` out of Git's exec
    // directory. With a cleared environment it must be told where that is.
    ("GIT_EXEC_PATH".to_owned(), exec_path.clone()),
    ("PATH".to_owned(), format!("{exec_path}:/usr/bin:/bin")),
  ];
  if let Some(value) = protocol.env_value() {
    env.push(("GIT_PROTOCOL".to_owned(), value.to_owned()));
  }
  env
}

/// Git's helper directory, resolved once per process.
///
/// A deployment should pin this to the packaged path. Asking the binary is
/// correct here for the same reason `scripts/check.sh` asserts the Git version:
/// the gateway must agree with whichever Git it actually runs.
fn git_exec_path() -> String {
  use std::sync::OnceLock;
  static PATH: OnceLock<String> = OnceLock::new();
  PATH
    .get_or_init(|| {
      std::process::Command::new("git")
        .arg("--exec-path")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "/usr/lib/git-core".to_owned())
    })
    .clone()
}

/// One repository, ready to serve upload-pack requests.
#[derive(Debug, Clone)]
pub struct UploadPack {
  repo: PathBuf,
  policy: UploadPackPolicy,
}

impl UploadPack {
  /// Bind to a bare repository path taken from the **catalog**, never from a
  /// request.
  ///
  /// Repository selection happens upstream, in the router, by parsing a
  /// `RepositoryId` and looking it up. By the time a path reaches here it has
  /// already been through `Registry::require_servable`, so this constructor
  /// checks shape rather than authority.
  pub fn new(repo: &Path, policy: UploadPackPolicy) -> Result<Self, GfsError> {
    let repo = repo.canonicalize().map_err(|e| {
      GfsError::new(
        ErrorCode::Internal,
        format!("repository path is unusable: {e}"),
      )
    })?;
    if !repo.join("objects").is_dir() {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        "not a bare Git repository",
      ));
    }
    Ok(UploadPack { repo, policy })
  }

  pub fn policy(&self) -> &UploadPackPolicy {
    &self.policy
  }

  /// The full argument vector for one invocation, including the resource-limit
  /// wrapper when one applies.
  ///
  /// Returned as data so M5.3's isolation test can assert that no shell, no
  /// user-controlled executable, and no user-controlled argument is present.
  pub fn argv(&self, protocol: GitProtocol, mode: Mode) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    if let (Some(cpu), Some(prlimit)) = (self.policy.limits.cpu_seconds, prlimit_path()) {
      // `prlimit` execs the target directly -- it is not a shell, and its
      // arguments here are constants. It is the only way to set `RLIMIT_CPU`
      // without `pre_exec`, which this workspace denies (`unsafe_code = deny`).
      // When it is absent the invocation proceeds without the limit rather than
      // failing: the wall-clock and inactivity deadlines still bound the child,
      // and refusing to serve Git because a util-linux binary is missing would
      // be a worse failure than a softer bound.
      argv.push(prlimit.into());
      argv.push(format!("--cpu={cpu}").into());
      argv.push("--".into());
    }
    argv.push("git".into());
    argv.extend(protected_config(&self.policy).into_iter().map(Into::into));
    argv.push("upload-pack".into());
    argv.push(match mode {
      Mode::Advertise => "--http-backend-info-refs".into(),
      Mode::StatelessRpc => "--stateless-rpc".into(),
    });
    argv.push(self.repo.clone().into());
    let _ = protocol; // carried in the environment, never in argv
    argv
  }

  /// Spawn the child with piped stdio.
  ///
  /// `kill_on_drop` is load-bearing rather than tidy: when a client disconnects
  /// mid-clone the response body is dropped, the task owning this child is
  /// cancelled, and without it a `git pack-objects` would keep burning CPU on a
  /// pack nobody will read.
  pub fn spawn(
    &self,
    protocol: GitProtocol,
    mode: Mode,
  ) -> Result<tokio::process::Child, GfsError> {
    let argv = self.argv(protocol, mode);
    let mut command = tokio::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.env_clear();
    for (key, value) in protected_env(protocol) {
      command.env(key, value);
    }
    // A fixed working directory that is not the repository, so a relative path
    // anywhere in the request can never resolve to something useful.
    command.current_dir("/");
    command.stdin(match mode {
      Mode::Advertise => Stdio::null(),
      Mode::StatelessRpc => Stdio::piped(),
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    command.spawn().map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("cannot start upload-pack: {e}"),
      )
    })
  }

  /// Reject a request whose filter or shape the policy does not permit.
  ///
  /// Runs before a subprocess exists to attack. Enforced here rather than left
  /// to Git's configuration because the two have different granularity:
  /// `uploadpackfilter.blob.*` cannot express "`blob:none` but not
  /// `blob:limit`".
  pub fn validate_request(&self, request: &[u8]) -> Result<(), GfsError> {
    let packets = pkt::decode(request)?;
    for packet in &packets {
      let Packet::Data(payload) = packet else {
        continue;
      };
      let text = String::from_utf8_lossy(payload);
      let text = text.trim_end_matches(['\n', '\0']);

      // v0 sends `filter <spec>`; v2 sends the same line inside the `fetch`
      // command section, so one check covers both.
      if let Some(spec) = text.strip_prefix("filter ") {
        let spec = spec.trim();
        if !self.policy.filter.permits(spec) {
          return Err(GfsError::new(
            ErrorCode::PermissionDenied,
            format!("partial-clone filter {spec:?} is not permitted"),
          ));
        }
      }

      // A `want-ref` naming the reserved namespace is refused by name. Git
      // would refuse it too because the ref is hidden, but the gateway's
      // rejection is legible in an audit log and does not depend on that.
      if let Some(rest) = text.strip_prefix("want-ref ") {
        for prefix in &self.policy.hidden_ref_prefixes {
          if rest.trim().starts_with(prefix.as_str()) {
            return Err(GfsError::new(
              ErrorCode::ReservedNamespace,
              "that ref namespace is internal to GFS",
            ));
          }
        }
      }
    }
    Ok(())
  }

  /// Decompress a `Content-Encoding: gzip` request body under explicit bounds.
  ///
  /// Unbounded decompression here is a trivially reachable memory-exhaustion
  /// bug -- the request body is the one thing an unauthenticated-adjacent
  /// caller fully controls -- so both an absolute output cap and an expansion
  /// ratio cap apply, and the reader is `take`-bounded so the cap is enforced
  /// during inflation rather than after it.
  pub fn decompress_body(&self, body: &[u8]) -> Result<Vec<u8>, GfsError> {
    decompress(&self.policy, body)
  }

  /// Redact anything about the server's filesystem out of a child's stderr.
  ///
  /// upload-pack names the repository path in several of its errors. That path
  /// is deployment topology, and the client has no use for it; the unredacted
  /// text goes to the server's own log.
  pub fn redact(&self, stderr: &str) -> String {
    let path = self.repo.display().to_string();
    let redacted = stderr.replace(&path, "<repository>");
    // Also drop the parent directory, which appears in "not a git repository"
    // style messages without the repository component.
    match self.repo.parent() {
      Some(parent) => redacted.replace(&parent.display().to_string(), "<root>"),
      None => redacted,
    }
  }
}

/// Decompress a `Content-Encoding: gzip` request body under explicit bounds.
///
/// A free function because both directions of the gateway need it and the
/// bounds are the policy's, not the service's. See [`UploadPack::decompress_body`]
/// for why both caps exist.
pub fn decompress(policy: &UploadPackPolicy, body: &[u8]) -> Result<Vec<u8>, GfsError> {
  use std::io::Read;
  let limit = policy.max_decompressed_bytes;
  let mut out = Vec::new();
  let mut decoder = flate2::read::GzDecoder::new(body).take(limit as u64 + 1);
  decoder.read_to_end(&mut out).map_err(|e| {
    GfsError::new(
      ErrorCode::InvalidArgument,
      format!("malformed gzip request body: {e}"),
    )
  })?;
  if out.len() > limit {
    return Err(GfsError::new(
      ErrorCode::ResourceLimit,
      format!("gzip request body exceeded the {limit} byte output limit"),
    ));
  }
  if !body.is_empty() && out.len() / body.len() > policy.max_decompression_ratio {
    return Err(GfsError::new(
      ErrorCode::ResourceLimit,
      format!(
        "gzip request body exceeded the {}:1 expansion ratio",
        policy.max_decompression_ratio
      ),
    ));
  }
  Ok(out)
}

/// Which of the two upload-pack invocations to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  /// `GET /info/refs`: the ref and capability advertisement.
  Advertise,
  /// `POST /git-upload-pack`: the stateless RPC.
  StatelessRpc,
}

/// `prlimit`, if util-linux is installed.
pub(crate) fn prlimit_path() -> Option<&'static str> {
  use std::sync::OnceLock;
  static FOUND: OnceLock<Option<&'static str>> = OnceLock::new();
  *FOUND.get_or_init(|| {
    ["/usr/bin/prlimit", "/bin/prlimit"]
      .into_iter()
      .find(|p| std::path::Path::new(p).is_file())
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_hostile_protocol_header_never_reaches_the_environment_verbatim() {
    let protocol = GitProtocol::from_header(Some("version=2:evil=1"));
    assert_eq!(protocol, GitProtocol::V2);
    // Reconstructed, not copied: the `evil=1` item is gone.
    assert_eq!(protocol.env_value(), Some("version=2"));

    for rejected in ["version=99", "garbage", "version=2x", &"a".repeat(4096)] {
      assert_eq!(
        GitProtocol::from_header(Some(rejected)),
        GitProtocol::V0,
        "{rejected:?} must not negotiate v2"
      );
    }
    assert_eq!(GitProtocol::from_header(None), GitProtocol::V0);
    assert_eq!(GitProtocol::V0.env_value(), None);
  }

  #[test]
  fn the_filter_policy_permits_only_the_exact_form() {
    let policy = FilterPolicy::BlobNoneOnly;
    assert!(policy.permits("blob:none"));
    // Every one of these is inside a family Git's own configuration would allow.
    for denied in [
      "blob:limit=1k",
      "tree:0",
      "tree:1",
      "sparse:oid=main",
      "combine:blob:none+tree:0",
      "object:type=blob",
      "",
    ] {
      assert!(!policy.permits(denied), "{denied:?} must be denied");
    }
    assert!(!FilterPolicy::Disabled.permits("blob:none"));
  }

  #[test]
  fn protected_config_denies_unadvertised_wants_and_hides_the_lease_namespace() {
    let config = protected_config(&UploadPackPolicy::default()).join(" ");
    assert!(config.contains("uploadpack.allowAnySHA1InWant=false"));
    assert!(config.contains("uploadpack.allowReachableSHA1InWant=false"));
    assert!(config.contains("uploadpack.allowTipSHA1InWant=false"));
    assert!(config.contains("transfer.hideRefs=refs/gfs/"));
    assert!(config.contains("core.hooksPath=/dev/null"));
    // Colon-bearing subsection name; the dotted spelling is a silent no-op.
    assert!(config.contains("uploadpackfilter.blob:none.allow=true"));
    assert!(config.contains("uploadpackfilter.blob:limit.allow=false"));
    // `tree`, not `tree:depth`: Git's config name for the family differs from
    // the filter's wire spelling, and the wrong name denies nothing.
    assert!(config.contains("uploadpackfilter.tree.allow=false"));
    assert!(!config.contains("uploadpackfilter.tree:depth"));
    // Deny-by-default is what actually enforces the policy; the per-family
    // lines are defence in depth against it changing.
    assert!(config.contains("uploadpackfilter.allow=false"));
    // Never set to the empty string. See the module comment.
    assert!(!config.contains("packObjectsHook"));

    let disabled = protected_config(&UploadPackPolicy {
      filter: FilterPolicy::Disabled,
      ..Default::default()
    })
    .join(" ");
    assert!(disabled.contains("uploadpack.allowFilter=false"));
    assert!(!disabled.contains("uploadpackfilter.blob:none.allow=true"));
  }

  #[test]
  fn the_environment_is_an_allow_list_that_neutralizes_configuration() {
    let env: std::collections::HashMap<_, _> = protected_env(GitProtocol::V2).into_iter().collect();
    assert_eq!(
      env.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
      Some("1")
    );
    assert_eq!(
      env.get("GIT_CONFIG_GLOBAL").map(String::as_str),
      Some("/dev/null")
    );
    assert_eq!(
      env.get("GIT_PROTOCOL").map(String::as_str),
      Some("version=2")
    );
    // Present, or the child cannot fork `git-pack-objects` once a real client
    // gets past the advertisement.
    assert!(env.contains_key("GIT_EXEC_PATH"));
    // Nothing that would reintroduce inherited state.
    assert!(!env.contains_key("GIT_ALTERNATE_OBJECT_DIRECTORIES"));
    assert!(!env.contains_key("GIT_DIR"));

    let v0: std::collections::HashMap<_, _> = protected_env(GitProtocol::V0).into_iter().collect();
    assert!(!v0.contains_key("GIT_PROTOCOL"));
  }

  #[test]
  fn a_gzip_bomb_is_refused_by_ratio_before_it_is_refused_by_size() {
    use std::io::Write;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("objects")).unwrap();
    let pack = UploadPack::new(repo.path(), UploadPackPolicy::default()).unwrap();

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&vec![b'a'; 4 * 1024 * 1024]).unwrap();
    let bomb = encoder.finish().unwrap();
    let err = pack.decompress_body(&bomb).unwrap_err();
    assert_eq!(err.code, ErrorCode::ResourceLimit);

    // An ordinary body round-trips.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
      .write_all(b"0032want 1111111111111111111111111111111111111111\n0000")
      .unwrap();
    let ordinary = encoder.finish().unwrap();
    assert!(pack.decompress_body(&ordinary).is_ok());

    // Garbage is an argument error, not a panic or a hang.
    assert!(pack.decompress_body(b"not gzip at all").is_err());
  }

  #[test]
  fn request_validation_rejects_the_filters_git_config_would_allow() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("objects")).unwrap();
    let pack = UploadPack::new(repo.path(), UploadPackPolicy::default()).unwrap();

    let body = |line: &str| {
      let mut out = pkt::pkt_line(line.as_bytes());
      out.extend_from_slice(pkt::FLUSH_PKT);
      out
    };
    assert!(pack.validate_request(&body("filter blob:none\n")).is_ok());
    for denied in ["filter tree:0\n", "filter blob:limit=1k\n"] {
      let err = pack.validate_request(&body(denied)).unwrap_err();
      assert_eq!(err.code, ErrorCode::PermissionDenied);
    }
    let err = pack
      .validate_request(&body("want-ref refs/gfs/mounts/m-1\n"))
      .unwrap_err();
    assert_eq!(err.code, ErrorCode::ReservedNamespace);
  }

  #[test]
  fn argv_contains_no_shell_and_no_user_controlled_argument() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("objects")).unwrap();
    let pack = UploadPack::new(repo.path(), UploadPackPolicy::default()).unwrap();
    let argv = pack.argv(GitProtocol::V2, Mode::StatelessRpc);
    let rendered: Vec<String> = argv
      .iter()
      .map(|a| a.to_string_lossy().into_owned())
      .collect();

    // The executable is `git` or the constant `prlimit` wrapper, never a shell.
    assert!(rendered[0].ends_with("git") || rendered[0].ends_with("prlimit"));
    assert!(!rendered.iter().any(|a| a.contains("sh -c") || a == "sh"));
    // The repository path is the last argument and comes from the catalog.
    assert_eq!(rendered.last().unwrap(), &pack.repo.display().to_string());
    assert!(rendered.contains(&"--stateless-rpc".to_owned()));
    // The negotiated protocol travels in the environment, not in argv, so a
    // header cannot become an argument even if validation were bypassed.
    assert!(!rendered.iter().any(|a| a.contains("version=2")));
  }

  #[test]
  fn stderr_redaction_removes_the_server_filesystem_layout() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("objects")).unwrap();
    let pack = UploadPack::new(repo.path(), UploadPackPolicy::default()).unwrap();
    let raw = format!("fatal: bad object in {}\n", pack.repo.display());
    let redacted = pack.redact(&raw);
    assert!(redacted.contains("<repository>"));
    assert!(!redacted.contains(&pack.repo.display().to_string()));
  }
}
