//! Repository paths as raw bytes.
//!
//! Git paths are bytes, not UTF-8. ADR 0006 answers open question 7 with the
//! measurement that no corpus repository tip actually contains a non-UTF-8 path,
//! and then keeps byte handling anyway, because it is far cheaper to build in
//! than to retrofit. The `bytes` fixture keeps it tested.
//!
//! The wire form is base64url (`path_b64url`), which avoids ambiguous URL
//! normalization of encoded slashes and non-UTF-8 names. Encoding and decoding
//! are implemented here rather than pulled from a crate so that `xvfs-types`
//! stays dependency-light; both directions are tested against RFC 4648 vectors.

use std::fmt;

use crate::error::{ErrorCode, XvfsError};
use crate::limits;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BytePath(Vec<u8>);

impl BytePath {
  pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
    BytePath(bytes.into())
  }

  /// The empty path, meaning the root of a snapshot.
  pub fn root() -> Self {
    BytePath(Vec::new())
  }

  pub fn as_bytes(&self) -> &[u8] {
    &self.0
  }

  pub fn into_bytes(self) -> Vec<u8> {
    self.0
  }

  pub fn components(&self) -> impl Iterator<Item = &[u8]> {
    self.0.split(|b| *b == b'/').filter(|c| !c.is_empty())
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }

  pub fn len(&self) -> usize {
    self.0.len()
  }

  /// The final path component, or `None` for the root.
  pub fn file_name(&self) -> Option<&[u8]> {
    self.components().last()
  }

  /// Append a child component. Used when building a directory listing's entry
  /// paths from a parent path plus a tree entry name.
  pub fn join(&self, component: &[u8]) -> BytePath {
    let mut out = self.0.clone();
    if !out.is_empty() && !out.ends_with(b"/") {
      out.push(b'/');
    }
    out.extend_from_slice(component);
    BytePath(out)
  }

  /// Validate a caller-supplied path before it reaches tree traversal.
  ///
  /// DESIGN.md section 10 item 3 requires repository paths to stay validated
  /// byte strings and item 4 requires typed validation before a path becomes a
  /// filesystem or command argument. This is where both happen, so that no
  /// later layer has to remember to.
  ///
  /// A path that fails validation is rejected rather than normalized. A
  /// traversal attempt is a bug or an attack; silently rewriting it to
  /// something valid hides which one it was.
  pub fn validate(&self) -> Result<(), XvfsError> {
    if self.0.len() > limits::MAX_PATH_BYTES {
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        format!("path exceeds {} bytes", limits::MAX_PATH_BYTES),
      ));
    }
    if self.0.contains(&0) {
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        "path contains NUL",
      ));
    }
    if self.0.starts_with(b"/") {
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        "path is absolute; snapshot paths are relative to the tree root",
      ));
    }
    let mut count = 0usize;
    for c in self.0.split(|b| *b == b'/') {
      // An empty component is either a leading, trailing, or doubled slash.
      // Git trees cannot contain one, so accepting it would mean two spellings
      // of the same path -- and two spellings is what path-traversal bugs are
      // built from.
      if c.is_empty() {
        if self.0.is_empty() {
          continue;
        }
        return Err(XvfsError::new(
          ErrorCode::InvalidArgument,
          "path has an empty component",
        ));
      }
      if c == b"." || c == b".." {
        return Err(XvfsError::new(
          ErrorCode::InvalidArgument,
          "path contains a relative component",
        ));
      }
      count += 1;
      if count > limits::MAX_PATH_COMPONENTS {
        return Err(XvfsError::new(
          ErrorCode::InvalidArgument,
          format!("path exceeds {} components", limits::MAX_PATH_COMPONENTS),
        ));
      }
    }
    Ok(())
  }

  /// Lossless display form: valid UTF-8 verbatim, anything else escaped. The
  /// escape is unambiguous because a literal backslash is doubled.
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

  pub fn to_b64url(&self) -> String {
    b64url_encode(&self.0)
  }

  pub fn from_b64url(s: &str) -> Result<Self, XvfsError> {
    Ok(BytePath(b64url_decode(s)?))
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
  /// Serializes as `{"escaped": ..., "b64url": ...}`. The escaped form is for
  /// humans; the base64url form is the one a client must round-trip, matching
  /// the `path_b64url` parameter in DESIGN.md section 7.3.
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut m = s.serialize_map(Some(2))?;
    m.serialize_entry("escaped", &self.escaped())?;
    m.serialize_entry("b64url", &self.to_b64url())?;
    m.end()
  }
}

