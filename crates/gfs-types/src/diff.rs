//! The vocabulary of a commit-to-commit diff.
//!
//! These live here, at the bottom of the graph, because four crates need the
//! same words: the repository layer produces them, the proto layer converts
//! them, the daemon relays them, and the CLI prints them. A type defined in the
//! repository layer instead would force `gfs-proto` to depend on `gfs-git`,
//! which would put libgit2 in the FUSE client's dependency graph for the sake of
//! an enum.
//!
//! # Why this is a different vocabulary from the manifest's
//!
//! `gfs_git::TreeDelta` has two cases, because a manifest is a map from path to
//! blob and "added", "modified" and "type changed" are all the same write to it.
//! [`DiffStatus`] has Git's five, because a person reviewing a commit is being
//! told what happened and those are different facts. Two vocabularies for two
//! audiences is deliberate; collapsing them would make one of the two wrong.

/// How a diff is rendered for reading.
///
/// The structured per-file summary comes back whatever this says. Only the
/// rendered bytes change.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFormat {
  /// A unified `diff --git` patch, as `git show` prints one.
  #[default]
  Patch,
  /// `git diff --stat`'s histogram.
  Stat,
  /// One status letter and a path per line.
  NameStatus,
  /// One path per line.
  NameOnly,
}

/// What happened to one path between two commits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
  Added,
  Modified,
  Deleted,
  Renamed,
  TypeChanged,
}

impl DiffStatus {
  /// The letter `git diff --name-status` prints.
  pub fn letter(self) -> char {
    match self {
      DiffStatus::Added => 'A',
      DiffStatus::Modified => 'M',
      DiffStatus::Deleted => 'D',
      DiffStatus::Renamed => 'R',
      DiffStatus::TypeChanged => 'T',
    }
  }
}

/// One file's line in a diff summary.
///
/// Not `Serialize`: a [`crate::BytePath`] is bytes and deliberately has no serde
/// impl, so anything crossing the JSON control socket carries its own wire form
/// with the paths encoded. See `gfs_mount::control`.
#[derive(Clone, Debug)]
pub struct DiffFileChange {
  pub path: crate::BytePath,
  /// Where the content came from. Present only for a rename.
  pub old_path: Option<crate::BytePath>,
  pub status: DiffStatus,
  pub additions: u32,
  pub deletions: u32,
  /// True when Git treats the content as binary, so the counts are zero and the
  /// patch carries a note rather than lines.
  pub binary: bool,
  pub old_mode: u32,
  pub new_mode: u32,
}

/// One contiguous run of lines attributed to one commit.
#[derive(Clone, Debug)]
pub struct BlameHunk {
  /// 1-based line number in the file as of the blamed commit.
  pub final_start_line: u32,
  pub lines: u32,
  /// The commit the lines are attributed to — the one `git blame` prints.
  pub commit: crate::ObjectId,
  /// What the file was called in that commit, which differs from the blamed
  /// path once the file has been renamed.
  pub orig_path: crate::BytePath,
  pub orig_start_line: u32,
  pub author: crate::Signature,
  /// True when the walk stopped here at a boundary commit rather than because
  /// the line was introduced there.
  pub boundary: bool,
}

/// Default context lines around a hunk. `git diff`'s own default.
pub const DEFAULT_CONTEXT_LINES: u32 = 3;
