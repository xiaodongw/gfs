//! The real `.git`: what the daemon seeds it with, and what it reads back.
//!
//! ADR 0009 replaced ADR 0005's synthesized read-only surface with a real git
//! directory on local disk; ADR 0011 moved that directory *inside* the
//! workspace (`<workspace>/.git`), shadowed by the mount and served back
//! through [`crate::passthrough`]. What remains here is the seeding: `HEAD`,
//! the pinned branch ref, the shipped index, the configuration Git needs to
//! be fast over a projection, and `gfs.json`, the machine-readable facts the
//! shim and the fsmonitor hook read.

use gfs_types::path::b64url_encode;
use gfs_types::{CommitMeta, MountId, ObjectId, RepositoryId, Timestamp};

/// What the seeded git dir describes.
#[derive(Clone, Debug)]
pub struct GitDirFacts {
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub tree: ObjectId,
  /// The full ref name the selector resolved to, when it named one.
  pub ref_name: Option<String>,
  pub mount_id: MountId,
  pub snapshot_time: Timestamp,
  pub grpc_endpoint: String,
  pub http_endpoint: String,
  /// The mount generation `gfs refresh` produced this surface for.
  pub generation: u64,
  /// The daemon's control socket.
  ///
  /// The shim's `status`, `diff`, and `ls-files` need the overlay journal, which
  /// only the daemon has. A local `SOCK_STREAM` at mode 0600 is not a credential
  /// and does not make the shim one -- it carries no token, and a process that
  /// can open it can already read the workspace.
  pub control_socket: std::path::PathBuf,
  /// The pinned commit's metadata, fetched once at mount time.
  ///
  /// Embedded rather than fetched on demand so that the `git` shim's bounded
  /// `log -1` needs no network and no credential: DESIGN.md section 8.6 says
  /// `GetCommit` supplies "the one commit of metadata" the shim needs, and one
  /// commit's worth is small enough to carry in the surface itself. A shim that
  /// had to call the server would need the mount capability, which would put a
  /// credential in a `PATH`-installed convenience wrapper.
  pub commit_meta: Option<CommitMeta>,
  /// Every ref the repository shows this credential, fetched once at pin time.
  ///
  /// Written as `packed-refs`: tags verbatim, branches as
  /// `refs/remotes/origin/*`. `None` means the call did not answer — an older
  /// server, or a transient failure on a repin — and the seed then leaves
  /// whatever is on disk alone rather than deleting a good ref view because one
  /// request failed.
  pub refs: Option<Vec<gfs_types::RefTarget>>,
  /// The caller's push namespace (`refs/gfs/work/<subject>`, no trailing
  /// slash), from `CreateMount`. What the seeded `origin` push refspec maps
  /// local branches into; `None` on servers that predate the field, which
  /// seeds no remote and leaves `gfs push` as the only push path.
  pub work_ref_root: Option<String>,
}

/// A commit's identity lines, with the byte-exact fields base64url-encoded.
///
/// Git does not constrain author names, emails, or commit messages to UTF-8, and
/// JSON strings must be. Encoding them keeps the bytes exact; the `_text` fields
/// alongside are a lossy convenience for a human reading the file and are never
/// what the shim prints.
fn signature_json(signature: &gfs_types::Signature) -> serde_json::Value {
  serde_json::json!({
    "name_b64url": b64url_encode(&signature.name),
    "name_text": String::from_utf8_lossy(&signature.name),
    "email_b64url": b64url_encode(&signature.email),
    "email_text": String::from_utf8_lossy(&signature.email),
    "time": { "secs": signature.time.secs, "nanos": signature.time.nanos },
    "tz_offset_minutes": signature.tz_offset_minutes,
  })
}

fn commit_json(meta: &CommitMeta) -> serde_json::Value {
  serde_json::json!({
    "parents": meta.parents.iter().map(ObjectId::to_qualified).collect::<Vec<_>>(),
    "author": signature_json(&meta.author),
    "committer": signature_json(&meta.committer),
    "message_b64url": b64url_encode(&meta.message),
    "message_text": String::from_utf8_lossy(&meta.message),
  })
}