impl<'de> serde::Deserialize<'de> for BytePath {
  /// Reads back the form [`BytePath`]'s `Serialize` writes.
  ///
  /// `b64url` is authoritative and `escaped` is ignored. The escaped form is for
  /// humans and is deliberately not round-trippable through this path: two
  /// different byte strings can have escaped forms that a careless reader would
  /// treat as equal, so decoding from it would reintroduce exactly the ambiguity
  /// the two-field representation exists to avoid.
  ///
  /// A bare string is also accepted and read as base64url, because that is what a
  /// hand-written config or a `path_b64url` query parameter carries.
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    struct V;

    impl<'de> serde::de::Visitor<'de> for V {
      type Value = BytePath;

      fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a base64url string or a map containing a `b64url` field")
      }

      fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<BytePath, E> {
        BytePath::from_b64url(s).map_err(serde::de::Error::custom)
      }

      fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<BytePath, A::Error> {
        let mut b64: Option<String> = None;
        while let Some(key) = m.next_key::<std::borrow::Cow<'de, str>>()? {
          match key.as_ref() {
            "b64url" => b64 = Some(m.next_value()?),
            // Skipped rather than rejected so an added display field does not
            // break older readers.
            _ => {
              m.next_value::<serde::de::IgnoredAny>()?;
            }
          }
        }
        let b64 = b64.ok_or_else(|| serde::de::Error::missing_field("b64url"))?;
        BytePath::from_b64url(&b64).map_err(serde::de::Error::custom)
      }
    }

    d.deserialize_any(V)
  }
}

