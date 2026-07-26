//! Algorithm-generic Git object IDs.
//!
//! DESIGN.md section 6.1 is explicit that internal IDs must carry the hash
//! algorithm alongside the digest and that no code may assume a 20-byte SHA-1,
//! because Git pack formats also support SHA-256 repositories. ADR 0001 records
//! that XVFS cannot currently *host* a SHA-256 repository -- `git2-rs` does not
//! compile against a SHA-256 libgit2 -- but the types stay algorithm-generic
//! anyway. The reason is asymmetric cost: carrying the algorithm now is nearly
//! free, and retrofitting it after every signature says `[u8; 20]` is not.

use std::fmt;

#[derive(
  Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
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

impl fmt::Display for HashAlgorithm {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.name())
  }
}

/// An object ID that always knows its algorithm.
///
/// The wire and storage form is `{algorithm}:{hex}`, which is what the blob
/// endpoint in DESIGN.md section 7.3 assumes and what ADR 0006 froze as the
/// canonical representation. A bare hex digest is accepted only where a
/// repository context supplies the algorithm.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    let bytes = hex.as_bytes();
    let mut raw = Vec::with_capacity(algorithm.raw_len());
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
    // Git prints lowercase; accepting uppercase on input is harmless and avoids
    // a class of copy-paste failures from tools that upcase.
    b'A'..=b'F' => Ok(b - b'A' + 10),
    _ => Err(OidError::NotHex),
  }
}

/// Whether every byte is a hex digit. Used to classify a revision selector
/// without deciding whether its length is an acceptable abbreviation.
pub fn is_hex(s: &str) -> bool {
  !s.is_empty() && s.as_bytes().iter().all(|b| hex_val(*b).is_ok())
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

impl<'de> serde::Deserialize<'de> for ObjectId {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
    ObjectId::parse_qualified(&s).map_err(serde::de::Error::custom)
  }
}

#[derive(Debug, PartialEq, Eq, Clone)]
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
    // A SHA-256 digest offered as SHA-1 must not be silently truncated. This is
    // the concrete failure DESIGN.md section 6.1's rule exists to prevent.
    let err = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(32)).unwrap_err();
    assert!(matches!(err, OidError::Length { .. }));
  }

  #[test]
  fn oid_of_different_algorithms_never_compares_equal() {
    // Both are 32 hex characters of the same value, but one is a truncated
    // SHA-256 and the other is not a valid SHA-1 at all.
    let a = ObjectId::from_hex(HashAlgorithm::Sha256, &"00".repeat(32)).unwrap();
    let b = ObjectId::from_hex(HashAlgorithm::Sha1, &"00".repeat(20)).unwrap();
    assert_ne!(a, b);
  }

  #[test]
  fn qualified_form_round_trips_through_serde() {
    let oid = ObjectId::from_hex(HashAlgorithm::Sha1, &"3a".repeat(20)).unwrap();
    let json = serde_json::to_string(&oid).unwrap();
    assert_eq!(json, format!("\"sha1:{}\"", "3a".repeat(20)));
    let back: ObjectId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, oid);
  }
}
