//! The retention-lease heartbeat.
//!
//! M1 built the server side: a lease is created atomically under the repository
//! lock, anchors the commit as a reachability root, and expires if nobody renews
//! it. This is the client half — the thing that must not stop running, because
//! when it does, a routine upstream force push eventually prunes the objects out
//! from under a live mount and every uncached read fails permanently mid-task.
//!
//! # The numbers are ADR 0006's, not invented here
//!
//! | Parameter | Value |
//! | --- | --- |
//! | Initial TTL | 30 minutes |
//! | Heartbeat interval | 5 minutes (server-supplied) |
//! | Renewal grace after expiry | 15 minutes |
//! | Alert threshold | 2 consecutive failures |
//!
//! Six renewals per TTL means five consecutive failures are survivable before
//! grace begins, and the alert at two fires roughly ten minutes before that. The
//! interval comes from the server's `CreateMount` response rather than from a
//! constant compiled into the client, so the cadence can change without a client
//! release.
//!
//! # Failure is surfaced, never silent
//!
//! ADR 0006's failure policy: "lease renewal failing — warn at 2 failures, `EIO`
//! on uncached reads after grace, never silent." [`LeaseHealth`] is what `gfs
//! health` reports and what makes the warning reach an operator before the
//! workspace is destroyed rather than after.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gfs_types::{LeasePolicy, MountId, Timestamp};

/// What an operator needs to decide whether to intervene.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HealthState {
  /// Renewing normally.
  Healthy,
  /// Renewal is failing but the lease is still valid. ADR 0006's alert point.
  Warning,
  /// The lease has expired and its grace period has passed. Uncached reads will
  /// fail, and the commit is now prunable.
  Critical,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LeaseHealth {
  pub state: HealthState,
  pub consecutive_failures: u32,
  pub lease_expiry: Timestamp,
  pub last_renewal: Option<Timestamp>,
  /// The most recent failure's message, redacted of anything but the error code
  /// and text the server chose to return.
  pub last_error: Option<String>,
  pub heartbeat_interval_secs: u64,
  pub grace_deadline: Timestamp,
}

impl LeaseHealth {
  pub fn is_healthy(&self) -> bool {
    self.state == HealthState::Healthy
  }
}

/// Shared, observable heartbeat state for one lease.
#[derive(Debug)]
pub struct LeaseMonitor {
  mount_id: MountId,
  policy: LeasePolicy,
  inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
  expiry: Timestamp,
  interval: Duration,
  failures: u32,
  last_renewal: Option<Timestamp>,
  last_error: Option<String>,
}

impl LeaseMonitor {
  pub fn new(
    mount_id: MountId,
    expiry: Timestamp,
    interval: Duration,
    policy: LeasePolicy,
  ) -> Arc<Self> {
    Arc::new(LeaseMonitor {
      mount_id,
      policy,
      inner: Mutex::new(Inner {
        expiry,
        interval,
        failures: 0,
        last_renewal: None,
        last_error: None,
      }),
    })
  }

  pub fn mount_id(&self) -> &MountId {
    &self.mount_id
  }

  pub fn interval(&self) -> Duration {
    self.inner.lock().expect("lease monitor").interval
  }

  pub fn record_success(&self, expiry: Timestamp) {
    let mut inner = self.inner.lock().expect("lease monitor");
    inner.expiry = expiry;
    inner.failures = 0;
    inner.last_error = None;
    inner.last_renewal = Some(Timestamp::now());
  }

  pub fn record_failure(&self, message: String) -> u32 {
    let mut inner = self.inner.lock().expect("lease monitor");
    inner.failures += 1;
    inner.last_error = Some(message);
    inner.failures
  }

  pub fn expiry(&self) -> Timestamp {
    self.inner.lock().expect("lease monitor").expiry
  }

  pub fn health(&self) -> LeaseHealth {
    self.health_at(Timestamp::now())
  }