const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url, per RFC 4648 section 5. Unpadded because the value
/// appears in a URL query parameter, where `=` would need escaping.
pub fn b64url_encode(bytes: &[u8]) -> String {
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

pub fn b64url_decode(s: &str) -> Result<Vec<u8>, XvfsError> {
  fn val(b: u8) -> Option<u32> {
    match b {
      b'A'..=b'Z' => Some((b - b'A') as u32),
      b'a'..=b'z' => Some((b - b'a') as u32 + 26),
      b'0'..=b'9' => Some((b - b'0') as u32 + 52),
      b'-' => Some(62),
      b'_' => Some(63),
      _ => None,
    }
  }
  let bad = || XvfsError::new(ErrorCode::InvalidArgument, "value is not base64url");
  // Padding is accepted on input but never produced, because some clients add
  // it. Rejecting it would be a needless interoperability failure.
  let s = s.trim_end_matches('=');
  let mut out = Vec::with_capacity(s.len() * 3 / 4);
  for chunk in s.as_bytes().chunks(4) {
    // A 1-character group encodes 6 bits, which cannot complete a byte. Such a
    // group means the input was truncated.
    if chunk.len() == 1 {
      return Err(bad());
    }
    let mut n: u32 = 0;
    for (i, b) in chunk.iter().enumerate() {
      n |= val(*b).ok_or_else(bad)? << (18 - 6 * i);
    }
    out.push((n >> 16) as u8);
    if chunk.len() > 2 {
      out.push((n >> 8) as u8);
    }
    if chunk.len() > 3 {
      out.push(n as u8);
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn b64url_matches_rfc4648_vectors_both_directions() {
    for (raw, encoded) in [
      (&b""[..], ""),
      (b"f", "Zg"),
      (b"fo", "Zm8"),
      (b"foo", "Zm9v"),
      (b"foob", "Zm9vYg"),
      (b"fooba", "Zm9vYmE"),
      (b"foobar", "Zm9vYmFy"),
    ] {
      assert_eq!(b64url_encode(raw), encoded, "encoding {raw:?}");
      assert_eq!(b64url_decode(encoded).unwrap(), raw, "decoding {encoded:?}");
    }
    // The two characters that distinguish base64url from base64.
    assert_eq!(b64url_encode(&[0xfb, 0xef]), "--8");
    assert_eq!(b64url_decode("--8").unwrap(), vec![0xfb, 0xef]);
  }

  #[test]
  fn b64url_round_trips_a_non_utf8_path() {
    let p = BytePath::new(b"drivers/\xff\xfe/x.c".to_vec());
    assert_eq!(BytePath::from_b64url(&p.to_b64url()).unwrap(), p);
  }

  #[test]
  fn b64url_rejects_invalid_input() {
    // `+` and `/` are base64, not base64url; accepting them would make two
    // encodings map to one path.
    assert!(b64url_decode("Zm9v+g").is_err());
    assert!(b64url_decode("Zm9v/g").is_err());
    // A trailing 6-bit group cannot complete a byte.
    assert!(b64url_decode("Zm9vY").is_err());
  }

  #[test]
  fn validate_rejects_traversal_and_absolute_paths() {
    for bad in [
      &b"../etc/passwd"[..],
      b"a/../../b",
      b"/etc/passwd",
      b"a//b",
      b"a/./b",
      b"a/",
      b"with\0nul",
    ] {
      assert!(
        BytePath::new(bad.to_vec()).validate().is_err(),
        "should reject {:?}",
        BytePath::new(bad.to_vec())
      );
    }
  }

  #[test]
  fn validate_accepts_the_root_and_ordinary_paths() {
    BytePath::root().validate().unwrap();
    BytePath::new(b"drivers/net/ethernet/Makefile".to_vec())
      .validate()
      .unwrap();
    // A non-UTF-8 name is a valid Git path and must pass.
    BytePath::new(b"drivers/\xffbad.c".to_vec())
      .validate()
      .unwrap();
    // `..` is only rejected as a whole component. A name that merely contains
    // dots is ordinary.
    BytePath::new(b"a/..b/c...d".to_vec()).validate().unwrap();
  }

  #[test]
  fn serde_round_trips_a_non_utf8_path_losslessly() {
    let p = BytePath::new(b"drivers/\xff\xfe/x.c".to_vec());
    let json = serde_json::to_string(&p).unwrap();
    // Both forms are present: one for a human reading a log, one for a machine.
    assert!(json.contains("\"escaped\""));
    assert!(json.contains("\"b64url\""));
    assert_eq!(serde_json::from_str::<BytePath>(&json).unwrap(), p);
    // And a bare base64url string is accepted, which is the query-parameter form.
    let bare = format!("\"{}\"", p.to_b64url());
    assert_eq!(serde_json::from_str::<BytePath>(&bare).unwrap(), p);
  }

  #[test]
  fn deserialization_ignores_the_escaped_form_even_when_it_disagrees() {
    // `escaped` is for humans and is not round-trippable: two different byte
    // strings can escape to forms a careless reader treats as equal. `b64url` is
    // therefore authoritative, and a mismatched `escaped` must not win.
    let json = r#"{"escaped":"totally/different","b64url":"Zm9v"}"#;
    assert_eq!(
      serde_json::from_str::<BytePath>(json).unwrap(),
      BytePath::new("foo")
    );
  }

  #[test]
  fn join_builds_child_paths_from_the_root_without_a_leading_slash() {
    assert_eq!(BytePath::root().join(b"a").as_bytes(), b"a");
    assert_eq!(BytePath::new("a").join(b"b").as_bytes(), b"a/b");
  }
}
