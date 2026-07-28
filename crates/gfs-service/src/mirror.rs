//! Upstream mirroring, and the refspecs that keep pruning away from
//! `refs/gfs/`.
//!
//! Fetching runs **stock Git as a subprocess**, not libgit2. ADR 0001 builds
//! `git2` with `default-features = false`, dropping its `https` and `ssh`
//! features, precisely so the object path does not link OpenSSL and libssh2: the
//! server never uses libgit2 as a network client. Fetching is stock Git's job.
//!
//! # Why not `--mirror`
//!
//! `git fetch --mirror` (and `remote.<name>.mirror = true`) maps `refs/*` to
//! `refs/*` and prunes anything on the local side that the remote does not have.
//! Lease anchors live under `refs/gfs/mounts/*` and exist *only* locally, so a
//! mirroring prune deletes every one of them -- silently, as part of a routine
//! fetch -- and the next `git gc` then prunes the pinned commits out from under
//! every live mount.
//!
//! ADR 0006 therefore forbids unrestricted mirror pruning over internal refs. The
//! refspecs below name exactly the namespaces GFS mirrors, so pruning cannot
//! reach anything else. This is enforced by construction rather than by review:
//! [`FETCH_REFSPECS`] is the only set the fetch uses, and
//! [`refspec_is_safe`] rejects any addition that would cover the reserved
//! namespace.

use std::path::Path;
use std::process::Command;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::revision::RESERVED_REF_PREFIX;

/// The namespaces GFS mirrors from upstream.
///
/// Deliberately not `+refs/*:refs/*`. Adding a namespace here is a decision that
/// has to be checked against [`refspec_is_safe`].
pub const FETCH_REFSPECS: &[&str] = &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Whether a refspec's destination can be pruned without touching the reserved
/// namespace.
///
/// A destination pattern is unsafe when the reserved prefix would match it. The
/// two cases that matter: a bare `refs/*` wildcard, and anything explicitly naming
/// `refs/gfs/`.
pub fn refspec_is_safe(refspec: &str) -> bool {
  let spec = refspec.strip_prefix('+').unwrap_or(refspec);
  let Some((_, dst)) = spec.split_once(':') else {
    // A refspec with no destination fetches into FETCH_HEAD and prunes nothing,
    // but it is also not something this code should be constructing.
    return false;
  };
  if dst.contains("gfs") {
    return false;
  }
  match dst.strip_suffix('*') {
    // A wildcard destination is safe only when its literal prefix cannot be a
    // prefix of the reserved namespace. `refs/*` fails this; `refs/heads/*`
    // passes.
    Some(prefix) => !RESERVED_REF_PREFIX.starts_with(prefix),
    None => !dst.starts_with(RESERVED_REF_PREFIX),
  }
}

/// The outcome of one fetch.
#[derive(Clone, Debug, Default)]
pub struct FetchOutcome {
  /// Stock Git's summary output, already bounded and safe to log.
  pub summary: String,
}

/// Fetch from upstream into a bare mirror.
///
/// `credential` is the resolved secret for this fetch, held only for the duration
/// of the call. The catalog stores a *reference* to it, never the value, so a
/// catalog dump is not a credential leak.
pub fn fetch(
  repo_path: &Path,
  upstream_url: &str,
  credential: Option<&str>,
  git_binary: &Path,
) -> Result<FetchOutcome, GfsError> {
  for spec in FETCH_REFSPECS {
    // Checked at the point of use, not only when the constant was written, so a
    // future edit to `FETCH_REFSPECS` cannot quietly widen the prune.
    if !refspec_is_safe(spec) {
      return Err(GfsError::internal(format!(
        "refusing to fetch with refspec {spec:?}: its destination could prune \
         {RESERVED_REF_PREFIX}"
      )));
    }
  }

  let mut cmd = Command::new(git_binary);
  // A cleared environment with an allow-list, matching ADR 0001's subprocess
  // sandbox. `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` are pointed at
  // `/dev/null` so a host's `~/.gitconfig` cannot change the refspecs, enable a
  // mirror mode, or install a credential helper.
  cmd
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_TERMINAL_PROMPT", "0")
    // Without this, a fetch over an authenticated URL can block forever waiting
    // for a passphrase that will never arrive.
    .env("GIT_ASKPASS", "/bin/true")
    .env("PATH", "/usr/bin:/bin")
    .current_dir(repo_path);

  if let Some(secret) = credential {
    // Passed through the environment rather than embedded in the URL: a URL
    // appears in `ps`, in Git's own error messages, and in the reflog.
    cmd.env("GFS_UPSTREAM_CREDENTIAL", secret);
    cmd.env(
      "GIT_ASKPASS",
      // A tiny helper is out of scope for M1; the credential path is exercised by
      // the local `file://` fixtures, which need none. M6.1 wires the real secret
      // store in, and this is where it lands.
      "/bin/true",
    );
  }

  cmd
    .arg("fetch")
    .arg("--prune")
    .arg("--prune-tags")
    .arg("--no-tags");
  cmd.arg("--").arg(upstream_url);
  for spec in FETCH_REFSPECS {
    cmd.arg(spec);
  }

  let out = cmd
    .output()
    .map_err(|e| GfsError::new(ErrorCode::Unavailable, format!("cannot run git fetch: {e}")))?;

  if !out.status.success() {
    // Bounded, and the repository path is not included: DESIGN.md section 10 and
    // ADR 0006 both require diagnostics without leaking server-side paths.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bounded: String = stderr.chars().take(2000).collect();
    return Err(GfsError::new(
      ErrorCode::Unavailable,
      format!("upstream fetch failed: {}", bounded.trim()),
    ));
  }

  let summary: String = String::from_utf8_lossy(&out.stderr)
    .chars()
    .take(2000)
    .collect();
  Ok(FetchOutcome { summary })
}