  /// The health computation, with the clock passed in so it can be tested
  /// without waiting thirty minutes.
  pub fn health_at(&self, now: Timestamp) -> LeaseHealth {
    let inner = self.inner.lock().expect("lease monitor");
    let grace_deadline = Timestamp::new(
      inner
        .expiry
        .secs
        .saturating_add(self.policy.renewal_grace.as_secs() as i64),
      inner.expiry.nanos,
    );
    let state = if now > grace_deadline {
      // Past grace: the server may already have released the anchor, so the
      // pinned commit is prunable and uncached reads are on borrowed time.
      HealthState::Critical
    } else if inner.failures >= self.policy.alert_after_failures || now > inner.expiry {
      HealthState::Warning
    } else {
      HealthState::Healthy
    };
    LeaseHealth {
      state,
      consecutive_failures: inner.failures,
      lease_expiry: inner.expiry,
      last_renewal: inner.last_renewal,
      last_error: inner.last_error.clone(),
      heartbeat_interval_secs: inner.interval.as_secs(),
      grace_deadline,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn monitor(expiry_secs: i64) -> Arc<LeaseMonitor> {
    LeaseMonitor::new(
      MountId::parse("m-lease").unwrap(),
      Timestamp::from_secs(expiry_secs),
      Duration::from_secs(300),
      LeasePolicy::adr_0006(),
    )
  }

  #[test]
  fn a_fresh_lease_is_healthy() {
    let m = monitor(2_000);
    assert_eq!(
      m.health_at(Timestamp::from_secs(1_000)).state,
      HealthState::Healthy
    );
  }

  #[test]
  fn two_consecutive_failures_warn_while_the_lease_is_still_valid() {
    // ADR 0006's alert threshold, chosen to fire roughly ten minutes before
    // grace begins so a human can act before a workspace is destroyed.
    let m = monitor(2_000);
    assert_eq!(m.record_failure("transient".to_owned()), 1);
    assert_eq!(
      m.health_at(Timestamp::from_secs(1_000)).state,
      HealthState::Healthy
    );
    assert_eq!(m.record_failure("transient".to_owned()), 2);
    let health = m.health_at(Timestamp::from_secs(1_000));
    assert_eq!(health.state, HealthState::Warning);
    assert_eq!(health.last_error.as_deref(), Some("transient"));
  }

  #[test]
  fn a_success_clears_the_failure_count() {
    let m = monitor(2_000);
    m.record_failure("a".to_owned());
    m.record_failure("b".to_owned());
    m.record_success(Timestamp::from_secs(4_000));
    let health = m.health_at(Timestamp::from_secs(1_000));
    assert_eq!(health.state, HealthState::Healthy);
    assert_eq!(health.consecutive_failures, 0);
    assert!(health.last_error.is_none());
    assert_eq!(health.lease_expiry, Timestamp::from_secs(4_000));
  }

  #[test]
  fn expiry_warns_and_only_the_end_of_grace_is_critical() {
    // The distinction that matters operationally: an expired lease is still
    // recoverable during grace, and only after it is the mount actually doomed.
    let m = monitor(2_000);
    let grace = LeasePolicy::adr_0006().renewal_grace.as_secs() as i64;

    assert_eq!(
      m.health_at(Timestamp::from_secs(2_500)).state,
      HealthState::Warning
    );
    assert_eq!(
      m.health_at(Timestamp::from_secs(2_000 + grace + 1)).state,
      HealthState::Critical
    );
  }

  #[test]
  fn the_heartbeat_interval_comes_from_the_server() {
    // Not a compiled-in constant: the cadence must be changeable without a
    // client release.
    let m = LeaseMonitor::new(
      MountId::parse("m-x").unwrap(),
      Timestamp::from_secs(1),
      Duration::from_secs(42),
      LeasePolicy::adr_0006(),
    );
    assert_eq!(m.interval(), Duration::from_secs(42));
    assert_eq!(m.health().heartbeat_interval_secs, 42);
  }
}
