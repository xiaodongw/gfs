//! The sandboxed `git receive-pack` subprocess contract.
//!
//! The push half of what ADR 0009 left open: a workspace's real `.git`
//! accumulates local commits, and `git push` to the gateway is how they leave
//! it as a pack — replacing `CommitChanges`' file-by-file re-upload for
//! callers that speak Git. The RPC surface stays for callers that do not.
//!
//! # The authorization model
//!
//! Same repository authorization as every other surface, plus one thing this
//! surface adds: a pusher may update **real branches**, `refs/heads/…`, and
//! their own work-branch subtree, `refs/gfs/work/<subject>/…` — nothing else.
//! Branches are safe to accept because the upstream sync is fast-forward-only
//! ([`crate::mirror`]): pushed work is never overwritten by a fetch, exactly a
//! fork's contract. Everything under `refs/heads/` is shared by every caller of
//! this repository, the way a shared fork is; protected branches are the
//! eventual refinement of that, not per-caller hiding. That boundary is
//! enforced twice, on the same defence-in-depth grounds as upload-pack's
//! filter validation:
//!
//! * `receive.hideRefs=refs/` followed by `!refs/heads/` and `!<own subtree>` —
//!   Git checks the list back to front, so the negations win for branches and
//!   the caller's subtree and everything else on the wire is "deny updating a
//!   hidden ref". Tags stay fetch-owned: they keep mirroring upstream, pruned
//!   and forced, so a pushed tag would be silently lost — hiding them keeps
//!   that a refusal at the door instead.
//! * [`ReceivePack::validate_commands`] parses the command section before the
//!   child is spawned and refuses by name, so an out-of-tree push is a legible
//!   `PERMISSION_DENIED` in the audit log rather than a Git-worded rejection.
//!
//! The advertisement is scanned like upload-pack's, with the caller's own
//! subtree as the one permitted appearance of the reserved namespace.
//!
//! # What receive-pack must not do to the projection
//!
//! `receive.autogc` is off (with `gc.auto=0` behind it): a post-receive gc
//! could repack away files that live mounts' odb projections still reference,
//! and ADR 0009's retention policy makes maintenance an operator action, never
//! a push side effect. Quarantine migration only *adds* pack and loose files,
//! which the manifest model tolerates by construction.
//!
//! Deletes and non-fast-forwards are left at Git's defaults (allowed): both
//! are ordinary operations on one's own work branch, and mounts pin commits
//! through lease anchors, not through work refs, so neither can strand a
//! mount.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use gfs_types::error::{ErrorCode, GfsError};

use super::pkt::{self, Packet};
use super::upload_pack::{protected_env, GitProtocol, Mode, UploadPackPolicy};

/// The most ref updates one push may carry. Far above any legitimate workflow
/// on a work branch; a bound so the pre-spawn parse is never the amplifier.
const MAX_COMMANDS: usize = 1_000;

/// One repository plus one caller's push boundary, ready to serve receive-pack.
#[derive(Debug, Clone)]
pub struct ReceivePack {
  repo: PathBuf,
  policy: UploadPackPolicy,
  /// The caller's work-ref subtree, no trailing slash (`refs/gfs/work/alice`).
  work_root: String,
}

impl ReceivePack {
  /// Bind to a bare repository path from the **catalog** and a work root from
  /// the **authenticated identity** — neither ever comes from the request.
  pub fn new(repo: &Path, policy: UploadPackPolicy, work_root: String) -> Result<Self, GfsError> {
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
    Ok(ReceivePack {
      repo,
      policy,
      work_root,
    })
  }

  pub fn policy(&self) -> &UploadPackPolicy {
    &self.policy
  }

  pub fn work_root(&self) -> &str {
    &self.work_root
  }

