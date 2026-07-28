//! The synthesized read-only `.git` surface.
//!
//! ADR 0005 chose this over a real shallow blobless partial clone, and corrected
//! DESIGN.md section 8.6's contents while doing so. The measurement behind the
//! choice: a partial clone of the Linux kernel costs a 9.7 MiB per-job index and
//! **101 180 `stat` syscalls per `git status`**, which inside a mount become 94 850
//! distinct first-time FUSE lookups — a full metadata sweep of the monorepo, which
//! is the exact cost GFS exists to avoid.
//!
//! # Six entries, not four
//!
//! DESIGN.md listed `HEAD`, `packed-refs`, `config`, and `gfs.json`. ADR 0005
//! measured that with exactly those four **Git does not recognize the directory as
//! a repository at all**, and every command fails with `not a git repository`, so
//! the surface satisfies nothing. Empty `objects/` and `refs/` directories are what
//! make repository detection work, and they are load-bearing rather than
//! decorative.
//!
//! # What works and what does not
//!
//! Measured in `spikes/git-surface` and recorded in ADR 0005:
//!
//! | Command | Result |
//! | --- | --- |
//! | `rev-parse --show-toplevel`, `--git-dir`, `HEAD`, `--abbrev-ref HEAD` | works |
//! | `symbolic-ref --short HEAD` | works |
//! | `status`, `log -1`, `show HEAD:<path>`, `cat-file -t HEAD` | fails visibly |
//! | `ls-files`, `diff --stat` | **exit 0, empty output — silently wrong** |
//!
//! The last row is why the `git` shim is a correctness requirement and not a
//! usability measure. A tool asking "what files are tracked?" is told "none".
//! Tools that invoke Git by absolute path bypass the shim and see that behaviour;
//! that is a documented limitation of the MVP boundary, not a bug to be fixed
//! here.
//!
//! # Collision safety
//!
//! Git refuses to record a tree entry named `.git` at any level, so nothing in the
//! pinned commit can shadow or be shadowed by this surface.

use std::collections::BTreeMap;
use std::sync::Arc;

use gfs_types::path::b64url_encode;
use gfs_types::{BytePath, CommitMeta, MountId, ObjectId, RepositoryId, Timestamp};

/// The directory name, and the reason there is no configuration for it: a
/// different name would not be found by any of the tooling this exists for.
pub const GIT_DIR: &[u8] = b".git";

#[derive(Clone, Debug)]
pub enum SynthNode {
  Dir,
  File(Arc<Vec<u8>>),
}

impl SynthNode {
  pub fn size(&self) -> u64 {
    match self {
      SynthNode::Dir => 0,
      SynthNode::File(bytes) => bytes.len() as u64,
    }
  }

  pub fn is_dir(&self) -> bool {
    matches!(self, SynthNode::Dir)
  }
}

/// What the surface describes.
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
}

#[derive(Debug)]
pub struct GitDir {
  nodes: BTreeMap<Vec<u8>, SynthNode>,
}

impl GitDir {
  pub fn new(facts: &GitDirFacts) -> Self {
    let mut nodes = BTreeMap::new();
    nodes.insert(GIT_DIR.to_vec(), SynthNode::Dir);
    nodes.insert(b".git/objects".to_vec(), SynthNode::Dir);
    nodes.insert(b".git/refs".to_vec(), SynthNode::Dir);
    nodes.insert(
      b".git/HEAD".to_vec(),
      SynthNode::File(Arc::new(head(facts))),
    );
    nodes.insert(
      b".git/packed-refs".to_vec(),
      SynthNode::File(Arc::new(packed_refs(facts))),
    );
    nodes.insert(
      b".git/config".to_vec(),
      SynthNode::File(Arc::new(config(facts))),
    );
    nodes.insert(
      b".git/gfs.json".to_vec(),
      SynthNode::File(Arc::new(gfs_json(facts))),
    );
    GitDir { nodes }
  }

  /// Whether a path is inside the synthesized surface.
  pub fn owns(path: &BytePath) -> bool {
    let bytes = path.as_bytes();
    bytes == GIT_DIR || bytes.starts_with(b".git/")
  }

  pub fn get(&self, path: &BytePath) -> Option<SynthNode> {
    self.nodes.get(path.as_bytes()).cloned()
  }

  /// The immediate children of a synthesized directory, in Git's byte order.
  pub fn children(&self, path: &BytePath) -> Vec<(Vec<u8>, SynthNode)> {
    let mut prefix = path.as_bytes().to_vec();
    prefix.push(b'/');
    self
      .nodes
      .iter()
      .filter_map(|(candidate, node)| {
        let rest = candidate.strip_prefix(prefix.as_slice())?;
        // Immediate children only.
        if rest.is_empty() || rest.contains(&b'/') {
          return None;
        }
        Some((rest.to_vec(), node.clone()))
      })
      .collect()
  }
}

/// `HEAD`, symbolic when the selector named a branch.
///
/// A tag or a bare object ID produces a detached `HEAD` holding the raw hex,
/// because a symbolic ref to `refs/tags/...` would make `git symbolic-ref
/// --short HEAD` report a tag as if it were the current branch.
fn head(facts: &GitDirFacts) -> Vec<u8> {
  match facts.ref_name.as_deref() {
    Some(name) if name.starts_with("refs/heads/") => format!("ref: {name}\n").into_bytes(),
    _ => format!("{}\n", facts.commit.to_hex()).into_bytes(),
  }
}

