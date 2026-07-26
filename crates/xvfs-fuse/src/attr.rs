//! Attribute mapping: a Git tree entry as a POSIX inode.
//!
//! # Timestamps
//!
//! Every base entry reports the same value for `atime`, `mtime`, and `ctime`: the
//! stored sanitized `snapshot_time` from ADR 0006, never the raw committer time.
//! Git accepts future-dated commits and job hosts have clock skew, and a source
//! file dated in the future makes a build system either rebuild forever or never.
//! The value is computed once by the catalog writer and reused by every replica,
//! which is what makes M2's exit criterion — base timestamps identical across
//! remounts and hosts — hold at all.
//!
//! # Permissions
//!
//! The base is read-only in M2 and the overlay arrives in M3, so nothing here is
//! writable. Executability is the one bit Git records, and it is preserved
//! exactly: a checked-out build script that lost its `+x` fails in a way that
//! looks like a missing file.
//!
//! # Ownership
//!
//! Files report the daemon's own UID and GID. ADR 0005 verified that the same-UID
//! case needs no `safe.directory`; the cross-UID case is where the bind mount
//! requires it, and that is the deployment concern M6.1 owns.
//!
//! # Unsupported modes
//!
//! DESIGN.md section 8.2 requires unsupported Git modes to be represented
//! *predictably*. They appear as unreadable regular files rather than being
//! coerced to the nearest supported kind, because presenting a file as something
//! it is not is worse than reporting it as unusable.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{EntryKind, Timestamp};

use crate::gitdir::SynthNode;
use crate::inode::{Node, Record};

/// Who the mount reports as owner, fixed at daemon start.
#[derive(Clone, Copy, Debug)]
pub struct Ownership {
  pub uid: u32,
  pub gid: u32,
}

impl Ownership {
  /// The daemon's own effective UID and GID.
  ///
  /// The workspace denies `unsafe_code` and this is the documented opt-out the
  /// deny (rather than a `forbid`) exists for. `geteuid` and `getegid` are the
  /// two POSIX calls specified never to fail: they take no arguments, touch no
  /// memory, and have no error return. There is no safe wrapper in `std`, and
  /// reconstructing the answer from `/proc/self` would read the *real* UID, not
  /// the effective one, which is the wrong value for a daemon that was started
  /// set-uid or under a different login.
  #[allow(unsafe_code)]
  pub fn current() -> Self {
    Ownership {
      uid: unsafe { libc::geteuid() },
      gid: unsafe { libc::getegid() },
    }
  }
}

/// Convert a sanitized snapshot timestamp to a `SystemTime`.
///
/// Saturating rather than panicking on an out-of-range value: `snapshot_time` is
/// already clamped to `[1990-01-01, first_seen)`, so a value that cannot be
/// represented means the catalog was corrupted, and a mount that reports an
/// implausible date is more useful than a daemon that aborts.
pub fn to_system_time(t: Timestamp) -> SystemTime {
  if t.secs >= 0 {
    UNIX_EPOCH
      .checked_add(Duration::new(t.secs as u64, t.nanos))
      .unwrap_or(UNIX_EPOCH)
  } else {
    UNIX_EPOCH
      .checked_sub(Duration::new(t.secs.unsigned_abs(), 0))
      .map(|t0| t0 + Duration::from_nanos(u64::from(t.nanos)))
      .unwrap_or(UNIX_EPOCH)
  }
}

/// The attributes of one inode.
pub fn attr_of(record: &Record, snapshot_time: Timestamp, owner: Ownership) -> FileAttr {
  let (kind, perm, size, nlink) = match &record.node {
    Node::Base(entry) => match entry.kind {
      EntryKind::Regular => (FileType::RegularFile, 0o444, entry.size, 1),
      EntryKind::Executable => (FileType::RegularFile, 0o555, entry.size, 1),
      EntryKind::Symlink => (
        FileType::Symlink,
        0o777,
        entry
          .symlink_target
          .as_ref()
          .map_or(entry.size, |t| t.len() as u64),
        1,
      ),
      EntryKind::Directory => (FileType::Directory, 0o555, 0, 2),
      // A submodule: an empty, read-only directory with inspectable XVFS
      // metadata (DESIGN.md section 8.2). ADR 0006 measured 12 of these in the
      // Rust repository, so this is a live case for the pilot.
      EntryKind::Gitlink => (FileType::Directory, 0o555, 0, 2),
      // Mode 0 means nothing can open it even where the kernel does the check,
      // and `open` refuses it explicitly where the kernel does not.
      EntryKind::Unsupported(_) => (FileType::RegularFile, 0o000, entry.size, 1),
    },
    Node::Synth(node) => match node {
      SynthNode::Dir => (FileType::Directory, 0o555, 0, 2),
      SynthNode::File(bytes) => (FileType::RegularFile, 0o444, bytes.len() as u64, 1),
    },
  };

  let time = to_system_time(snapshot_time);
  FileAttr {
    ino: INodeNo(record.ino),
    size,
    // 512-byte blocks, rounded up, as `stat(2)` reports them. `du` on the mount
    // reports the logical size rather than zero, which is what a build's disk
    // accounting expects.
    blocks: size.div_ceil(512),
    atime: time,
    mtime: time,
    ctime: time,
    crtime: time,
    kind,
    perm,
    nlink,
    uid: owner.uid,
    gid: owner.gid,
    rdev: 0,
    blksize: 4096,
    flags: 0,
  }
}

