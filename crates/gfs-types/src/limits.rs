//! Limits and fixed policy constants.
//!
//! Two kinds of value live here, and the difference matters.
//!
//! The **limits** are engineering defaults: page sizes, byte caps, component
//! counts. They can be tuned from measurement without a review.
//!
//! The **lease policy** is not. ADR 0006 fixed those six numbers with a stated
//! reason for each, precisely so they would not be rediscovered during
//! implementation. They appear here as named constants so that changing one is
//! visibly a change to a reviewed decision rather than an edit to a config
//! default.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Request and value limits
// ---------------------------------------------------------------------------

/// Longest accepted repository path, in bytes. Linux's `PATH_MAX` is 4096 and no
/// Git tree in the M0.1 corpus comes near it.
pub const MAX_PATH_BYTES: usize = 4096;

/// Longest accepted single path component, in bytes.
///
/// POSIX's `NAME_MAX`, which every ordinary Linux filesystem enforces. Git does
/// not impose one, so this is a deliberate narrowing: a workspace that accepted a
/// 300-byte file name would produce a tree that cannot be checked out anywhere
/// else, and the failure would arrive at `git checkout` on someone else's machine
/// rather than at the write that caused it.
///
/// Reported by `statfs` as `f_namemax`, which is what makes the kernel refuse an
/// over-long component before it ever becomes an upcall.
pub const MAX_NAME_BYTES: usize = 255;

/// Deepest accepted path. Guards tree traversal, which walks one tree object per
/// component and therefore does one object read per component.
pub const MAX_PATH_COMPONENTS: usize = 256;

pub const MAX_REPOSITORY_ID_BYTES: usize = 128;
pub const MAX_DISPLAY_NAME_BYTES: usize = 512;
pub const MAX_MOUNT_ID_BYTES: usize = 64;
pub const MAX_SUBJECT_ID_BYTES: usize = 512;
pub const MAX_REVISION_SELECTOR_BYTES: usize = 512;

/// Furthest a `~n`/`^n` chain may walk from its base commit.
///
/// An ancestry walk is one parent read per hop and cannot be paged, so a
/// caller asking for `main~9000000` would be asking for a revwalk dressed as a
/// selector. Well above anything a person types and far below a history.
pub const MAX_ANCESTRY_DISTANCE: u32 = 4096;

/// Shortest hex prefix accepted as an abbreviated object ID.
///
/// Git's own default is 7 and it grows the printed abbreviation as a repository
/// gets larger. 7 is kept as the *accepted* floor because that is what a human
/// pastes from `git log --oneline`; resolution still fails on ambiguity rather
/// than choosing, so a too-short prefix is a clear error and not a wrong answer.
pub const MIN_ABBREV_HEX: usize = 7;

/// Default and maximum entries per `ListDirectory` page.
///
/// The maximum exists because a Git tree can be enormous -- the M0.1 corpus has
/// single directories in the thousands and the exit criteria require paging a
/// million-entry snapshot -- and an unbounded page turns one request into an
/// unbounded response.
pub const DEFAULT_DIRECTORY_PAGE_SIZE: usize = 1000;
pub const MAX_DIRECTORY_PAGE_SIZE: usize = 10_000;

/// Maximum paths in one `BatchGetEntry`. Each one is an independent tree walk.
pub const MAX_BATCH_ENTRIES: usize = 1000;

/// Commits returned by one `Log` page.
///
/// The default is what `git log` shows on a terminal before a human stops
/// reading, and the ceiling keeps a revwalk over the M0.1 worst case — 1.4
/// million commits — from becoming one response.
pub const DEFAULT_LOG_LIMIT: usize = 50;
pub const MAX_LOG_LIMIT: usize = 1000;

/// Paths returned by one `FindPaths` page.
///
/// Larger than a directory page because a filename search is answered against
/// the whole tree and a caller that asked for `*.py` across a monorepo wants the
/// set, not a tour of it. Still bounded: the M0.1 worst case has 94 751 files at
/// tip and the glob may match all of them.
pub const DEFAULT_FIND_PATHS_LIMIT: usize = 10_000;
pub const MAX_FIND_PATHS_LIMIT: usize = 100_000;

/// Largest blob the API will return in a single whole-blob response.
///
/// DESIGN.md section 7.4 takes whole-blob fetch for the MVP, so this is also the
/// largest file the FUSE client can open. It is separate from
/// [`MAX_SEARCHABLE_BLOB_BYTES`] because a file can legitimately be served and
/// not indexed.
pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;

