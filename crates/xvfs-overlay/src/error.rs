//! Overlay errors carry a POSIX condition, because their caller is a syscall.
//!
//! `XvfsError`'s codes are a *service* vocabulary: they describe what a remote
//! API refused and why. An overlay mutation is refused for filesystem reasons —
//! the name exists, the directory is not empty, the target is a directory — and
//! collapsing those onto `InvalidArgument` would make `mkdir` on an existing
//! directory indistinguishable from `mkdir` with a malformed path. A shell tells
//! those two apart and so must this.
//!
//! The enum is deliberately closed and deliberately small. The FUSE layer maps it
//! to `Errno` in exactly one place, so a new condition cannot reach the kernel as
//! a plausible-but-wrong `EIO`.

use std::fmt;

use xvfs_types::error::XvfsError;

/// The POSIX condition an overlay refusal corresponds to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Condition {
  /// `EEXIST`
  Exists,
  /// `ENOENT`
  NoEntry,
  /// `ENOTDIR`
  NotDirectory,
  /// `EISDIR`
  IsDirectory,
  /// `ENOTEMPTY`
  NotEmpty,
  /// `EINVAL`
  Invalid,
  /// `EPERM` — refused for the life of the MVP rather than not yet implemented.
  NotPermitted,
  /// `EDQUOT` — the per-job overlay quota.
  QuotaExceeded,
  /// `EIO` — the overlay's own storage failed, or its state is damaged.
  Io,
}

impl Condition {
  pub fn as_str(self) -> &'static str {
    match self {
      Condition::Exists => "EEXIST",
      Condition::NoEntry => "ENOENT",
      Condition::NotDirectory => "ENOTDIR",
      Condition::IsDirectory => "EISDIR",
      Condition::NotEmpty => "ENOTEMPTY",
      Condition::Invalid => "EINVAL",
      Condition::NotPermitted => "EPERM",
      Condition::QuotaExceeded => "EDQUOT",
      Condition::Io => "EIO",
    }
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OverlayError {
  pub condition: Condition,
  pub message: String,
}

impl OverlayError {
  pub fn new(condition: Condition, message: impl Into<String>) -> Self {
    OverlayError {
      condition,
      message: message.into(),
    }
  }

  pub fn exists(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::Exists, message)
  }

  pub fn no_entry(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::NoEntry, message)
  }

  pub fn not_directory(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::NotDirectory, message)
  }

  pub fn is_directory(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::IsDirectory, message)
  }

  pub fn not_empty(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::NotEmpty, message)
  }

  pub fn invalid(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::Invalid, message)
  }

  pub fn quota(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::QuotaExceeded, message)
  }

  pub fn io(message: impl Into<String>) -> Self {
    OverlayError::new(Condition::Io, message)
  }
}

impl fmt::Display for OverlayError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.condition.as_str(), self.message)
  }
}

impl std::error::Error for OverlayError {}

/// A service error reaching the overlay is always a storage or protocol failure
/// by the time it gets here, so it becomes `EIO` with its message preserved.
impl From<XvfsError> for OverlayError {
  fn from(e: XvfsError) -> Self {
    OverlayError::io(format!("{}: {}", e.code.as_str(), e.message))
  }
}

pub type Result<T> = std::result::Result<T, OverlayError>;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_service_error_becomes_eio_with_its_message_intact() {
    let e: OverlayError = XvfsError::not_found("no such blob").into();
    assert_eq!(e.condition, Condition::Io);
    assert!(e.message.contains("no such blob"));
  }
}