/// `gfs.json`: the machine-readable description DESIGN.md section 8.6 asks for.
fn gfs_json(facts: &GitDirFacts) -> Vec<u8> {
  let value = serde_json::json!({
    "api_version": gfs_types::API_VERSION,
    "state_format_version": gfs_types::STATE_FORMAT_VERSION,
    "repository_id": facts.repository_id.as_str(),
    "commit": facts.commit.to_qualified(),
    "tree": facts.tree.to_qualified(),
    "ref": facts.ref_name,
    "mount_id": facts.mount_id.as_str(),
    "generation": facts.generation,
    "snapshot_time": { "secs": facts.snapshot_time.secs, "nanos": facts.snapshot_time.nanos },
    "grpc_endpoint": facts.grpc_endpoint,
    "http_endpoint": facts.http_endpoint,
    // Historical shape, kept for readers: the seeded metadata is read-only in
    // spirit (the daemon rewrites it on a repin), the workspace is not.
    "read_only": true,
    "workspace_writable": true,
    "control_socket": facts.control_socket.display().to_string(),
    "surface": "real",
    "adr": "docs/adr/0011-single-mount-workspace.md",
    "commit_meta": facts.commit_meta.as_ref().map(commit_json),
  });
  let mut bytes = serde_json::to_vec_pretty(&value).unwrap_or_else(|_| b"{}".to_vec());
  bytes.push(b'\n');
  bytes
}

// ---------------------------------------------------------------------------
// Seeding the real `.git` (ADR 0009, layout per ADR 0011)
// ---------------------------------------------------------------------------

/// Everything the seeded git dir is built from.
///
/// The workspace's `.git` is a real directory at the workspace root. The
/// object database is never fully here: `objects/info/alternates` borrows the
/// projection presented at `.git/gfs/objects`, and everything Git writes
/// (index refreshes, local commits, refs) lands on local disk where lockfiles
/// and renames behave exactly as Git expects.
#[derive(Debug)]
pub struct SeedSpec<'a> {
  /// The real git dir, `<workspace>/.git` — through the retained handle when
  /// the workspace is already mounted over.
  pub git_dir: &'a std::path::Path,
  pub facts: &'a GitDirFacts,
  /// The gateway-built index for the pinned commit. `None` reuses whatever
  /// index is already seeded, which is how a repin that failed to fetch keeps a
  /// working workspace instead of none.
  pub index: Option<&'a [u8]>,
  /// Leave `HEAD` and the branch ref exactly as they are on disk.
  ///
  /// This is the adoption path: a leftover workspace whose `HEAD` has moved
  /// past the seeded commit holds local commits, and when the pin being seeded
  /// *is* the commit they were made on, rewriting the refs is the only thing
  /// that would strand them. The caller passes `index: None` alongside, for the
  /// same reason — the on-disk index describes the local `HEAD`, not the pin.
  pub preserve_local_head: bool,
}

/// Replace a seeded file by rename, never by truncation.
///
/// `std::fs::write` truncates in place, and truncation is visible to every
/// process holding the old file — most dangerously **gitstatusd, which keeps
/// `.git/index` mmap'ed**: shrink the file under a live mapping and the next
/// page access is SIGBUS. gitstatusd dying mid-request is what turned a prompt
/// redraw into a wedged terminal on 2026-08-02 (the plugin's watchdog holds
/// the response pipe open, so the reader never even sees EOF). A rename swaps
/// the directory entry while the old inode — and any mmap of it — lives on
/// untouched, which is also how Git itself replaces the index.
///
/// The temp file sits beside the target so the rename cannot cross a
/// filesystem, and its name is unique per process so two seeding daemons
/// racing (mount vs. repin cannot race, but belt and braces) never rename each
/// other's half-written file.
fn write_atomic(target: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
  let mut temp = target.as_os_str().to_owned();
  temp.push(format!(".gfs-new.{}", std::process::id()));
  let temp = std::path::PathBuf::from(temp);
  std::fs::write(&temp, bytes)?;
  std::fs::rename(&temp, target).inspect_err(|_| {
    let _ = std::fs::remove_file(&temp);
  })
}

