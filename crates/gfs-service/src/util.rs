//! Small shared helpers.

use gfs_types::error::{ErrorCode, GfsError};

/// Cryptographically strong random bytes.
///
/// Read from `/dev/urandom` rather than pulled from a crate. GFS targets Linux
/// only (DESIGN.md section 12), `/dev/urandom` is the kernel CSPRNG, and these
/// bytes become mount IDs and capability tokens -- so the source has to be one
/// whose properties are stated rather than inherited from a dependency's default
/// feature set.
///
/// A failure here is fatal to the request rather than falling back to anything
/// weaker: a predictable capability token is a forgeable capability token.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, GfsError> {
  use std::io::Read;
  let mut f = std::fs::File::open("/dev/urandom").map_err(|e| {
    GfsError::new(
      ErrorCode::Internal,
      format!("cannot open the system random source: {e}"),
    )
  })?;
  let mut buf = vec![0u8; n];
  f.read_exact(&mut buf).map_err(|e| {
    GfsError::new(
      ErrorCode::Internal,
      format!("cannot read the system random source: {e}"),
    )
  })?;
  Ok(buf)
}

pub fn random_hex(bytes: usize) -> Result<String, GfsError> {
  Ok(
    random_bytes(bytes)?
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect(),
  )
}

/// A fresh mount identifier.
///
/// `m-` plus 16 hex characters of randomness. The character set is constrained by
/// [`gfs_types::MountId`] because the value is interpolated into
/// `refs/gfs/mounts/{id}`, and the randomness is there so a mount ID is not
/// guessable -- it appears in a capability and in an anchor ref name.
pub fn new_mount_id() -> String {
  // 8 bytes of randomness. A collision would be caught by `begin_lease`'s
  // uniqueness check and surfaced as a conflict rather than silently reusing
  // another mount's anchor, so this only has to make collisions negligible rather
  // than impossible.
  match random_hex(8) {
    Ok(hex) => format!("m-{hex}"),
    // Only reachable if /dev/urandom is unavailable, in which case the caller's
    // `MountId::parse` will fail on the empty suffix and the request errors out --
    // which is the correct outcome, and better than inventing a weak fallback.
    Err(_) => String::new(),
  }
}

/// A fresh opaque repository identifier.
pub fn new_repository_id() -> Result<String, GfsError> {
  Ok(format!("r-{}", random_hex(8)?))
}

/// Map a `JoinError` from `spawn_blocking`.
///
/// A panic payload can contain arbitrary text, including values from the request,
/// so it is deliberately not propagated into the error message.
pub fn join_error(e: tokio::task::JoinError) -> GfsError {
  if e.is_cancelled() {
    GfsError::new(ErrorCode::Cancelled, "the operation was cancelled")
  } else {
    GfsError::new(ErrorCode::Internal, "the operation failed unexpectedly")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mount_ids_are_valid_refs_and_do_not_repeat() {
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
      let id = new_mount_id();
      let parsed = gfs_types::MountId::parse(&id).expect("must be a valid mount id");
      let anchor = gfs_types::revision::lease_anchor_ref(parsed.as_str());
      assert!(gfs_types::revision::is_reserved_ref(&anchor));
      assert!(seen.insert(id), "mount ids must not repeat");
    }
  }

  #[test]
  fn random_bytes_are_not_constant() {
    // A weak guard against the source silently becoming a zero fill.
    let a = random_bytes(32).unwrap();
    let b = random_bytes(32).unwrap();
    assert_ne!(a, b);
    assert!(a.iter().any(|byte| *byte != 0));
  }
}
