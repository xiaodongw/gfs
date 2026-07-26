//! Algorithm-generic Git value types.
//!
//! DESIGN.md section 6.1 is explicit that internal IDs must carry the hash
//! algorithm alongside the digest and that no code may assume a 20-byte SHA-1.
//! These types are the M0 proof that the rule is implementable against libgit2;
//! M1.1 re-homes them in `xvfs-types`.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
  Sha1,
  Sha256,
}

impl HashAlgorithm {
  pub fn raw_len(self) -> usize {
    match self {
      HashAlgorithm::Sha1 => 20,
      HashAlgorithm::Sha256 => 32,
    }
  }

  pub fn hex_len(self) -> usize {
    self.raw_len() * 2
  }

  pub fn name(self) -> &'static str {
    match self {
      HashAlgorithm::Sha1 => "sha1",
      HashAlgorithm::Sha256 => "sha256",
    }
  }

  /// Parse the value of `extensions.objectformat` / `--object-format`.
  pub fn from_name(s: &str) -> Option<Self> {
    match s {
      "sha1" => Some(HashAlgorithm::Sha1),
      "sha256" => Some(HashAlgorithm::Sha256),
      _ => None,
    }
  }
}

/// An object ID that always knows its algorithm.
///
/// The wire and storage form is `{algorithm}:{hex}`, which is what the blob
/// endpoint in DESIGN.md section 7.3 already assumes. A bare hex string is
/// accepted only when a repository context supplies the algorithm.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ObjectId {
  algorithm: HashAlgorithm,
  raw: Vec<u8>,
}

impl ObjectId {
  pub fn from_raw(algorithm: HashAlgorithm, raw: &[u8]) -> Result<Self, OidError> {
    if raw.len() != algorithm.raw_len() {
      return Err(OidError::Length {
        expected: algorithm.raw_len(),
        actual: raw.len(),
      });
    }
    Ok(ObjectId {
      algorithm,
      raw: raw.to_vec(),
    })
  }

  /// Parse a full-length hex digest in a known-algorithm context.
  pub fn from_hex(algorithm: HashAlgorithm, hex: &str) -> Result<Self, OidError> {
    if hex.len() != algorithm.hex_len() {
      return Err(OidError::Length {
        expected: algorithm.hex_len(),
        actual: hex.len(),
      });
    }
    let mut raw = Vec::with_capacity(algorithm.raw_len());
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
      let hi = hex_val(pair[0])?;
      let lo = hex_val(pair[1])?;
      raw.push((hi << 4) | lo);
    }
    Ok(ObjectId { algorithm, raw })
  }

  /// Parse the canonical qualified form `{algorithm}:{hex}`.
  pub fn parse_qualified(s: &str) -> Result<Self, OidError> {
    let (algo, hex) = s.split_once(':').ok_or(OidError::MissingAlgorithm)?;
    let algorithm = HashAlgorithm::from_name(algo).ok_or(OidError::UnknownAlgorithm)?;
    ObjectId::from_hex(algorithm, hex)
  }

  pub fn algorithm(&self) -> HashAlgorithm {
    self.algorithm
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.raw
  }

  pub fn to_hex(&self) -> String {
    let mut s = String::with_capacity(self.algorithm.hex_len());
    for b in &self.raw {
      s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
      s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
  }

  pub fn to_qualified(&self) -> String {
    format!("{}:{}", self.algorithm.name(), self.to_hex())
  }
}

fn hex_val(b: u8) -> Result<u8, OidError> {
  match b {
    b'0'..=b'9' => Ok(b - b'0'),
    b'a'..=b'f' => Ok(b - b'a' + 10),
    // Git prints lowercase; accepting uppercase on input is harmless and
    // avoids a class of copy-paste failures from tools that upcase.
    b'A'..=b'F' => Ok(b - b'A' + 10),
    _ => Err(OidError::NotHex),
  }
}

impl fmt::Debug for ObjectId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.to_qualified())
  }
}

impl fmt::Display for ObjectId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.to_qualified())
  }
}

impl serde::Serialize for ObjectId {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&self.to_qualified())
  }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OidError {
  Length { expected: usize, actual: usize },
  NotHex,
  MissingAlgorithm,
  UnknownAlgorithm,
}

impl fmt::Display for OidError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      OidError::Length { expected, actual } => {
        write!(f, "expected {expected} units, got {actual}")
      }
      OidError::NotHex => f.write_str("not a hex digest"),
      OidError::MissingAlgorithm => f.write_str("missing `algorithm:` prefix"),
      OidError::UnknownAlgorithm => f.write_str("unknown hash algorithm"),
    }
  }
}

impl std::error::Error for OidError {}

/// A repository path as raw bytes.
///
/// Git paths are bytes, not UTF-8. Tests in the corpus include names that are
/// invalid UTF-8, so anything that round-trips a path through `String` is a bug
/// waiting for a Linux kernel filename with a Latin-1 byte in it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BytePath(Vec<u8>);