  /// The hardened configuration, as data so tests can assert on it exactly.
  pub fn config(&self) -> Vec<String> {
    let mut config: Vec<String> = Vec::new();
    let mut push = |key: &str, value: &str| {
      config.push("-c".to_owned());
      config.push(format!("{key}={value}"));
    };

    // Order matters and is load-bearing: Git evaluates hideRefs back to front,
    // so the negations must be appended *after* the blanket entry.
    // Command-line `-c` values are read after the repository's own config, so
    // a repository cannot append a wider negation that wins.
    push("receive.hideRefs", "refs/");
    push("receive.hideRefs", "!refs/heads/");
    push("receive.hideRefs", &format!("!{}", self.work_root));

    // A push that brings corrupt objects is refused at the door rather than
    // stored and discovered at fsck.
    push("transfer.fsckObjects", "true");
    // No maintenance as a side effect of a push. A gc here could delete packs
    // that live odb projections reference (ADR 0009's retention policy).
    push("receive.autogc", "false");
    push("gc.auto", "0");
    push("core.hooksPath", "/dev/null");

    config
  }

  /// The full argument vector for one invocation.
  pub fn argv(&self, mode: Mode) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    if let (Some(cpu), Some(prlimit)) = (
      self.policy.limits.cpu_seconds,
      super::upload_pack::prlimit_path(),
    ) {
      argv.push(prlimit.into());
      argv.push(format!("--cpu={cpu}").into());
      argv.push("--".into());
    }
    argv.push("git".into());
    argv.extend(self.config().into_iter().map(Into::into));
    argv.push("receive-pack".into());
    argv.push(match mode {
      Mode::Advertise => "--http-backend-info-refs".into(),
      Mode::StatelessRpc => "--stateless-rpc".into(),
    });
    argv.push(self.repo.clone().into());
    argv
  }

  /// Spawn the child with piped stdio.
  ///
  /// Receive-pack has no protocol v2, so the environment never carries
  /// `GIT_PROTOCOL`; a v2-requesting client falls back on its own.
  pub fn spawn(&self, mode: Mode) -> Result<tokio::process::Child, GfsError> {
    let argv = self.argv(mode);
    let mut command = tokio::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.env_clear();
    for (key, value) in protected_env(GitProtocol::V0) {
      command.env(key, value);
    }
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
        format!("cannot start receive-pack: {e}"),
      )
    })
  }

  /// Refuse a push whose ref updates leave the branch namespace and the
  /// caller's work subtree.
  ///
  /// Runs before a subprocess exists. Only the command section is parsed — the
  /// packfile that follows the flush is raw bytes and is the child's to parse.
  /// A command line is `<old-oid> SP <new-oid> SP <refname>`, the first one
  /// carrying `NUL<capabilities>`.
  pub fn validate_commands(&self, request: &[u8]) -> Result<(), GfsError> {
    let packets = pkt::decode_until_flush(request)?;
    let boundary = format!("{}/", self.work_root);
    let mut commands = 0usize;
    for packet in &packets {
      let Packet::Data(payload) = packet else {
        continue;
      };
      // Capabilities are the client's feature list, not a ref update.
      let line = payload.split(|&b| b == 0).next().unwrap_or_default();
      let text = String::from_utf8_lossy(line);
      let text = text.trim_end_matches('\n');
      // Anything that is not a three-field command (e.g. a push-cert block) is
      // left to the child; the hideRefs config still bounds what it can do.
      let mut fields = text.splitn(3, ' ');
      let (Some(old), Some(new), Some(refname)) = (fields.next(), fields.next(), fields.next())
      else {
        continue;
      };
      if !is_hex_oid(old) || !is_hex_oid(new) {
        continue;
      }
      commands += 1;
      if commands > MAX_COMMANDS {
        return Err(GfsError::new(
          ErrorCode::ResourceLimit,
          format!("a push may update at most {MAX_COMMANDS} refs"),
        ));
      }
      let is_branch = refname
        .strip_prefix("refs/heads/")
        .is_some_and(gfs_types::revision::is_valid_branch_name);
      if !is_branch && !refname.starts_with(&boundary) {
        return Err(GfsError::new(
          ErrorCode::PermissionDenied,
          format!(
            "push may only update refs/heads/* or {boundary}*; tags and \
             everything else follow upstream and are written by fetch, not by \
             push"
          ),
        ));
      }
    }
    Ok(())
  }

  /// Redact the server's filesystem layout out of a child's stderr.
  pub fn redact(&self, stderr: &str) -> String {
    let path = self.repo.display().to_string();
    let redacted = stderr.replace(&path, "<repository>");
    match self.repo.parent() {
      Some(parent) => redacted.replace(&parent.display().to_string(), "<root>"),
      None => redacted,
    }
  }
}