/// Write (or re-point) the real git dir inside a workspace.
///
/// Idempotent, and called again on every repin: `HEAD`, the branch ref, and the
/// index are rewritten to the new pin; local loose objects and any local
/// branches survive, because they are the agent's work and a re-pin is not a
/// reset. The caller is responsible for refusing the repin when [`local_head`]
/// shows unpushed commits — or, when the pin equals the commit they were made
/// on, for seeding around them with `preserve_local_head`.
pub fn seed_git_dir(spec: &SeedSpec<'_>) -> Result<(), gfs_types::error::GfsError> {
  use gfs_types::error::GfsError;
  let io = |what: &str, e: std::io::Error| {
    GfsError::internal(format!("seeding the workspace git dir ({what}): {e}"))
  };
  let dir = spec.git_dir;
  std::fs::create_dir_all(dir.join("objects/info")).map_err(|e| io("objects", e))?;
  std::fs::create_dir_all(dir.join("refs/heads")).map_err(|e| io("refs", e))?;
  std::fs::create_dir_all(dir.join("hooks")).map_err(|e| io("hooks", e))?;

  // The pinned ref view: one branch, captured at pin time. A live-projected ref
  // would move under an index that still describes the old commit, and Git
  // would report the whole repository as modified.
  let facts = spec.facts;
  match facts.ref_name.as_deref() {
    _ if spec.preserve_local_head => {}
    Some(name) if name.starts_with("refs/heads/") => {
      std::fs::write(dir.join("HEAD"), format!("ref: {name}\n")).map_err(|e| io("HEAD", e))?;
      let ref_path = dir.join(name);
      std::fs::create_dir_all(ref_path.parent().expect("refs/heads/x has a parent"))
        .map_err(|e| io("ref dir", e))?;
      std::fs::write(ref_path, format!("{}\n", facts.commit.to_hex()))
        .map_err(|e| io("branch ref", e))?;
    }
    _ => {
      std::fs::write(dir.join("HEAD"), format!("{}\n", facts.commit.to_hex()))
        .map_err(|e| io("HEAD", e))?;
    }
  }

  // Relative on purpose, and load-bearing (ADR 0011): Git resolves it against
  // the objects directory, so `objects/../gfs/objects` names the projection
  // wherever the folder happens to sit — copied to another machine included.
  // An absolute path would silently re-introduce the location dependence the
  // single-mount layout exists to remove.
  std::fs::write(
    dir.join("objects/info/alternates"),
    format!("{}\n", crate::passthrough::ALTERNATES_POINTER),
  )
  .map_err(|e| io("alternates", e))?;

  // The required configuration, and the reason each line exists. None of this
  // is enforceable -- `git -c` overrides any of it -- which is why the
  // hydration budget is mandatory (ADR 0009); this is the fast path, the
  // budget is the guarantee.
  let fsmonitor = helper_hook(dir, "gfs-fsmonitor")?;
  let lfs_filter = helper_hook(dir, "gfs-lfs-filter")?;
  // No `core.worktree`: the git dir sits inside the working tree, so Git
  // infers the parent directory — and an absolute worktree path here would
  // break the copied-folder story the relative alternates exists for.
  let mut config = String::from(
    "[core]\n\
     \trepositoryformatversion = 0\n\
     \tfilemode = true\n\
     \tbare = false\n\
     \tautocrlf = false\n\
     \tlogallrefupdates = false\n\
     # A shipped index's dev/ino/uid/gid cannot match this host; comparing them\n\
     # would re-hash the whole tree (1 615 MiB on linux, measured).\n\
     \tcheckStat = minimal\n\
     \ttrustctime = false\n\
     # The untracked cache kills the readdir walk; fsmonitor kills the lstat\n\
     # sweep. Together: 108 445 lookups to 170 on the kernel tree.\n\
     \tuntrackedCache = true\n",
  );
  if fsmonitor.is_some() {
    // Relative to the worktree root, where Git runs the hook — never the
    // daemon's own handle path, which no other process can exec, and never
    // an absolute path, which would break the copied folder (ADR 0011).
    config.push_str("\tfsmonitor = .git/hooks/gfs-fsmonitor\n");
  }
  if lfs_filter.is_some() {
    // The filter is a correctness requirement, not an optimization (ADR 0012,
    // m05d): without a clean filter, `git status` misreports every LFS file
    // and `git add` writes the expanded content into the object database.
    // `required = true` makes a broken shim fail the command loudly instead
    // of silently producing that corruption.
    // The `process` line is load-bearing beyond performance: a host with
    // git-lfs installed has `filter.lfs.process = git-lfs filter-process` in
    // its global config, and Git prefers the process form over clean/smudge
    // across scopes — real git-lfs would then hijack the driver, derive a
    // batch endpoint the gateway does not serve, and fail every checkout.
    // Only a local process entry wins that precedence (a set-but-empty one
    // poisons the driver rather than falling back; measured on Git 2.53), so
    // the shim speaks the long-running protocol and is named here. The
    // clean/smudge pair stays as documentation-by-configuration and for
    // manual invocation.
    config.push_str(
      "[filter \"lfs\"]\n\
       \tclean = .git/hooks/gfs-lfs-filter clean %f\n\
       \tsmudge = .git/hooks/gfs-lfs-filter smudge %f\n\
       \tprocess = .git/hooks/gfs-lfs-filter process\n\
       \trequired = true\n",
    );
  } else {
    // Without the shim, an expanded workspace is one `git add` away from
    // committing expanded content. Entries only expand when the server has an
    // LFS store, so a deployment that never configured one is unaffected —
    // but a missing binary in an LFS-serving deployment deserves a loud line.
    tracing::warn!(
      "gfs-lfs-filter not found next to the daemon or on PATH; \
       LFS filter config omitted — `git add` of an expanded LFS file would \
       commit its content"
    );
  }
  config.push_str(
    "# `repack -a` without `-l` copies every borrowed object out of the\n\
     # projection -- 6.6 GiB on linux, measured. Maintenance is an operator\n\
     # action against the gateway, never a side effect here.\n\
     [gc]\n\
     \tauto = 0\n\
     [maintenance]\n\
     \tauto = false\n",
  );
  if facts.work_ref_root.is_some() {
    // The push path (ADR 0009's receive-pack surface). Branches push to real
    // branches: the gateway is a fork of its upstream, its sync fast-forward
    // only, so `git push origin <branch>` lands where every other Git host
    // would put it and the next clone sees it. The credential helper reads
    // the job's own GFS_TOKEN at push time; the token itself is never written
    // to disk here.
    //
    // **No `remote.origin.push`.** It used to say `refs/heads/*:refs/heads/*`,
    // from when pushes were confined to the work namespace and the mapping had
    // to be spelled out. Once the gateway became a fork, that refspec was
    // exactly what Git already does for `git push origin <branch>` — so it
    // changed nothing except the *bare* `git push`, which it silently turned
    // into "push every local branch": a configured `remote.<name>.push`
    // overrides `push.default` entirely. An agent that makes scratch branches
    // would publish all of them by typing the most ordinary Git command there
    // is. `push.default` is named explicitly rather than left to Git's default
    // for the same reason: this config is what the workspace guarantees, and a
    // host's global `push.default = matching` would otherwise reintroduce the
    // same fan-out.
    config.push_str(&format!(
      "[remote \"origin\"]\n\
       \turl = {url}/v1/repos/{repository}\n\
       \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
       [push]\n\
       \tdefault = simple\n\
       [credential]\n\
       \thelper = \"!f() {{ echo username=x-access-token; echo password=$GFS_TOKEN; }}; f\"\n",
      url = facts.http_endpoint.trim_end_matches('/'),
      repository = facts.repository_id.as_str(),
    ));
    // The pinned branch's upstream. Without it `git status -sb` prints a bare
    // `## main` — no `...origin/main`, no ahead/behind — because a branch with
    // no configured upstream has nothing to count against, which reads as "this
    // repository does not know where it came from".
    if let Some(branch) = facts
      .ref_name
      .as_deref()
      .and_then(|n| n.strip_prefix("refs/heads/"))
    {
      config.push_str(&format!(
        "[branch \"{branch}\"]\n\
         \tremote = origin\n\
         \tmerge = refs/heads/{branch}\n"
      ));
    }
  }
  config.push_str(&format!(
    "[gfs]\n\
     \trepository = {repository}\n\
     \tcommit = {commit}\n\
     \tmount = {mount}\n\
     \tgeneration = {generation}\n",
    repository = facts.repository_id.as_str(),
    commit = facts.commit.to_qualified(),
    mount = facts.mount_id.as_str(),
    generation = facts.generation,
  ));
  write_atomic(&dir.join("config"), config.as_bytes()).map_err(|e| io("config", e))?;

  write_packed_refs(dir, facts).map_err(|e| io("packed-refs", e))?;

  if let Some(index) = spec.index {
    // Atomic above all for the *index*: this is the file gitstatusd mmaps.
    write_atomic(&dir.join("index"), index).map_err(|e| io("index", e))?;
  }

  // The same machine-readable facts the synthesized surface carried, now inside
  // the real git dir where the shim and the fsmonitor hook find them by
  // resolving the `.git` file.
  write_atomic(&dir.join("gfs.json"), &gfs_json(facts)).map_err(|e| io("gfs.json", e))?;
  Ok(())
}

