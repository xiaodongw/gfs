//! Git LFS pointer files (spec v1).
//!
//! A pointer is a small text blob:
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
//! size 12345
//! ```
//!
//! The `oid` is the SHA-256 of the *raw* expanded content, which is what makes
//! ADR 0012's `lfs-sha256:{hex}` content key verifiable end to end. Parsing is
//! strict enough that ordinary small text files cannot be mistaken for
//! pointers: the version line must come first, every line must be `key value`,
//! and `oid`/`size` must be well-formed. It is lenient exactly where the spec
//! is: unknown extra keys are allowed and the legacy pre-rename version URL is
//! accepted.

use crate::oid::{HashAlgorithm, ObjectId};

/// Pointers are tiny; git-lfs itself refuses to scan blobs above this size, so
/// anything larger is definitionally not a pointer and callers can skip the
/// blob read entirely on the strength of the size from the object header.
pub const MAX_POINTER_SIZE: u64 = 1024;

const VERSION_URLS: [&str; 2] = [
  "https://git-lfs.github.com/spec/v1",
  // The pre-rename URL, still accepted by git-lfs for reading.
  "https://hawser.github.com/spec/v1",
];

/// A parsed spec v1 pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsPointer {
  /// The expanded content's identity, spelled `lfs-sha256:{hex}`.
  pub oid: ObjectId,
  /// The expanded content's size in bytes.
  pub size: u64,
}

impl LfsPointer {
  /// Render the canonical pointer text this pointer round-trips to.
  ///
  /// This is byte-for-byte what git-lfs's clean filter writes: version, oid,
  /// size, LF line endings, trailing newline. A pointer that carried
  /// nonstandard extra keys does not survive the round trip — accepted for the
  /// MVP and recorded in the plan.
  pub fn to_pointer_text(&self) -> String {
    format!(
      "version {}\noid sha256:{}\nsize {}\n",
      VERSION_URLS[0],
      self.oid.to_hex(),
      self.size
    )
  }
}

/// Parse a blob as a spec v1 pointer, returning `None` for anything that is
/// not one. There is no error case: "not a pointer" is an ordinary answer for
/// a small text file on an LFS-filtered path.
pub fn parse_pointer(content: &[u8]) -> Option<LfsPointer> {
  if content.is_empty() || content.len() as u64 > MAX_POINTER_SIZE {
    return None;
  }
  let text = std::str::from_utf8(content).ok()?;
  let mut lines = text.split_inclusive('\n');

  let version = lines.next()?.strip_suffix('\n')?;
  let url = version.strip_prefix("version ")?;
  if !VERSION_URLS.contains(&url) {
    return None;
  }

  let mut oid = None;
  let mut size = None;
  for line in lines {
    // Every line, including the last, must be LF-terminated `key value`.
    let line = line.strip_suffix('\n')?;
    let (key, value) = line.split_once(' ')?;
    match key {
      "oid" => {
        let hex = value.strip_prefix("sha256:")?;
        oid = Some(ObjectId::from_hex(HashAlgorithm::LfsSha256, hex).ok()?);
      }
      "size" => size = Some(value.parse::<u64>().ok()?),
      // Unknown keys are legal; git-lfs writes none but tolerates them.
      _ => {}
    }
  }
  Some(LfsPointer {
    oid: oid?,
    size: size?,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  const HEX: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

  fn pointer_text() -> String {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HEX}\nsize 12345\n")
  }

  #[test]
  fn a_spec_pointer_parses_and_round_trips() {
    let p = parse_pointer(pointer_text().as_bytes()).unwrap();
    assert_eq!(p.size, 12345);
    assert_eq!(p.oid.to_qualified(), format!("lfs-sha256:{HEX}"));
    assert_eq!(p.to_pointer_text(), pointer_text());
  }

  #[test]
  fn near_misses_are_not_pointers() {
    // Ordinary text starting with other content.
    assert_eq!(parse_pointer(b"hello\nworld\n"), None);
    // Version line not first.
    let swapped = format!("oid sha256:{HEX}\nversion https://git-lfs.github.com/spec/v1\nsize 1\n");
    assert_eq!(parse_pointer(swapped.as_bytes()), None);
    // Missing size.
    let no_size = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HEX}\n");
    assert_eq!(parse_pointer(no_size.as_bytes()), None);
    // Truncated oid.
    let short = format!("version https://git-lfs.github.com/spec/v1\noid sha256:abcd\nsize 1\n");
    assert_eq!(parse_pointer(short.as_bytes()), None);
    // Missing trailing newline on the last line.
    let untermd = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HEX}\nsize 1");
    assert_eq!(parse_pointer(untermd.as_bytes()), None);
    // Oversized content is not even scanned.
    let mut big = pointer_text().into_bytes();
    big.resize(2048, b'x');
    assert_eq!(parse_pointer(&big), None);
  }

  #[test]
  fn extra_keys_are_tolerated_reading_but_dropped_on_round_trip() {
    let with_extra = format!(
      "version https://git-lfs.github.com/spec/v1\nx-custom yes\noid sha256:{HEX}\nsize 7\n"
    );
    let p = parse_pointer(with_extra.as_bytes()).unwrap();
    assert_eq!(p.size, 7);
    assert_ne!(p.to_pointer_text(), with_extra);
  }
}
