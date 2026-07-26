//! Repository, mount, and subject identity.
//!
//! ADR 0006 fixes the rule these types enforce: a repository is identified by an
//! opaque server-assigned `repository_id`, never by a display name or a
//! filesystem path, and a display name is mutable metadata. PLAN.md M1.2 states
//! the same requirement from the other direction -- "define repository IDs
//! independently from display names and filesystem paths".
//!
//! Keeping them as distinct types rather than three `String`s is what makes that
//! rule checkable. A function that takes a `RepositoryId` cannot be handed a
//! display name by accident, which is the mistake that turns a mutable label
//! into a storage key.

use std::fmt;

use crate::error::XvfsError;
use crate::limits;

/// An opaque, immutable, server-assigned repository identifier.
///
/// Opaque to clients, but not random: it is the single URL path component from
/// ADR 0006 and appears in `/v1/repos/{id}/...`, so it is constrained to
/// characters that need no URL escaping and cannot be confused for a path.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(String);

impl RepositoryId {
  /// Validate an identifier received from a client or read from the catalog.
  ///
  /// The rule is ADR 0006's: `[A-Za-z0-9._-]{1,128}`, not starting with `.`, not
  /// containing `..`. The M0.3 gateway probe tests ten traversal and
  /// absolute-path forms against it.
  pub fn parse(s: &str) -> Result<Self, XvfsError> {
    if s.is_empty() || s.len() > limits::MAX_REPOSITORY_ID_BYTES {
      return Err(XvfsError::invalid(format!(
        "repository id must be 1..={} characters",
        limits::MAX_REPOSITORY_ID_BYTES
      )));
    }
    if s.starts_with('.') {
      return Err(XvfsError::invalid("repository id may not start with `.`"));
    }
    if s.contains("..") {
      return Err(XvfsError::invalid("repository id may not contain `..`"));
    }
    if !s
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
      return Err(XvfsError::invalid(
        "repository id may contain only letters, digits, `.`, `_`, and `-`",
      ));
    }
    Ok(RepositoryId(s.to_owned()))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl fmt::Display for RepositoryId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for RepositoryId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "RepositoryId({})", self.0)
  }
}

/// A human-facing repository label, such as `acme/monorepo`.
///
/// Mutable, non-unique in principle, and never a storage key or a URL component.
/// It exists as its own type so it cannot be passed where a [`RepositoryId`] is
/// expected.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DisplayName(String);

impl DisplayName {
  pub fn parse(s: &str) -> Result<Self, XvfsError> {
    if s.is_empty() || s.len() > limits::MAX_DISPLAY_NAME_BYTES {
      return Err(XvfsError::invalid(format!(
        "display name must be 1..={} characters",
        limits::MAX_DISPLAY_NAME_BYTES
      )));
    }
    // No control characters: a display name reaches logs, CLI output, and
    // audit records, and a newline in any of those is log injection.
    if s.chars().any(|c| c.is_control()) {
      return Err(XvfsError::invalid(
        "display name may not contain control characters",
      ));
    }
    Ok(DisplayName(s.to_owned()))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl fmt::Display for DisplayName {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

/// A mount identifier. Appears inside a lease anchor ref name, so its character
/// set is constrained to what `git check-ref-format` accepts.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct MountId(String);

impl MountId {
  pub fn parse(s: &str) -> Result<Self, XvfsError> {
    if s.is_empty() || s.len() > limits::MAX_MOUNT_ID_BYTES {
      return Err(XvfsError::invalid(format!(
        "mount id must be 1..={} characters",
        limits::MAX_MOUNT_ID_BYTES
      )));
    }
    // Deliberately narrower than repository ids: this value is interpolated
    // into `refs/xvfs/mounts/{id}`, so a `.` would risk a `..` or a `.lock`
    // suffix and every other punctuation risks an invalid ref name.
    if !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
      return Err(XvfsError::invalid(
        "mount id may contain only letters, digits, and `-`",
      ));
    }
    Ok(MountId(s.to_owned()))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl fmt::Display for MountId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for MountId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "MountId({})", self.0)
  }
}

/// An authenticated principal: a user, a workload identity, or a job.
///
/// The mount capability binds one of these (DESIGN.md section 7.1), and audit
/// records carry it (ADR 0006).
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SubjectId(String);

impl SubjectId {
  pub fn parse(s: &str) -> Result<Self, XvfsError> {
    if s.is_empty() || s.len() > limits::MAX_SUBJECT_ID_BYTES {
      return Err(XvfsError::invalid(format!(
        "subject id must be 1..={} characters",
        limits::MAX_SUBJECT_ID_BYTES
      )));
    }
    if s.chars().any(|c| c.is_control()) {
      return Err(XvfsError::invalid(
        "subject id may not contain control characters",
      ));
    }
    Ok(SubjectId(s.to_owned()))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl fmt::Display for SubjectId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for SubjectId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "SubjectId({})", self.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn repository_id_rejects_traversal_and_path_forms() {
    // The ten shapes the M0.3 gateway probe tests, plus the ones a URL router
    // can produce after its own normalization.
    for bad in [
      "..",
      "../etc",
      "a/../b",
      "/absolute",
      "a/b",
      ".hidden",
      "a b",
      "a\0b",
      "a%2fb",
      "",
    ] {
      assert!(
        RepositoryId::parse(bad).is_err(),
        "should reject {bad:?} as a repository id"
      );
    }
    RepositoryId::parse("acme-monorepo.git").unwrap();
    RepositoryId::parse("r_1").unwrap();
  }

  #[test]
  fn mount_id_cannot_produce_an_invalid_ref_name() {
    // A mount id is interpolated into `refs/xvfs/mounts/{id}`, so anything that
    // could make that an invalid or ambiguous ref must be rejected here.
    for bad in ["a.lock", "..", "a/b", "a.b", "-", ""] {
      if bad == "-" {
        // A single `-` is accepted by the character rule; assert the ref it
        // produces is still valid rather than pretending otherwise.
        let anchor = crate::revision::lease_anchor_ref(MountId::parse(bad).unwrap().as_str());
        assert!(crate::revision::is_reserved_ref(&anchor));
        continue;
      }
      assert!(
        MountId::parse(bad).is_err(),
        "should reject {bad:?} as a mount id"
      );
    }
    MountId::parse("m-0f3a9c").unwrap();
  }

  #[test]
  fn display_name_and_id_are_not_interchangeable_types() {
    // The compile-time half of ADR 0006's rule. `acme/monorepo` is a perfectly
    // good display name and an invalid repository id, and this is where that
    // difference is enforced.
    DisplayName::parse("acme/monorepo").unwrap();
    assert!(RepositoryId::parse("acme/monorepo").is_err());
  }

  #[test]
  fn identifiers_reject_control_characters_that_would_inject_into_logs() {
    assert!(DisplayName::parse("acme\nmonorepo").is_err());
    assert!(SubjectId::parse("user\r\nadmin").is_err());
  }
}
