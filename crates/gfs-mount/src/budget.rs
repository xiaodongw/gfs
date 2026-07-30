//! The hard hydration budget: the one limit a job cannot talk its way past.
//!
//! DESIGN.md section 8.4 originally made hard budgets opt-in, "because a build may
//! legitimately need many files". [ADR
//! 0009](../../../docs/adr/0009-raw-git-over-a-projected-object-store.md) makes
//! them mandatory, and the reason is worth keeping next to the code: once stock
//! Git runs inside the mount, the configuration that keeps it cheap is not
//! enforceable. `core.checkStat`, `core.fsmonitor` and `gc.auto` are all
//! overridable per invocation — `git -c core.checkStat=default status` costs
//! 1 615 MiB on the Linux kernel — so no amount of configuring the workspace
//! bounds what a caller does to it. Counting bytes at the filesystem does.
//!
//! Four properties are load-bearing rather than incidental, each measured in
//! `spikes/reports/m05b-git-projection.md`.
//!
//! **`EDQUOT`, chosen for its `strerror`.** A FUSE reply carries an errno and no
//! prose, so the only text the caller prints is whatever `strerror` says. Measured
//! against the probe, `grep` prints "Disk quota exceeded" and the path, `rg` adds
//! "(os error 122)", and Python raises `OSError [Errno 122]`. All three name a
//! quota, which is true and actionable. `EIO` would say "Input/output error" and
//! send an agent looking for a corrupt filesystem.
//!
//! **Refuse at `open`, not at `read`.** With refusal in `read`, `grep -r` did not
//! abort — it printed one error per file and kept walking, thousands of identical
//! lines. Refusing the open makes the tool stop. In this crate that falls out of
//! where hydration happens: a base blob is fetched whole by `Gfs::open_blob`
//! during `open`, so refusing there refuses the open.
//!
//! **Charge each blob once.** A cache eviction followed by a re-read must not
//! charge twice, or a job dies for the cache's behaviour rather than its own. The
//! budget therefore measures *how much of the repository a job looked at*, not how
//! many times bytes crossed the network. Re-fetches are counted separately,
//! because their ratio to unique bytes is the thrash signal a residency budget
//! needs — a cyclic scan larger than the cache re-fetches forever under LRU, which
//! presents as a slow job rather than an error.
//!
//! **Cached reads are free.** Section 8.4 limits "new remote hydration while
//! preserving overlay and cached access", so a blob another mount already
//! published costs this job nothing. That is the amortization ADR 0008 built the
//! shared cache for.
//!
//! # What this deliberately does not do
//!
//! No process identity. It is a name check rather than a boundary — renaming a
//! binary or reaching for `python3` defeats it, `/proc` lookups are racy, and
//! readahead does not carry the caller's pid. Byte counting discriminates without
//! a deny list: measured, a configured `git status` never touches the budget at
//! all while `grep -r` trips it inside 2 MB.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::ObjectId;

/// A per-job ceiling on bytes hydrated from the server.
///
/// Per *job*, not per pin: `gfs switch` replaces the pinned commit and does not
/// reset what the job has already spent, because the disk and the network do not
/// care which commit the bytes belonged to.
#[derive(Debug)]
pub struct HydrationBudget {
  /// Zero means unlimited, which is the only way to turn the budget off. An
  /// `Option` would make "off" and "zero bytes allowed" the same shape, and one
  /// of those must refuse everything.
  limit: u64,
  state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
  /// Unique bytes charged. Never decreases.
  charged: u64,
  /// Digests of blobs already charged, so a re-fetch is free.
  ///
  /// A 64-bit digest rather than the object ID: a job touching a million paths
  /// would otherwise hold ~64 MB of hex strings, and the host serves many mounts.
  /// A collision means one blob goes uncharged, which is harmless for a limit
  /// whose purpose is to stop a runaway sweep.
  seen: HashSet<u64>,
  refusals: u64,
  refetches: u64,
  refetched_bytes: u64,
}

/// What the budget has done, for `gfs status` and telemetry.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetReport {
  pub limit_bytes: u64,
  pub charged_bytes: u64,
  pub unique_blobs: u64,
  pub refusals: u64,
  /// Blobs fetched again after the shared cache evicted them. Free against the
  /// limit, and the numerator of the thrash ratio.
  pub refetches: u64,
  pub refetched_bytes: u64,
}

fn digest(oid: &ObjectId) -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  oid.hash(&mut hasher);
  hasher.finish()
}