impl BytePath {
  pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
    BytePath(bytes.into())
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.0
  }

  pub fn components(&self) -> impl Iterator<Item = &[u8]> {
    self.0.split(|b| *b == b'/').filter(|c| !c.is_empty())
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }

  /// Lossless display form: valid UTF-8 verbatim, anything else escaped.
  /// The escape is unambiguous because a literal backslash is doubled.
  pub fn escaped(&self) -> String {
    let mut out = String::with_capacity(self.0.len());
    let mut rest: &[u8] = &self.0;
    while !rest.is_empty() {
      match std::str::from_utf8(rest) {
        Ok(s) => {
          push_escaped_str(&mut out, s);
          break;
        }
        Err(e) => {
          let good = &rest[..e.valid_up_to()];
          push_escaped_str(&mut out, std::str::from_utf8(good).unwrap());
          let bad_len = e.error_len().unwrap_or(rest.len() - e.valid_up_to());
          for b in &rest[e.valid_up_to()..e.valid_up_to() + bad_len] {
            out.push_str(&format!("\\x{b:02x}"));
          }
          rest = &rest[e.valid_up_to() + bad_len..];
        }
      }
    }
    out
  }
}

fn push_escaped_str(out: &mut String, s: &str) {
  for c in s.chars() {
    match c {
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c => out.push(c),
    }
  }
}

impl fmt::Debug for BytePath {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:?}", self.escaped())
  }
}

impl serde::Serialize for BytePath {
  /// Serializes as `{"escaped": ..., "b64": ...}`. The escaped form is for
  /// humans; the base64url form is the one a client must round-trip, matching
  /// the `path_b64url` parameter in DESIGN.md section 7.3.
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut m = s.serialize_map(Some(2))?;
    m.serialize_entry("escaped", &self.escaped())?;
    m.serialize_entry("b64url", &b64url(&self.0))?;
    m.end()
  }
}

pub fn b64url(bytes: &[u8]) -> String {
  const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  let mut out = String::new();
  for chunk in bytes.chunks(3) {
    let b0 = chunk[0] as u32;
    let b1 = *chunk.get(1).unwrap_or(&0) as u32;
    let b2 = *chunk.get(2).unwrap_or(&0) as u32;
    let n = (b0 << 16) | (b1 << 8) | b2;
    out.push(T[(n >> 18) as usize & 63] as char);
    out.push(T[(n >> 12) as usize & 63] as char);
    if chunk.len() > 1 {
      out.push(T[(n >> 6) as usize & 63] as char);
    }
    if chunk.len() > 2 {
      out.push(T[n as usize & 63] as char);
    }
  }
  out
}

/// The Git file modes XVFS commits to supporting (DESIGN.md section 8.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
  Regular,
  Executable,
  Symlink,
  Directory,
  /// A submodule reference. Presented as an empty read-only directory.
  Gitlink,
  /// Anything else libgit2 reports. Recorded rather than guessed at.
  Unsupported(u32),
}

impl EntryKind {
  pub fn from_mode(mode: u32) -> Self {
    match mode {
      0o100644 => EntryKind::Regular,
      0o100755 => EntryKind::Executable,
      0o120000 => EntryKind::Symlink,
      0o040000 => EntryKind::Directory,
      0o160000 => EntryKind::Gitlink,
      other => EntryKind::Unsupported(other),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn oid_round_trips_both_algorithms() {
    let sha1 = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
    assert_eq!(sha1.to_qualified(), format!("sha1:{}", "ab".repeat(20)));
    assert_eq!(
      ObjectId::parse_qualified(&sha1.to_qualified()).unwrap(),
      sha1
    );

    let sha256 = ObjectId::from_hex(HashAlgorithm::Sha256, &"cd".repeat(32)).unwrap();
    assert_eq!(sha256.algorithm(), HashAlgorithm::Sha256);
    assert_eq!(
      ObjectId::parse_qualified(&sha256.to_qualified()).unwrap(),
      sha256
    );
  }

  #[test]
  fn oid_rejects_wrong_length_for_algorithm() {
    // A SHA-256 digest offered as SHA-1 must not be silently truncated.
    let err = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(32)).unwrap_err();
    assert!(matches!(err, OidError::Length { .. }));
  }

  #[test]
  fn byte_path_escape_is_lossless_and_unambiguous() {
    // An invalid UTF-8 byte and a literal backslash must not collide.
    let a = BytePath::new(b"drivers/\xffbad.c".to_vec());
    let b = BytePath::new(b"drivers/\\xffbad.c".to_vec());
    assert_eq!(a.escaped(), "drivers/\\xffbad.c");
    assert_eq!(b.escaped(), "drivers/\\\\xffbad.c");
    assert_ne!(a.escaped(), b.escaped());
  }

  #[test]
  fn b64url_matches_known_vectors() {
    assert_eq!(b64url(b""), "");
    assert_eq!(b64url(b"f"), "Zg");
    assert_eq!(b64url(b"fo"), "Zm8");
    assert_eq!(b64url(b"foo"), "Zm9v");
    assert_eq!(b64url(b"foob"), "Zm9vYg");
    assert_eq!(b64url(&[0xfb, 0xef]), "--8");
  }
}