fn is_hex_oid(s: &str) -> bool {
  (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn pack(root: &str) -> (tempfile::TempDir, ReceivePack) {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("objects")).unwrap();
    let pack = ReceivePack::new(repo.path(), UploadPackPolicy::default(), root.to_owned()).unwrap();
    (repo, pack)
  }

  #[test]
  fn the_config_hides_everything_then_unhides_branches_and_the_callers_subtree() {
    let (_tmp, pack) = pack("refs/gfs/work/alice");
    let config = pack.config();
    let rendered = config.join(" ");
    // All entries present, blanket first: Git checks the list back to front,
    // so the negations only win because they come after it.
    let blanket = rendered.find("receive.hideRefs=refs/ ").expect("blanket");
    let branches = rendered
      .find("receive.hideRefs=!refs/heads/")
      .expect("branch negation");
    let negation = rendered
      .find("receive.hideRefs=!refs/gfs/work/alice")
      .expect("negation");
    assert!(
      blanket < branches && blanket < negation,
      "the negations must come after the blanket"
    );
    assert!(rendered.contains("receive.autogc=false"));
    assert!(rendered.contains("gc.auto=0"));
    assert!(rendered.contains("transfer.fsckObjects=true"));
    assert!(rendered.contains("core.hooksPath=/dev/null"));
  }

  #[test]
  fn command_validation_confines_pushes_to_branches_and_the_work_subtree() {
    let (_tmp, pack) = pack("refs/gfs/work/alice");
    let body = |refname: &str| {
      let line = format!(
        "{} {} {refname}\0report-status\n",
        "1".repeat(40),
        "2".repeat(40)
      );
      let mut out = pkt::pkt_line(line.as_bytes());
      out.extend_from_slice(pkt::FLUSH_PKT);
      // Raw pack bytes after the flush must not confuse the parse.
      out.extend_from_slice(b"PACK\x00\x00\x00\x02");
      out
    };
    for allowed in [
      // The fork contract: real branches take pushes.
      "refs/heads/main",
      "refs/heads/feature/nested",
      "refs/gfs/work/alice/feature",
    ] {
      assert!(pack.validate_commands(&body(allowed)).is_ok(), "{allowed}");
    }
    for refused in [
      "refs/tags/v1",
      "refs/gfs/work/bob/feature",
      "refs/gfs/mounts/m-1",
      // A prefix that shares the root's spelling without being inside it.
      "refs/gfs/work/alice2/feature",
      // Branch-shaped but not a usable branch name: reaches a git command
      // line elsewhere, so the same gate applies here.
      "refs/heads/-flag",
      "refs/heads/a..b",
    ] {
      let err = pack.validate_commands(&body(refused)).unwrap_err();
      assert_eq!(err.code, ErrorCode::PermissionDenied, "{refused}");
    }
  }

  #[test]
  fn argv_is_receive_pack_with_no_user_controlled_argument() {
    let (_tmp, pack) = pack("refs/gfs/work/alice");
    let argv = pack.argv(Mode::StatelessRpc);
    let rendered: Vec<String> = argv
      .iter()
      .map(|a| a.to_string_lossy().into_owned())
      .collect();
    assert!(rendered[0].ends_with("git") || rendered[0].ends_with("prlimit"));
    assert!(rendered.contains(&"receive-pack".to_owned()));
    assert!(rendered.contains(&"--stateless-rpc".to_owned()));
    assert!(!rendered.iter().any(|a| a.contains("sh -c") || a == "sh"));
  }
}