/// `packed-refs` in stock Git's format, including the trailing space Git writes
/// after `sorted`.
fn packed_refs(facts: &GitDirFacts) -> Vec<u8> {
  let mut out = String::from("# pack-refs with: peeled fully-peeled sorted \n");
  if let Some(name) = &facts.ref_name {
    out.push_str(&format!("{} {}\n", facts.commit.to_hex(), name));
  }
  out.into_bytes()
}

/// A minimal `config`.
///
/// `repositoryformatversion = 0` and no `extensions.*`: a version Git does not
/// recognize makes it refuse the repository outright, which would defeat the root
/// discovery this surface exists to provide. `bare = false` so the mount root is
/// inferred as the working tree.
///
/// The `[gfs]` section is inert to Git — an unknown section is ignored — and
/// gives a human reading `.git/config` the same facts `gfs.json` carries.
fn config(facts: &GitDirFacts) -> Vec<u8> {
  format!(
    "[core]\n\
     \trepositoryformatversion = 0\n\
     \tfilemode = true\n\
     \tbare = false\n\
     \tlogallrefupdates = false\n\
     [gfs]\n\
     \trepository = {repository}\n\
     \tcommit = {commit}\n\
     \tmount = {mount}\n\
     \tgeneration = {generation}\n\
     \tendpoint = {endpoint}\n",
    repository = facts.repository_id.as_str(),
    commit = facts.commit.to_qualified(),
    mount = facts.mount_id.as_str(),
    generation = facts.generation,
    endpoint = facts.grpc_endpoint,
  )
  .into_bytes()
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
    // The `.git` surface is read-only; the workspace around it is not.
    "read_only": true,
    "workspace_writable": true,
    "control_socket": facts.control_socket.display().to_string(),
    "surface": "synthesized",
    "adr": "docs/adr/0005-git-command-surface.md",
    "commit_meta": facts.commit_meta.as_ref().map(commit_json),
  });
  let mut bytes = serde_json::to_vec_pretty(&value).unwrap_or_else(|_| b"{}".to_vec());
  bytes.push(b'\n');
  bytes
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
    }
  }

  #[test]
  fn the_surface_has_exactly_the_six_entries_adr_0005_specifies() {
    let dir = GitDir::new(&facts(Some("refs/heads/main")));
    let mut names: Vec<String> = dir
      .children(&BytePath::new(GIT_DIR.to_vec()))
      .into_iter()
      .map(|(name, _)| String::from_utf8(name).unwrap())
      .collect();
    names.sort();
    assert_eq!(
      names,
      // Compared against a sorted `names`, so this list is in byte order, not
      // in the order `children` happens to emit. `gfs.json` sorts before
      // `objects`; the old `xvfs.json` sorted last, which is why the rename
      // moved it.
      vec![
        "HEAD",
        "config",
        "gfs.json",
        "objects",
        "packed-refs",
        "refs"
      ]
    );
  }

  #[test]
  fn objects_and_refs_are_directories() {
    // The two entries DESIGN.md omitted. Without them Git does not recognize the
    // directory as a repository at all, so this assertion is the ADR's correction.
    let dir = GitDir::new(&facts(Some("refs/heads/main")));
    assert!(dir
      .get(&BytePath::new(b".git/objects".to_vec()))
      .unwrap()
      .is_dir());
    assert!(dir
      .get(&BytePath::new(b".git/refs".to_vec()))
      .unwrap()
      .is_dir());
  }

  #[test]
  fn a_branch_produces_a_symbolic_head() {
    let bytes = head(&facts(Some("refs/heads/main")));
    assert_eq!(bytes, b"ref: refs/heads/main\n");
  }

  #[test]
  fn a_tag_or_object_id_produces_a_detached_head() {
    // `symbolic-ref --short HEAD` on a tag would otherwise report the tag as the
    // current branch, which is a wrong answer rather than a missing one.
    assert_eq!(
      head(&facts(Some("refs/tags/v1"))),
      b"abababababababababababababababababababab\n"
    );
    assert_eq!(
      head(&facts(None)),
      b"abababababababababababababababababababab\n"
    );
  }

  #[test]
  fn packed_refs_uses_stock_gits_header_including_its_trailing_space() {
    let bytes = packed_refs(&facts(Some("refs/heads/main")));
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("# pack-refs with: peeled fully-peeled sorted \n"));
    assert!(text.contains("abababababababababababababababababababab refs/heads/main\n"));
  }

  #[test]
  fn the_surface_owns_only_dot_git_paths() {
    assert!(GitDir::owns(&BytePath::new(b".git".to_vec())));
    assert!(GitDir::owns(&BytePath::new(b".git/HEAD".to_vec())));
    // A real tracked file whose name merely starts with the same bytes.
    assert!(!GitDir::owns(&BytePath::new(b".gitignore".to_vec())));
    assert!(!GitDir::owns(&BytePath::new(b"src/.gitkeep".to_vec())));
  }

  #[test]
  fn gfs_json_is_valid_json_naming_the_pinned_commit() {
    let bytes = gfs_json(&facts(Some("refs/heads/main")));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
      value["commit"],
      "sha1:abababababababababababababababababababab"
    );
    assert_eq!(value["read_only"], true);
  }
}
