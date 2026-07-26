//! The timestamp policy.
//!
//! ADR 0006 fixes two formulas and the acceptance cases for both:
//!
//! ```text
//! snapshot_time    = clamp(committer_time, minimum_supported_time,
//!                          authoritative_first_seen_time - one_tick)
//! new_overlay_time = max(host_wall_clock, snapshot_time + one_tick,
//!                        prior_entry_time + one_tick)
//! ```
//!
//! Both live here even though the overlay clock is not used until M2.2, because
//! they are one policy: the first guarantees base timestamps are stable and not
//! in the future, and the second guarantees an acknowledged local edit is always
//! newer than the base. Splitting them across crates would let one be changed
//! without the other, and the property that matters is the relationship between
//! them.
//!
//! The reason any of this exists: Git accepts future-dated commits and job hosts
//! have clock skew, so a raw committer timestamp cannot be used as a filesystem
//! clock. A build system that sees a source file dated in the future rebuilds
//! forever or not at all.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The timestamp resolution the mount reports. ADR 0006: 1 ns, which the pinned
/// libgit2 build's `nsec` support allows.
pub const ONE_TICK: Duration = Duration::from_nanos(1);

/// 1990-01-01T00:00:00Z.
///
/// ADR 0006: earlier committer timestamps exist in real repositories -- imported
/// history, bad clocks, deliberate epoch-0 commits -- and break build systems
/// that treat them as clock skew rather than as age.
pub const MIN_SUPPORTED_UNIX_SECS: i64 = 631_152_000;

/// A point in time with nanosecond resolution, as stored in the catalog and
/// reported by the filesystem.
///
/// Signed seconds because Git committer times can be negative and the clamp has
/// to be able to observe that before correcting it.
#[derive(
  Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct Timestamp {
  pub secs: i64,
  pub nanos: u32,
}

impl Timestamp {
  pub const fn new(secs: i64, nanos: u32) -> Self {
    Timestamp { secs, nanos }
  }

  pub const fn from_secs(secs: i64) -> Self {
    Timestamp { secs, nanos: 0 }
  }

  pub const MIN_SUPPORTED: Timestamp = Timestamp::from_secs(MIN_SUPPORTED_UNIX_SECS);

  pub fn now() -> Self {
    Timestamp::from_system_time(SystemTime::now())
  }

  pub fn from_system_time(t: SystemTime) -> Self {
    match t.duration_since(UNIX_EPOCH) {
      Ok(d) => Timestamp {
        secs: d.as_secs() as i64,
        nanos: d.subsec_nanos(),
      },
      // Before the epoch: a host clock set to the 1960s. Represented honestly
      // rather than saturated, so the clamp below can see it and correct it.
      Err(e) => {
        let d = e.duration();
        if d.subsec_nanos() == 0 {
          Timestamp {
            secs: -(d.as_secs() as i64),
            nanos: 0,
          }
        } else {
          Timestamp {
            secs: -(d.as_secs() as i64) - 1,
            nanos: 1_000_000_000 - d.subsec_nanos(),
          }
        }
      }
    }
  }

  /// Add one tick. Saturates at the representable maximum rather than wrapping.
  pub fn next_tick(self) -> Self {
    let nanos = self.nanos as u64 + ONE_TICK.as_nanos() as u64;
    if nanos >= 1_000_000_000 {
      Timestamp {
        secs: self.secs.saturating_add((nanos / 1_000_000_000) as i64),
        nanos: (nanos % 1_000_000_000) as u32,
      }
    } else {
      Timestamp {
        secs: self.secs,
        nanos: nanos as u32,
      }
    }
  }

  fn prev_tick(self) -> Self {
    if self.nanos == 0 {
      Timestamp {
        secs: self.secs.saturating_sub(1),
        nanos: 999_999_999,
      }
    } else {
      Timestamp {
        secs: self.secs,
        nanos: self.nanos - 1,
      }
    }
  }
}

/// Compute the stable sanitized snapshot time for a commit.
///
/// Called **once**, by the catalog writer, when a commit is first cataloged. The
/// value is then durable: replicas reuse the stored value and never recompute it
/// from their own clocks, or base timestamps would differ between hosts and the
/// M2 exit criterion "base timestamps are identical across remounts and hosts"
/// would fail.
///
/// `first_seen` must come from the authoritative catalog writer's clock.
pub fn snapshot_time(committer_time: Timestamp, first_seen: Timestamp) -> Timestamp {
  let lo = Timestamp::MIN_SUPPORTED;
  let hi = first_seen.prev_tick();

  // A `clamp` with `hi < lo` is a contradiction, and it happens for real: if the
  // catalog writer's own clock is set before 1990, every commit it sees is
  // "future-dated". Returning `lo` keeps the invariant callers depend on -- the
  // result is never before the minimum supported time -- and accepts that a
  // host with a 1980s clock produces 1990 timestamps until it is fixed. The
  // alternative, honouring `hi`, would put base times below the supported floor
  // and break the build systems the floor exists to protect.
  if hi < lo {
    return lo;
  }
  if committer_time < lo {
    return lo;
  }
  if committer_time > hi {
    return hi;
  }
  committer_time
}

/// The next overlay timestamp.
///
/// Guarantees the property M2.2 must test: every acknowledged local mutation is
/// strictly newer than the base, even for a future-dated commit, a host clock
/// skewed backwards, or two writes inside one tick.
pub fn overlay_time(
  host_wall_clock: Timestamp,
  snapshot_time: Timestamp,
  prior_entry_time: Option<Timestamp>,
) -> Timestamp {
  let mut t = host_wall_clock.max(snapshot_time.next_tick());
  if let Some(prior) = prior_entry_time {
    t = t.max(prior.next_tick());
  }
  t
}