/// Write the pinned ref view as `packed-refs`.
///
/// What the seed produced before this was one branch ref and `HEAD`, which is
/// why a workspace answered `git describe` with "No names found" and
/// `origin/main` with "unknown revision" — messages that read as a corrupt
/// repository rather than as a surface nobody had materialized. The gateway
/// advertises the whole set; this brings it into `.git`.
///
/// Two rules shape the mapping:
///
/// * **tags go in verbatim**, with their peel lines. `git describe`,
///   `hatch-vcs`, and `setuptools-scm` all want `refs/tags/*`, and the peel is
///   what keeps them from reading an annotated tag object out of the network
///   projection.
/// * **branches become `refs/remotes/origin/*`**, never `refs/heads/*`. A local
///   branch is the agent's own: writing upstream branches into `refs/heads/`
///   would collide with them, and packing one would resurrect a branch the
///   agent deleted. Remote-tracking is also what a real `clone` would have
///   produced, so `git log origin/main` and `@{upstream}` work the ordinary way.
///
/// Everything else the repository has — `HEAD`, notes, the mirror's own
/// remote-tracking refs — is left out: the seeded fetch refspec maps heads and
/// nothing else, and a ref the configuration cannot refresh is a ref that goes
/// stale silently.
///
/// Rewritten on every seed, so a ref deleted locally comes back on the next
/// repin. That is the same "pinned view" contract `HEAD` and the index already
/// have.
fn write_packed_refs(dir: &std::path::Path, facts: &GitDirFacts) -> Result<(), std::io::Error> {
  let Some(refs) = facts.refs.as_ref() else {
    return Ok(());
  };
  let mut lines: Vec<(String, String, Option<String>)> = Vec::with_capacity(refs.len());
  for r in refs {
    let name = if let Some(tag) = r.name.strip_prefix("refs/tags/") {
      format!("refs/tags/{tag}")
    } else if let Some(branch) = r.name.strip_prefix("refs/heads/") {
      format!("refs/remotes/origin/{branch}")
    } else {
      continue;
    };
    lines.push((
      name,
      r.target.to_hex(),
      r.peeled.as_ref().map(ObjectId::to_hex),
    ));
  }
  // Sorted because the header claims it, and Git binary-searches a file that
  // claims it.
  lines.sort();
  let mut out = String::with_capacity(lines.len() * 64 + 64);
  // The trailing space is part of the trait line's grammar: Git parses
  // space-separated traits up to the newline.
  out.push_str("# pack-refs with: peeled fully-peeled sorted \n");
  for (name, target, peeled) in &lines {
    out.push_str(target);
    out.push(' ');
    out.push_str(name);
    out.push('\n');
    if let Some(peeled) = peeled {
      out.push('^');
      out.push_str(peeled);
      out.push('\n');
    }
  }
  // Git reads this file without a lock on the read path, and a partially
  // written one is a repository that has lost refs.
  write_atomic(&dir.join("packed-refs"), out.as_bytes())
}