impl HydrationBudget {
  pub fn new(limit_bytes: u64) -> Self {
    HydrationBudget {
      limit: limit_bytes,
      state: Mutex::new(State::default()),
    }
  }

  pub fn is_unlimited(&self) -> bool {
    self.limit == 0
  }

  /// Admit `size` bytes for `oid`, charging them, or refuse.
  ///
  /// Checking and charging are one operation deliberately. Splitting them would
  /// need a reservation that a failed fetch has to return, and the failure mode of
  /// getting that wrong is a budget that leaks until it refuses everything. This
  /// way a fetch that fails after admission stays charged, which errs toward
  /// refusing early rather than serving more than the limit.
  ///
  /// The caller must know the size before fetching — `Gfs::open_blob` does,
  /// because the snapshot API reports it with the entry. Admitting after the fetch
  /// would mean the bytes are already on disk when the refusal is issued, which is
  /// the one thing a budget exists to prevent.
  pub fn admit(&self, oid: &ObjectId, size: u64) -> Result<(), GfsError> {
    if self.is_unlimited() {
      return Ok(());
    }
    let mut state = self.state.lock().expect("hydration budget");
    if !state.seen.insert(digest(oid)) {
      // Already paid for. The cache evicted it and something read it again, which
      // is the cache's business and not this job's debt.
      state.refetches += 1;
      state.refetched_bytes += size;
      return Ok(());
    }
    if state.charged.saturating_add(size) > self.limit {
      state.refusals += 1;
      // Undo the insert: the blob was not charged, so a later read of it with a
      // raised limit must not be mistaken for a re-fetch.
      state.seen.remove(&digest(oid));
      let charged = state.charged;
      let limit = self.limit;
      drop(state);
      return Err(GfsError::new(
        ErrorCode::ResourceLimit,
        // Reaches the caller as EDQUOT and nothing more -- see the module docs --
        // so the text is for the log and for `gfs status`, not for the tool.
        format!(
          "the job's hydration budget is spent: {charged} of {limit} bytes used, \
           and this read needs {size} more. Use `gfs rg` and `gfs find` instead of \
           scanning the tree, or raise --hydration-budget."
        ),
      ));
    }
    state.charged += size;
    Ok(())
  }

  pub fn report(&self) -> BudgetReport {
    let state = self.state.lock().expect("hydration budget");
    BudgetReport {
      limit_bytes: self.limit,
      charged_bytes: state.charged,
      unique_blobs: state.seen.len() as u64,
      refusals: state.refusals,
      refetches: state.refetches,
      refetched_bytes: state.refetched_bytes,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;

  fn oid(byte: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[byte; 20]).expect("oid")
  }

  #[test]
  fn zero_means_unlimited_not_forbidden() {
    let budget = HydrationBudget::new(0);
    assert!(budget.admit(&oid(1), u64::MAX).is_ok());
    assert!(budget.is_unlimited());
  }

  #[test]
  fn a_blob_is_charged_once_however_often_it_is_refetched() {
    let budget = HydrationBudget::new(1000);
    budget.admit(&oid(1), 600).expect("first");
    // Evicted and read again: free, because the job already paid to look at it.
    budget.admit(&oid(1), 600).expect("refetch");
    budget.admit(&oid(1), 600).expect("refetch");
    let report = budget.report();
    assert_eq!(report.charged_bytes, 600);
    assert_eq!(report.refetches, 2);
    assert_eq!(report.refetched_bytes, 1200);
    // And the limit still has room for a *different* blob, which is the whole
    // point: monotonic counting would have refused this.
    budget.admit(&oid(2), 400).expect("second blob fits");
  }

  #[test]
  fn refusal_is_a_resource_limit_so_it_reaches_the_kernel_as_edquot() {
    let budget = HydrationBudget::new(100);
    budget.admit(&oid(1), 100).expect("exactly the limit fits");
    let err = budget
      .admit(&oid(2), 1)
      .expect_err("one byte more must not");
    assert_eq!(err.code, ErrorCode::ResourceLimit);
    assert_eq!(crate::attr::errno_of(&err), fuser::Errno::EDQUOT);
    assert_eq!(budget.report().refusals, 1);
  }

  #[test]
  fn a_refused_blob_is_not_remembered_as_charged() {
    // Otherwise raising the limit and retrying would treat the never-fetched blob
    // as a re-fetch and serve it for free, understating what the job used.
    let budget = HydrationBudget::new(10);
    budget.admit(&oid(1), 99).expect_err("too big");
    assert_eq!(budget.report().unique_blobs, 0);
    assert_eq!(budget.report().refetches, 0);
  }
}