/// Clamp an explicitly requested overlay timestamp to the overlay floor.
///
/// ADR 0006 and DESIGN.md section 8.2: an explicit `utimensat` below
/// `snapshot_time + one_tick` is clamped rather than honoured, and exact
/// restoration of an older mtime is a documented MVP incompatibility rather than
/// a bug.
pub fn clamp_requested_overlay_time(requested: Timestamp, snapshot_time: Timestamp) -> Timestamp {
  requested.max(snapshot_time.next_tick())
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIRST_SEEN: Timestamp = Timestamp::new(1_700_000_000, 500);

  #[test]
  fn an_ordinary_commit_time_passes_through_unchanged() {
    let t = Timestamp::from_secs(1_600_000_000);
    assert_eq!(snapshot_time(t, FIRST_SEEN), t);
  }

  #[test]
  fn a_future_dated_commit_is_clamped_below_first_seen() {
    // ADR 0006 acceptance case 1. The commit claims to be from 2050.
    let future = Timestamp::from_secs(2_500_000_000);
    let s = snapshot_time(future, FIRST_SEEN);
    assert!(s < FIRST_SEEN, "{s:?} must be before {FIRST_SEEN:?}");
    assert_eq!(s, FIRST_SEEN.prev_tick());
  }

  #[test]
  fn a_prehistoric_commit_is_raised_to_the_supported_floor() {
    for ancient in [
      Timestamp::from_secs(0),
      Timestamp::from_secs(-1),
      Timestamp::from_secs(-2_000_000_000),
    ] {
      assert_eq!(snapshot_time(ancient, FIRST_SEEN), Timestamp::MIN_SUPPORTED);
    }
  }

  #[test]
  fn a_catalog_clock_set_before_1990_still_yields_a_supported_time() {
    // The contradictory case: `hi < lo`. Without explicit handling this is
    // where a clamp either panics or silently returns a pre-1990 time.
    let broken_first_seen = Timestamp::from_secs(300_000_000); // 1979
    let s = snapshot_time(Timestamp::from_secs(1_600_000_000), broken_first_seen);
    assert_eq!(s, Timestamp::MIN_SUPPORTED);
    assert!(s >= Timestamp::MIN_SUPPORTED);
  }

  #[test]
  fn an_edit_is_newer_than_the_base_even_for_a_future_dated_commit() {
    // ADR 0006 acceptance case 1, from the overlay side. `snapshot_time` has
    // already been clamped, but the host clock may still be behind it.
    let base = snapshot_time(Timestamp::from_secs(2_500_000_000), FIRST_SEEN);
    let host_behind = Timestamp::from_secs(1_500_000_000);
    let edit = overlay_time(host_behind, base, None);
    assert!(edit > base, "{edit:?} must be after base {base:?}");
  }

  #[test]
  fn an_edit_is_newer_than_the_base_when_the_host_clock_runs_backwards() {
    // ADR 0006 acceptance case 2.
    let base = Timestamp::from_secs(1_600_000_000);
    let skewed = Timestamp::from_secs(1_000_000_000);
    assert!(overlay_time(skewed, base, None) > base);
  }

  #[test]
  fn two_writes_within_one_tick_still_advance() {
    // ADR 0006 acceptance case 3. The host clock does not move between the two
    // writes, so only the prior-entry term can separate them.
    let base = Timestamp::from_secs(1_600_000_000);
    let host = Timestamp::new(1_700_000_000, 42);
    let first = overlay_time(host, base, None);
    let second = overlay_time(host, base, Some(first));
    let third = overlay_time(host, base, Some(second));
    assert!(first < second, "{first:?} < {second:?}");
    assert!(second < third, "{second:?} < {third:?}");
  }

  #[test]
  fn an_explicit_timestamp_below_the_floor_is_clamped_not_honoured() {
    // ADR 0006 acceptance case 4: a build tool calling `utimensat` with an old
    // mtime. Documented MVP incompatibility, so the assertion is that it is
    // clamped rather than that it round-trips.
    let base = Timestamp::from_secs(1_600_000_000);
    let requested = Timestamp::from_secs(1_000_000_000);
    let got = clamp_requested_overlay_time(requested, base);
    assert!(got > base);
    assert_ne!(got, requested);
    // A request above the floor is honoured exactly.
    let ok = Timestamp::from_secs(1_900_000_000);
    assert_eq!(clamp_requested_overlay_time(ok, base), ok);
  }

  #[test]
  fn snapshot_time_is_a_pure_function_of_its_inputs() {
    // ADR 0006 acceptance case 5, the part that is checkable without two hosts:
    // the value depends only on the committer time and the stored first-seen
    // time, so two replicas reusing the stored value cannot disagree.
    let c = Timestamp::from_secs(1_600_000_000);
    assert_eq!(snapshot_time(c, FIRST_SEEN), snapshot_time(c, FIRST_SEEN));
  }

  #[test]
  fn next_tick_carries_across_a_second_boundary() {
    let t = Timestamp::new(10, 999_999_999);
    assert_eq!(t.next_tick(), Timestamp::new(11, 0));
    assert_eq!(Timestamp::new(11, 0).prev_tick(), t);
  }

  #[test]
  fn pre_epoch_system_times_round_trip() {
    let t = UNIX_EPOCH - Duration::new(5, 500);
    let ts = Timestamp::from_system_time(t);
    assert_eq!(ts, Timestamp::new(-6, 999_999_500));
    assert!(ts < Timestamp::from_secs(0));
  }
}