/// Create an empty bare mirror.
pub fn init_bare(repo_path: &Path, git_binary: &Path) -> Result<(), GfsError> {
  std::fs::create_dir_all(repo_path).map_err(|e| {
    GfsError::new(
      ErrorCode::Internal,
      format!("cannot create the repository directory: {e}"),
    )
  })?;
  let out = Command::new(git_binary)
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .args([
      "init",
      "--bare",
      "--quiet",
      // ADR 0001's supported boundary, made explicit at creation rather than
      // inherited from whatever this Git version defaults to. Git 2.45+ can
      // create `reftable` repositories, which libgit2 cannot read at all.
      "--ref-format=files",
      "--object-format=sha1",
    ])
    .arg(repo_path)
    .output()
    .map_err(|e| GfsError::new(ErrorCode::Internal, format!("cannot run git init: {e}")))?;
  if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    return Err(GfsError::new(
      ErrorCode::Internal,
      format!("git init failed: {}", stderr.trim()),
    ));
  }
  Ok(())
}

/// Ask the upstream which branch its `HEAD` points at.
///
/// A bare mirror created by [`init_bare`] has a `HEAD` pointing at whatever this
/// Git version calls a default branch, which need not be the upstream's: cloning
/// a `master` repository with a Git that defaults to `main` leaves `HEAD`
/// dangling, and every later `HEAD` selector then resolves to nothing. Asking is
/// one extra round trip and removes the guess.
///
/// `--symref` rather than parsing `git remote show`, because `remote show` is
/// porcelain and its output is localized.
pub fn default_branch(
  upstream_url: &str,
  credential: Option<&str>,
  git_binary: &Path,
) -> Result<String, GfsError> {
  let mut cmd = Command::new(git_binary);
  cmd
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_ASKPASS", "/bin/true")
    .env("PATH", "/usr/bin:/bin");
  if let Some(secret) = credential {
    cmd.env("GFS_UPSTREAM_CREDENTIAL", secret);
  }
  let out = cmd
    .args(["ls-remote", "--symref"])
    .arg("--")
    .arg(upstream_url)
    .arg("HEAD")
    .output()
    .map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("cannot run git ls-remote: {e}"),
      )
    })?;
  if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bounded: String = stderr.chars().take(2000).collect();
    return Err(GfsError::new(
      ErrorCode::Unavailable,
      format!(
        "cannot read the upstream's default branch: {}",
        bounded.trim()
      ),
    ));
  }
  // `ref: refs/heads/main\tHEAD`, followed by the resolved object line.
  let text = String::from_utf8_lossy(&out.stdout);
  for line in text.lines() {
    let Some(rest) = line.strip_prefix("ref: ") else {
      continue;
    };
    let Some(target) = rest.split('\t').next() else {
      continue;
    };
    if let Some(branch) = target.strip_prefix("refs/heads/") {
      if !branch.is_empty() {
        return Ok(branch.to_owned());
      }
    }
  }
  // An empty upstream advertises no symref. That is not an error -- there is
  // simply nothing to mount yet -- so the caller decides what to do about it.
  Err(GfsError::new(
    ErrorCode::FailedPrecondition,
    "the upstream advertises no default branch; it may be empty",
  ))
}

/// Point a bare mirror's `HEAD` at one of its branches.
pub fn set_head(repo_path: &Path, branch: &str, git_binary: &Path) -> Result<(), GfsError> {
  // Built here rather than taken as a full ref, so a caller cannot aim `HEAD`
  // at the reserved namespace and make internal work refs the default view.
  let target = format!("refs/heads/{branch}");
  if !gfs_types::revision::is_valid_branch_name(branch) {
    return Err(GfsError::invalid(format!(
      "not a usable branch name: {branch:?}"
    )));
  }
  let out = Command::new(git_binary)
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .current_dir(repo_path)
    .args(["symbolic-ref", "HEAD"])
    .arg(&target)
    .output()
    .map_err(|e| {
      GfsError::new(
        ErrorCode::Internal,
        format!("cannot run git symbolic-ref: {e}"),
      )
    })?;
  if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    return Err(GfsError::new(
      ErrorCode::Internal,
      format!("cannot set HEAD: {}", stderr.trim()),
    ));
  }
  Ok(())
}