/// Install a helper-binary hook wrapper when the binary can be found.
///
/// The hook is a one-line shell script pointing at an absolute binary path,
/// because the config value is read by whatever Git the *job* runs and the
/// job's `PATH` is not ours to assume — while the config itself references
/// the wrapper by a worktree-relative path, so a copied folder keeps working
/// (ADR 0011). Looked for next to this process's own executable first (the
/// deployment layout), then on `PATH`. Absent: the caller decides how loud
/// that is — the fsmonitor degrades to the lstat sweep, the LFS filter warns.
fn helper_hook(
  git_dir: &std::path::Path,
  name: &str,
) -> Result<Option<std::path::PathBuf>, gfs_types::error::GfsError> {
  let binary = std::env::current_exe()
    .ok()
    .and_then(|exe| exe.parent().map(|d| d.join(name)))
    .filter(|p| p.is_file())
    .or_else(|| {
      let path = std::env::var_os("PATH")?;
      std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
    });
  let Some(binary) = binary else {
    return Ok(None);
  };
  let hook = git_dir.join(format!("hooks/{name}"));
  let script = format!("#!/bin/sh\nexec {} \"$@\"\n", binary.display());
  std::fs::write(&hook, script).map_err(|e| {
    gfs_types::error::GfsError::internal(format!("writing the {name} hook: {e}"))
  })?;
  let mut perms = std::fs::metadata(&hook)
    .map_err(|e| gfs_types::error::GfsError::internal(format!("hook metadata: {e}")))?
    .permissions();
  use std::os::unix::fs::PermissionsExt;
  perms.set_mode(0o755);
  std::fs::set_permissions(&hook, perms)
    .map_err(|e| gfs_types::error::GfsError::internal(format!("hook permissions: {e}")))?;
  Ok(Some(hook))
}