/// Map a service error onto the errno a POSIX caller can act on.
///
/// The mapping is not cosmetic. `EIO` tells a build to fail loudly; `ENOENT`
/// tells it a file is absent, which is a normal condition it will handle by
/// looking elsewhere. Reporting a transport failure as `ENOENT` would let a
/// compiler silently proceed as if a header did not exist, which is the class of
/// wrong answer this whole design is arranged to prevent.
pub fn errno_of(error: &XvfsError) -> fuser::Errno {
  use fuser::Errno;
  match error.code {
    ErrorCode::NotFound => Errno::ENOENT,
    ErrorCode::InvalidArgument | ErrorCode::ReservedNamespace => Errno::EINVAL,
    ErrorCode::PermissionDenied => Errno::EACCES,
    ErrorCode::Unauthenticated | ErrorCode::Expired => Errno::EACCES,
    ErrorCode::FailedPrecondition | ErrorCode::UnsupportedRepositoryFormat => Errno::EIO,
    ErrorCode::Conflict => Errno::EIO,
    ErrorCode::ResourceLimit => Errno::EDQUOT,
    // Retryable at the service layer, but the FUSE client has already exhausted
    // its retries by the time this is mapped, so the caller sees a hard failure.
    ErrorCode::Unavailable | ErrorCode::SnapshotBuilding | ErrorCode::DeadlineExceeded => {
      Errno::EIO
    }
    ErrorCode::NotIndexable => Errno::EIO,
    ErrorCode::Cancelled => Errno::EINTR,
    ErrorCode::Internal => Errno::EIO,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use xvfs_types::{BytePath, HashAlgorithm, ObjectId, TreeEntryInfo};

  fn record(kind: EntryKind, size: u64) -> Record {
    let entry = TreeEntryInfo {
      path: BytePath::new(b"f".to_vec()),
      kind,
      mode: 0,
      oid: ObjectId::from_raw(HashAlgorithm::Sha1, &[1; 20]).unwrap(),
      size,
      symlink_target: None,
      blob_ticket: None,
    };
    // Constructed through the table so the private fields stay private.
    let mut table = crate::inode::InodeTable::new(TreeEntryInfo {
      path: BytePath::root(),
      kind: EntryKind::Directory,
      ..entry.clone()
    });
    table.insert_lookup(BytePath::new(b"f".to_vec()), Node::Base(entry))
  }

  const OWNER: Ownership = Ownership {
    uid: 1000,
    gid: 100,
  };

  #[test]
  fn the_executable_bit_survives() {
    // A build script that lost `+x` fails in a way that looks like a missing file.
    assert_eq!(
      attr_of(
        &record(EntryKind::Regular, 10),
        Timestamp::from_secs(1),
        OWNER
      )
      .perm,
      0o444
    );
    assert_eq!(
      attr_of(
        &record(EntryKind::Executable, 10),
        Timestamp::from_secs(1),
        OWNER
      )
      .perm,
      0o555
    );
  }

  #[test]
  fn a_gitlink_is_an_empty_read_only_directory() {
    let attr = attr_of(
      &record(EntryKind::Gitlink, 0),
      Timestamp::from_secs(1),
      OWNER,
    );
    assert_eq!(attr.kind, FileType::Directory);
    assert_eq!(attr.perm, 0o555);
    assert_eq!(attr.size, 0);
  }

  #[test]
  fn an_unsupported_mode_is_unopenable_rather_than_coerced() {
    let attr = attr_of(
      &record(EntryKind::Unsupported(0o120_755), 3),
      Timestamp::from_secs(1),
      OWNER,
    );
    assert_eq!(attr.kind, FileType::RegularFile);
    assert_eq!(attr.perm, 0o000);
  }

  #[test]
  fn all_three_timestamps_report_the_snapshot_time() {
    let t = Timestamp::new(1_600_000_000, 123);
    let attr = attr_of(&record(EntryKind::Regular, 1), t, OWNER);
    assert_eq!(attr.mtime, attr.ctime);
    assert_eq!(attr.mtime, attr.atime);
    assert_eq!(attr.mtime, to_system_time(t));
  }

  #[test]
  fn block_counts_round_up_so_du_does_not_report_zero() {
    assert_eq!(
      attr_of(
        &record(EntryKind::Regular, 1),
        Timestamp::from_secs(1),
        OWNER
      )
      .blocks,
      1
    );
    assert_eq!(
      attr_of(
        &record(EntryKind::Regular, 512),
        Timestamp::from_secs(1),
        OWNER
      )
      .blocks,
      1
    );
    assert_eq!(
      attr_of(
        &record(EntryKind::Regular, 513),
        Timestamp::from_secs(1),
        OWNER
      )
      .blocks,
      2
    );
  }

  #[test]
  fn a_transport_failure_never_looks_like_a_missing_file() {
    // The mapping that matters most: a compiler told ENOENT for a header that
    // exists will silently take a different code path.
    for code in [
      ErrorCode::Unavailable,
      ErrorCode::DeadlineExceeded,
      ErrorCode::Internal,
      ErrorCode::SnapshotBuilding,
    ] {
      let errno = errno_of(&XvfsError::new(code, "x"));
      assert_eq!(errno, fuser::Errno::EIO, "{code:?}");
    }
    assert_eq!(errno_of(&XvfsError::not_found("x")), fuser::Errno::ENOENT);
  }

  #[test]
  fn a_pre_1990_timestamp_still_converts() {
    // `snapshot_time` clamps to 1990, but the conversion must not panic if a
    // corrupted catalog row reaches it.
    assert!(to_system_time(Timestamp::from_secs(-100)) < UNIX_EPOCH);
    assert_eq!(to_system_time(Timestamp::from_secs(0)), UNIX_EPOCH);
  }
}