/// Blobs larger than this are excluded from content search and reported as a
/// coverage exclusion, never silently omitted (DESIGN.md section 7.5).
pub const MAX_SEARCHABLE_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// Cap on a single non-blob response body, so a pathological tree cannot make
/// one request consume unbounded server memory.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Rendered bytes in one commit-to-commit diff.
///
/// The default is what a person or an agent will actually read: a vendored
/// dependency bump is megabytes of patch that nobody reviews line by line, and
/// the truncation flag says so rather than the response quietly stopping. The
/// ceiling keeps one `gfs show` of a squashed import from becoming a 400 MB
/// response.
pub const DEFAULT_DIFF_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DIFF_BYTES: usize = 64 * 1024 * 1024;

/// Context lines around a diff hunk. Git's own `-U` has no ceiling; this one
/// exists because context is emitted per hunk, so a large value multiplies the
/// response by the number of hunks.
pub const MAX_DIFF_CONTEXT_LINES: u32 = 1000;

/// Default per-request deadline when the caller supplies none.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded blocking pool sizing for libgit2 work.
///
/// ADR 0001's handle model makes this admission control rather than mere reuse:
/// when every handle is busy a caller waits instead of adding another concurrent
/// pack read. The default is per repository, not per process.
pub const DEFAULT_REPO_HANDLES: usize = 8;

/// Bounded decoded-tree cache, in entries. DESIGN.md section 7.4 caches decoded
/// trees by OID; M1.3 requires the bound.
pub const DEFAULT_TREE_CACHE_ENTRIES: usize = 4096;

// ---------------------------------------------------------------------------
// Mount retention-lease policy (ADR 0006)
// ---------------------------------------------------------------------------

/// The retention-lease policy. Every field is a decision recorded in ADR 0006's
/// table, with its reasoning, not a tunable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LeasePolicy {
  /// Long enough that a brief control-plane outage is survivable; short enough
  /// that an orphan is reclaimed within a shift.
  pub initial_ttl: Duration,
  /// Six renewals per TTL, so five consecutive failures are tolerated before
  /// grace begins.
  pub heartbeat_interval: Duration,
  /// A transient failure gets reported and recovered before a live workspace is
  /// destroyed.
  pub renewal_grace: Duration,
  /// Objects stay recoverable for a working day after a mistaken expiry.
  pub prune_delay: Duration,
  /// An abandoned daemon that renews forever is still bounded. Renewal past
  /// this fails and alerts.
  pub max_total_age: Duration,
  /// Fires roughly ten minutes before grace begins.
  pub alert_after_failures: u32,
}

impl LeasePolicy {
  pub const fn adr_0006() -> Self {
    LeasePolicy {
      initial_ttl: Duration::from_secs(30 * 60),
      heartbeat_interval: Duration::from_secs(5 * 60),
      renewal_grace: Duration::from_secs(15 * 60),
      prune_delay: Duration::from_secs(24 * 60 * 60),
      max_total_age: Duration::from_secs(24 * 60 * 60),
      alert_after_failures: 2,
    }
  }
}

impl Default for LeasePolicy {
  fn default() -> Self {
    LeasePolicy::adr_0006()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lease_policy_matches_the_recorded_decision() {
    // Not a tautology: this asserts the constants still say what ADR 0006's
    // table says, in the units the table uses. If someone tunes one of these
    // the way an ordinary default gets tuned, this fails and points at the ADR.
    let p = LeasePolicy::adr_0006();
    assert_eq!(p.initial_ttl.as_secs() / 60, 30, "initial TTL, minutes");
    assert_eq!(p.heartbeat_interval.as_secs() / 60, 5, "heartbeat, minutes");
    assert_eq!(p.renewal_grace.as_secs() / 60, 15, "grace, minutes");
    assert_eq!(p.prune_delay.as_secs() / 3600, 24, "prune delay, hours");
    assert_eq!(p.max_total_age.as_secs() / 3600, 24, "max age, hours");
    assert_eq!(p.alert_after_failures, 2);
  }

  #[test]
  fn heartbeat_tolerates_the_failures_the_adr_claims() {
    let p = LeasePolicy::adr_0006();
    // ADR 0006's stated reasoning: "six renewals per TTL, so five consecutive
    // failures are tolerated before grace". Check the arithmetic actually holds
    // rather than trusting the prose.
    let renewals_per_ttl = p.initial_ttl.as_secs() / p.heartbeat_interval.as_secs();
    assert_eq!(renewals_per_ttl, 6);
    assert!(p.alert_after_failures < renewals_per_ttl as u32);
    // And the alert must fire before grace ends, or it is not actionable.
    let time_to_alert = p.heartbeat_interval * p.alert_after_failures;
    assert!(time_to_alert < p.initial_ttl + p.renewal_grace);
  }
}