/// The commit the seeded git dir's `HEAD` currently names, if readable.
///
/// This is the repin guard: a value different from the pinned commit means the
/// agent has made local commits, and re-seeding would strand them on a branch
/// ref the next seed overwrites. Loose refs only -- `gc.auto=0` means nothing
/// packs them.
pub fn local_head(git_dir: &std::path::Path) -> Option<String> {
  let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
  let head = head.trim();
  if let Some(name) = head.strip_prefix("ref: ") {
    let target = std::fs::read_to_string(git_dir.join(name.trim())).ok()?;
    return Some(target.trim().to_owned());
  }
  Some(head.to_owned())
}

/// The commit the git dir was last seeded at, from its on-disk `gfs.json`.
///
/// This is [`local_head`]'s counterpart for the *first* mount over a leftover
/// state directory, where there is no in-memory pin to compare against: the
/// last seed recorded what it pinned, and a `HEAD` that has moved past it means
/// local commits. Returns the bare hex, to match what `local_head` reads out of
/// a loose ref. Best-effort: anything missing or unparseable is `None`, and the
/// caller treats that as "no previous seed".
pub fn seeded_commit(git_dir: &std::path::Path) -> Option<String> {
  let bytes = std::fs::read(git_dir.join("gfs.json")).ok()?;
  let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
  let qualified = value.get("commit")?.as_str()?;
  let (_, hex) = qualified.split_once(':')?;
  Some(hex.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;

  fn facts(ref_name: Option<&str>) -> GitDirFacts {
    GitDirFacts {
      repository_id: RepositoryId::parse("r-git").unwrap(),
      commit: ObjectId::from_raw(HashAlgorithm::Sha1, &[0xab; 20]).unwrap(),
      tree: ObjectId::from_raw(HashAlgorithm::Sha1, &[0xcd; 20]).unwrap(),
      ref_name: ref_name.map(str::to_owned),
      mount_id: MountId::parse("m-1").unwrap(),
      control_socket: std::path::PathBuf::from("/run/gfs/control.sock"),
      snapshot_time: Timestamp::from_secs(1_600_000_000),
      grpc_endpoint: "http://127.0.0.1:8431".to_owned(),
      http_endpoint: "http://127.0.0.1:8430".to_owned(),
      generation: 1,
      commit_meta: None,
      refs: Some(vec![
        gfs_types::RefTarget {
          name: "refs/heads/main".to_owned(),
          target: ObjectId::from_raw(HashAlgorithm::Sha1, &[0xab; 20]).unwrap(),
          peeled: None,
        },
        // An annotated tag: the target is the tag object, the peel is the
        // commit, and both have to reach `packed-refs` for `git describe` to
        // answer without reading the projection.
        gfs_types::RefTarget {
          name: "refs/tags/v1.0".to_owned(),
          target: ObjectId::from_raw(HashAlgorithm::Sha1, &[0x11; 20]).unwrap(),
          peeled: Some(ObjectId::from_raw(HashAlgorithm::Sha1, &[0xab; 20]).unwrap()),
        },
        // Neither a head nor a tag: the seeded refspec cannot refresh it, so it
        // is left out rather than materialized stale.
        gfs_types::RefTarget {
          name: "refs/notes/commits".to_owned(),
          target: ObjectId::from_raw(HashAlgorithm::Sha1, &[0x22; 20]).unwrap(),
          peeled: None,
        },
      ]),
      work_ref_root: Some("refs/gfs/work/job-owner".to_owned()),
    }
  }

  #[test]
  fn the_seed_packs_tags_verbatim_and_branches_as_remote_tracking() {
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();

    let packed = std::fs::read_to_string(git.join("packed-refs")).unwrap();
    let expected = format!(
      "# pack-refs with: peeled fully-peeled sorted \n\
       {commit} refs/remotes/origin/main\n\
       {tag} refs/tags/v1.0\n\
       ^{commit}\n",
      commit = "ab".repeat(20),
      tag = "11".repeat(20),
    );
    assert_eq!(
      packed, expected,
      "branches are remote-tracking, tags keep their name and carry their peel"
    );
    assert!(
      !packed.contains("refs/heads/"),
      "an upstream branch in refs/heads would collide with the agent's own"
    );
    assert!(
      !packed.contains("refs/notes/"),
      "a ref the seeded refspec cannot refresh is left out"
    );
    // The upstream that makes `git status -sb` print ahead/behind.
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    assert!(config.contains("[branch \"main\"]"), "{config}");
    assert!(config.contains("merge = refs/heads/main"), "{config}");
  }

  #[test]
  fn an_unanswered_ref_listing_leaves_the_previous_view_alone() {
    // A repin whose `ListRefs` failed must not delete a good ref view: an empty
    // answer and a failed one are indistinguishable at the seed, so `None`
    // means "do not touch".
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    let before = std::fs::read_to_string(git.join("packed-refs")).unwrap();

    let mut unanswered = facts(Some("refs/heads/main"));
    unanswered.refs = None;
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &unanswered,
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    assert_eq!(
      std::fs::read_to_string(git.join("packed-refs")).unwrap(),
      before
    );
  }

  #[test]
  // The documented opt-out from the workspace `deny(unsafe_code)`: `Mmap::map`
  // is unsafe because another process could truncate the file under the map —
  // which is the exact hazard this test exists to prove absent.
  #[allow(unsafe_code)]
  fn a_reseed_replaces_the_index_under_a_live_mmap_without_sigbus() {
    // gitstatusd keeps `.git/index` mmap'ed for the life of the shell session.
    // The seed used to rewrite it with `std::fs::write` — truncate-in-place —
    // and shrinking a file under a live mapping makes the next page access
    // SIGBUS: gitstatusd dies mid-request and the terminal wedges (2026-08-02).
    // A rename swaps the directory entry and leaves the mapped inode alone.
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    let big = vec![b'A'; 8192];
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: Some(&big),
      preserve_local_head: false,
    })
    .unwrap();

    // Map the seeded index the way gitstatusd does, then re-seed with a much
    // smaller one. Through a truncating write this read faults; through a
    // rename the old inode still holds every byte.
    let file = std::fs::File::open(git.join("index")).unwrap();
    let map = unsafe { memmap2::Mmap::map(&file).unwrap() };
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: Some(b"tiny"),
      preserve_local_head: false,
    })
    .unwrap();

    assert_eq!(map.len(), 8192);
    assert!(
      map.iter().all(|b| *b == b'A'),
      "the old mapping must survive the re-seed byte for byte"
    );
    assert_eq!(std::fs::read(git.join("index")).unwrap(), b"tiny");
    // And no temp file left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&git)
      .unwrap()
      .filter_map(|e| e.ok())
      .map(|e| e.file_name().to_string_lossy().into_owned())
      .filter(|n| n.contains(".gfs-new"))
      .collect();
    assert!(
      leftovers.is_empty(),
      "temp files must not linger: {leftovers:?}"
    );
  }

  #[test]
  fn the_seed_writes_a_relative_alternates_and_no_worktree() {
    // The two lines ADR 0011 legislates. A copied folder works because the
    // alternates travels with it, and would stop working if either an absolute
    // odb path or a `core.worktree` naming the old location crept back in.
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();

    let alternates = std::fs::read_to_string(git.join("objects/info/alternates")).unwrap();
    assert_eq!(alternates, "../gfs/objects\n");
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    assert!(
      !config.contains("worktree"),
      "no location dependence:\n{config}"
    );
    assert!(config.contains("checkStat = minimal"));
  }

  #[test]
  fn a_branch_seed_produces_a_symbolic_head_and_a_tag_a_detached_one() {
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join("a/.git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    assert_eq!(
      std::fs::read_to_string(git.join("HEAD")).unwrap(),
      "ref: refs/heads/main\n"
    );
    assert_eq!(
      local_head(&git).unwrap(),
      "ab".repeat(20),
      "the branch ref names the pinned commit"
    );

    let detached = tmp.path().join("b/.git");
    seed_git_dir(&SeedSpec {
      git_dir: &detached,
      facts: &facts(Some("refs/tags/v1")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    assert_eq!(
      std::fs::read_to_string(detached.join("HEAD")).unwrap(),
      format!("{}\n", "ab".repeat(20))
    );
  }

  #[test]
  fn a_preserving_seed_leaves_the_moved_head_and_refreshes_everything_else() {
    // The adoption path: local commits moved the branch ref past the seed, and
    // a re-seed at their base must not move it back — while the config and
    // `gfs.json` (endpoints, mount id, generation) are still rewritten.
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    let local = "cd".repeat(20);
    std::fs::write(git.join("refs/heads/main"), format!("{local}\n")).unwrap();

    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(Some("refs/heads/main")),
      index: None,
      preserve_local_head: true,
    })
    .unwrap();
    assert_eq!(local_head(&git).unwrap(), local, "the local commit survives");
    assert_eq!(
      seeded_commit(&git).unwrap(),
      "ab".repeat(20),
      "gfs.json still names the base the seed pinned"
    );
  }

  #[test]
  fn gfs_json_is_valid_json_naming_the_pinned_commit() {
    let bytes = gfs_json(&facts(Some("refs/heads/main")));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
      value["commit"],
      "sha1:abababababababababababababababababababab"
    );
    assert_eq!(value["control_socket"], "/run/gfs/control.sock");
    // What `seeded_commit` reads back for the adoption guard.
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    seed_git_dir(&SeedSpec {
      git_dir: &git,
      facts: &facts(None),
      index: None,
      preserve_local_head: false,
    })
    .unwrap();
    assert_eq!(seeded_commit(&git).unwrap(), "ab".repeat(20));
  }
}