/// Verify a mirror with `git fsck`.
///
/// Used by the verify step of the lifecycle state machine. A failure is a reason to
/// quarantine, not to delete.
pub fn fsck(repo_path: &Path, git_binary: &Path) -> Result<(), GfsError> {
  let out = Command::new(git_binary)
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .current_dir(repo_path)
    .args(["fsck", "--no-progress", "--connectivity-only"])
    .output()
    .map_err(|e| GfsError::new(ErrorCode::Internal, format!("cannot run git fsck: {e}")))?;
  if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bounded: String = stderr.chars().take(2000).collect();
    return Err(GfsError::new(
      ErrorCode::FailedPrecondition,
      format!("fsck reported problems: {}", bounded.trim()),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_configured_refspecs_cannot_prune_the_reserved_namespace() {
    for spec in FETCH_REFSPECS {
      assert!(refspec_is_safe(spec), "{spec} must be safe");
    }
  }

  #[test]
  fn a_mirror_style_refspec_is_rejected() {
    // This is the one that deletes every lease anchor as part of a routine fetch.
    assert!(!refspec_is_safe("+refs/*:refs/*"));
    assert!(!refspec_is_safe("refs/*:refs/*"));
    assert!(!refspec_is_safe("+refs/gfs/*:refs/gfs/*"));
    assert!(!refspec_is_safe("+refs/heads/*:refs/gfs/sneaky/*"));
    // A destination with no wildcard inside the namespace is equally unsafe.
    assert!(!refspec_is_safe("+refs/heads/main:refs/gfs/mounts/m-1"));
  }

  #[test]
  fn narrower_namespaces_are_safe() {
    assert!(refspec_is_safe("+refs/heads/*:refs/heads/*"));
    assert!(refspec_is_safe("+refs/tags/*:refs/tags/*"));
    assert!(refspec_is_safe("+refs/heads/main:refs/heads/main"));
    assert!(refspec_is_safe("+refs/pull/*:refs/pull/*"));
  }

  #[test]
  fn init_bare_creates_a_files_backend_sha1_repository() {
    // ADR 0001's boundary made explicit at creation. Git 2.45+ can create
    // `reftable` repositories, which the pinned libgit2 cannot read at all, so
    // relying on the default would make the supported format depend on the
    // installed Git version.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.git");
    init_bare(&path, Path::new("git")).unwrap();

    let format = gfs_git::read_format(&path).unwrap();
    assert_eq!(format.ref_backend, "files");
    assert_eq!(format.algorithm, gfs_types::HashAlgorithm::Sha1);
    assert!(gfs_git::verdict(&format).is_supported());
    // And it opens through the production path.
    gfs_git::Libgit2Repository::open(&path, 2, 1 << 20).unwrap();
  }

  #[test]
  fn a_fetch_with_explicit_refspecs_prunes_upstream_deletions_but_not_anchors() {
    // The end-to-end version of the refspec argument, against real Git.
    let (_up_tmp, upstream) = gfs_test::scratch_clone("basic").unwrap();
    let (_tmp, mirror) = gfs_test::scratch_clone("basic").unwrap();

    let repo = gfs_git::Libgit2Repository::open(&mirror, 2, 1 << 20).unwrap();
    let commit = {
      use gfs_git::GitRepository;
      let sel = gfs_types::RevisionSelector::parse("main", repo.algorithm()).unwrap();
      repo.resolve(&sel).unwrap().commit
    };
    let anchor = gfs_types::revision::lease_anchor_ref("m-fetch");
    {
      use gfs_git::GitRepository;
      repo.create_lease_anchor(&anchor, &commit).unwrap();
    }

    // Upstream deletes a branch, so the prune has real work to do.
    gfs_test::git(&upstream, &["update-ref", "-d", "refs/heads/feature"]).unwrap();

    fetch(&mirror, upstream.to_str().unwrap(), None, Path::new("git")).unwrap();

    let repo = gfs_git::Libgit2Repository::open(&mirror, 2, 1 << 20).unwrap();
    use gfs_git::GitRepository;
    assert_eq!(
      repo.read_lease_anchor(&anchor).unwrap(),
      Some(commit),
      "a pruning fetch must not remove the lease anchor"
    );
    let names: Vec<String> = repo
      .visible_refs()
      .unwrap()
      .into_iter()
      .map(|(n, _)| n)
      .collect();
    assert!(
      !names.iter().any(|n| n == "refs/heads/feature"),
      "the prune must have removed the deleted branch, or this test proves nothing: {names:?}"
    );
    assert!(names.iter().any(|n| n == "refs/heads/main"));
  }

  #[test]
  fn fsck_passes_on_a_healthy_mirror() {
    let (_tmp, mirror) = gfs_test::scratch_clone("basic").unwrap();
    fsck(&mirror, Path::new("git")).unwrap();
  }
}
