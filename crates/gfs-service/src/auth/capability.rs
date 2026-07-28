//! Mount capabilities and blob tickets: HMAC-signed, self-contained grants.
//!
//! DESIGN.md section 7.1 requires "an unforgeable capability binding subject,
//! repository, commit, mount ID, and expiry", and section 7.3 requires a
//! short-lived blob ticket binding repository, commit, blob OID, subject, and
//! expiry. Both are implemented here over one signing primitive.
//!
//! # Why not a JWT
//!
//! A JWT would bring an algorithm field the verifier has to police, a JSON parser
//! on the trust boundary, and a specification whose historical vulnerabilities are
//! almost all confusion about *which* key or algorithm applies. These tokens are
//! read only by the service that issued them, so none of that flexibility buys
//! anything. HMAC-SHA256 over a canonical encoding, with the algorithm fixed by
//! the token's own prefix, removes the whole class.
//!
//! # Why the encoding is length-prefixed
//!
//! A delimiter-joined encoding is forgeable when an attacker controls a field. A
//! subject may legitimately contain almost any character, so with `|`-joined fields
//! a subject of `x|repo-b|...` could shift every later field one position and
//! authorize a different repository under a valid signature. Length prefixes make
//! the field boundaries part of the signed bytes, so no field's content can be
//! reinterpreted as structure.
//!
//! # Why the type tag is inside the signed bytes
//!
//! Without domain separation a mount capability and a blob ticket signed by the
//! same key are interchangeable, and a long-lived mount capability would be usable
//! wherever a deliberately short-lived blob ticket is expected.

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::path::{b64url_decode, b64url_encode};
use gfs_types::{MountId, ObjectId, RepositoryId, SubjectId, Timestamp};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// The token format prefix. Bumping it invalidates every outstanding token, which
/// is the intended effect of a format change.
const PREFIX: &str = "gfs1";

/// Domain tags. Part of the signed bytes, so a token of one kind can never verify
/// as the other.
const TAG_MOUNT: u8 = 1;
const TAG_BLOB: u8 = 2;

/// The signing key for capabilities and tickets.
///
/// Held only by the server. `Debug` deliberately prints nothing about the bytes:
/// a key that reaches a log through a struct dump is a key that has to be rotated.
#[derive(Clone)]
pub struct CapabilityKey {
  secret: Vec<u8>,
}

impl std::fmt::Debug for CapabilityKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("CapabilityKey(<redacted>)")
  }
}

impl CapabilityKey {
  /// Build a key from at least 32 bytes of secret material.
  ///
  /// The minimum is enforced rather than documented: a short key is the one
  /// configuration mistake that silently makes every capability forgeable.
  pub fn new(secret: Vec<u8>) -> Result<Self, GfsError> {
    if secret.len() < 32 {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        "the capability signing key must be at least 32 bytes",
      ));
    }
    Ok(CapabilityKey { secret })
  }

  /// Generate a fresh random key.
  pub fn generate() -> Result<Self, GfsError> {
    CapabilityKey::new(crate::util::random_bytes(32)?)
  }

  fn sign(&self, bytes: &[u8]) -> Vec<u8> {
    let mut mac =
      HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a key of any length");
    mac.update(bytes);
    mac.finalize().into_bytes().to_vec()
  }

  fn verify(&self, bytes: &[u8], tag: &[u8]) -> bool {
    // Constant-time comparison. A byte-by-byte `==` leaks the length of the
    // matching prefix through timing, which is enough to forge a tag one byte at a
    // time given enough attempts.
    self.sign(bytes).ct_eq(tag).into()
  }
}

/// A decoded, verified mount capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountCapability {
  pub subject: SubjectId,
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub mount_id: MountId,
  pub expires_at: Timestamp,
}

/// A decoded, verified blob ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobTicket {
  pub subject: SubjectId,
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub blob: ObjectId,
  pub expires_at: Timestamp,
}

/// Length-prefixed canonical encoding.
///
/// Each field is a 4-byte big-endian length followed by its bytes, so no field's
/// content can be read as a boundary.
fn encode_fields(tag: u8, fields: &[&[u8]]) -> Vec<u8> {
  let mut out = Vec::new();
  out.push(tag);
  for f in fields {
    out.extend_from_slice(&(f.len() as u32).to_be_bytes());
    out.extend_from_slice(f);
  }
  out
}

fn decode_fields(bytes: &[u8], expected_tag: u8, count: usize) -> Result<Vec<Vec<u8>>, GfsError> {
  let bad = || GfsError::new(ErrorCode::Unauthenticated, "malformed capability");
  let (&tag, mut rest) = bytes.split_first().ok_or_else(bad)?;
  if tag != expected_tag {
    // A blob ticket presented as a mount capability, or the reverse.
    return Err(GfsError::new(
      ErrorCode::Unauthenticated,
      "capability is of the wrong kind",
    ));
  }
  let mut out = Vec::with_capacity(count);
  for _ in 0..count {
    if rest.len() < 4 {
      return Err(bad());
    }
    let (len_bytes, tail) = rest.split_at(4);
    let len = u32::from_be_bytes(len_bytes.try_into().map_err(|_| bad())?) as usize;
    if tail.len() < len {
      return Err(bad());
    }
    let (field, tail) = tail.split_at(len);
    out.push(field.to_vec());
    rest = tail;
  }
  if !rest.is_empty() {
    // Trailing bytes are signed but unread. Accepting them would allow two
    // distinct tokens with the same meaning, which breaks any later attempt to
    // revoke or deduplicate one.
    return Err(bad());
  }
  Ok(out)
}

fn assemble(key: &CapabilityKey, signed: &[u8]) -> String {
  let mac = key.sign(signed);
  format!("{PREFIX}.{}.{}", b64url_encode(signed), b64url_encode(&mac))
}

fn disassemble(key: &CapabilityKey, token: &str) -> Result<Vec<u8>, GfsError> {
  let bad = || GfsError::new(ErrorCode::Unauthenticated, "malformed capability");
  let mut parts = token.split('.');
  let (Some(prefix), Some(payload), Some(mac), None) =
    (parts.next(), parts.next(), parts.next(), parts.next())
  else {
    return Err(bad());
  };
  if prefix != PREFIX {
    return Err(GfsError::new(
      ErrorCode::Unauthenticated,
      "unsupported capability format",
    ));
  }
  let signed = b64url_decode(payload).map_err(|_| bad())?;
  let mac = b64url_decode(mac).map_err(|_| bad())?;

  // The signature is checked **before** any field is parsed or trusted. Parsing
  // first would run the decoder on attacker-controlled bytes and, worse, tempt a
  // later change into reporting *why* an unsigned token was malformed.
  if !key.verify(&signed, &mac) {
    return Err(GfsError::new(
      ErrorCode::Unauthenticated,
      "capability signature is invalid",
    ));
  }
  Ok(signed)
}

impl MountCapability {
  pub fn issue(key: &CapabilityKey, cap: &MountCapability) -> String {
    let expiry = cap.expires_at.secs.to_be_bytes();
    assemble(
      key,
      &encode_fields(
        TAG_MOUNT,
        &[
          cap.subject.as_str().as_bytes(),
          cap.repository_id.as_str().as_bytes(),
          cap.commit.to_qualified().as_bytes(),
          cap.mount_id.as_str().as_bytes(),
          &expiry,
        ],
      ),
    )
  }

  /// Verify a token and check that it has not expired.
  pub fn verify(key: &CapabilityKey, token: &str, now: Timestamp) -> Result<Self, GfsError> {
    MountCapability::verify_with_tolerance(key, token, now, std::time::Duration::ZERO)
  }

  /// Verify a token, accepting one that expired within `tolerance`.
  ///
  /// Needed by lease renewal, and only by lease renewal. A capability expires with
  /// its lease, so a daemon renewing during ADR 0006's grace interval necessarily
  /// presents a just-expired token -- rejecting it would make the grace interval
  /// unreachable, which would defeat the mechanism that exists so a transient
  /// renewal failure does not destroy a live workspace.
  ///
  /// The tolerance relaxes *freshness only*. The signature is still required, so
  /// authenticity is unaffected, and read paths pass `ZERO` so an expired
  /// capability cannot read anything.
  pub fn verify_with_tolerance(
    key: &CapabilityKey,
    token: &str,
    now: Timestamp,
    tolerance: std::time::Duration,
  ) -> Result<Self, GfsError> {
    let signed = disassemble(key, token)?;
    let fields = decode_fields(&signed, TAG_MOUNT, 5)?;
    let cap = MountCapability {
      subject: SubjectId::parse(&utf8(&fields[0])?)?,
      repository_id: RepositoryId::parse(&utf8(&fields[1])?)?,
      commit: ObjectId::parse_qualified(&utf8(&fields[2])?)?,
      mount_id: MountId::parse(&utf8(&fields[3])?)?,
      expires_at: Timestamp::from_secs(i64::from_be_bytes(
        fields[4]
          .as_slice()
          .try_into()
          .map_err(|_| GfsError::new(ErrorCode::Unauthenticated, "malformed capability"))?,
      )),
    };
    let deadline = cap
      .expires_at
      .secs
      .saturating_add(tolerance.as_secs() as i64);
    if now.secs > deadline {
      return Err(GfsError::new(
        ErrorCode::Expired,
        "mount capability has expired",
      ));
    }
    Ok(cap)
  }
}

impl BlobTicket {
  pub fn issue(key: &CapabilityKey, ticket: &BlobTicket) -> String {
    let expiry = ticket.expires_at.secs.to_be_bytes();
    assemble(
      key,
      &encode_fields(
        TAG_BLOB,
        &[
          ticket.subject.as_str().as_bytes(),
          ticket.repository_id.as_str().as_bytes(),
          ticket.commit.to_qualified().as_bytes(),
          ticket.blob.to_qualified().as_bytes(),
          &expiry,
        ],
      ),
    )
  }

  pub fn verify(key: &CapabilityKey, token: &str, now: Timestamp) -> Result<Self, GfsError> {
    let signed = disassemble(key, token)?;
    let fields = decode_fields(&signed, TAG_BLOB, 5)?;
    let ticket = BlobTicket {
      subject: SubjectId::parse(&utf8(&fields[0])?)?,
      repository_id: RepositoryId::parse(&utf8(&fields[1])?)?,
      commit: ObjectId::parse_qualified(&utf8(&fields[2])?)?,
      blob: ObjectId::parse_qualified(&utf8(&fields[3])?)?,
      expires_at: Timestamp::from_secs(i64::from_be_bytes(
        fields[4]
          .as_slice()
          .try_into()
          .map_err(|_| GfsError::new(ErrorCode::Unauthenticated, "malformed ticket"))?,
      )),
    };
    if now.secs > ticket.expires_at.secs {
      return Err(GfsError::new(ErrorCode::Expired, "blob ticket has expired"));
    }
    Ok(ticket)
  }
}

fn utf8(bytes: &[u8]) -> Result<String, GfsError> {
  String::from_utf8(bytes.to_vec())
    .map_err(|_| GfsError::new(ErrorCode::Unauthenticated, "malformed capability"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;

  fn key() -> CapabilityKey {
    CapabilityKey::new(vec![7u8; 32]).unwrap()
  }

  fn oid(b: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[b; 20]).unwrap()
  }

  fn cap() -> MountCapability {
    MountCapability {
      subject: SubjectId::parse("job-123").unwrap(),
      repository_id: RepositoryId::parse("r-abc").unwrap(),
      commit: oid(1),
      mount_id: MountId::parse("m-0f3a").unwrap(),
      expires_at: Timestamp::from_secs(2_000_000_000),
    }
  }

  fn now() -> Timestamp {
    Timestamp::from_secs(1_600_000_000)
  }

  #[test]
  fn a_capability_round_trips() {
    let k = key();
    let token = MountCapability::issue(&k, &cap());
    assert_eq!(MountCapability::verify(&k, &token, now()).unwrap(), cap());
  }

  #[test]
  fn a_short_key_is_refused_at_construction() {
    // The one configuration mistake that silently makes every capability
    // forgeable.
    assert!(CapabilityKey::new(vec![0u8; 16]).is_err());
    assert!(CapabilityKey::new(vec![0u8; 32]).is_ok());
  }

  #[test]
  fn a_key_never_prints_its_bytes() {
    // 0xCD rather than 0xAB: the hex "ab" occurs in "Capability" itself, so a
    // substring check for it would fail on the type name and prove nothing.
    let k = CapabilityKey::new(vec![0xCDu8; 32]).unwrap();
    let printed = format!("{k:?}");
    assert!(!printed.contains("cd"), "{printed}");
    assert!(!printed.contains("205"), "{printed}");
    assert!(printed.contains("redacted"));
  }

  #[test]
  fn a_token_signed_by_another_key_is_rejected() {
    let token = MountCapability::issue(&key(), &cap());
    let other = CapabilityKey::new(vec![9u8; 32]).unwrap();
    assert_eq!(
      MountCapability::verify(&other, &token, now())
        .unwrap_err()
        .code,
      ErrorCode::Unauthenticated
    );
  }

  #[test]
  fn any_single_bit_flip_invalidates_a_token() {
    let k = key();
    let token = MountCapability::issue(&k, &cap());
    let bytes = token.as_bytes();
    // Sample positions across the payload and the MAC rather than all of them,
    // which keeps the test fast while still covering both halves.
    for i in (0..bytes.len()).step_by(3) {
      let mut mutated = bytes.to_vec();
      // Flip to a different character in the base64url alphabet, or break the
      // structure -- either way it must not verify.
      mutated[i] = if mutated[i] == b'A' { b'B' } else { b'A' };
      let tampered = String::from_utf8_lossy(&mutated).into_owned();
      if tampered == token {
        continue;
      }
      assert!(
        MountCapability::verify(&k, &tampered, now()).is_err(),
        "byte {i} could be changed without invalidating the token"
      );
    }
  }

  #[test]
  fn a_blob_ticket_cannot_be_used_as_a_mount_capability() {
    // Domain separation. Without the tag inside the signed bytes, a long-lived
    // mount capability would be usable wherever a deliberately short-lived blob
    // ticket is expected -- and vice versa.
    let k = key();
    let ticket = BlobTicket {
      subject: SubjectId::parse("job-123").unwrap(),
      repository_id: RepositoryId::parse("r-abc").unwrap(),
      commit: oid(1),
      blob: oid(2),
      expires_at: Timestamp::from_secs(2_000_000_000),
    };
    let ticket_token = BlobTicket::issue(&k, &ticket);
    let cap_token = MountCapability::issue(&k, &cap());

    assert!(MountCapability::verify(&k, &ticket_token, now()).is_err());
    assert!(BlobTicket::verify(&k, &cap_token, now()).is_err());
    // Each still verifies as itself.
    assert_eq!(
      BlobTicket::verify(&k, &ticket_token, now()).unwrap(),
      ticket
    );
    assert_eq!(
      MountCapability::verify(&k, &cap_token, now()).unwrap(),
      cap()
    );
  }

  #[test]
  fn field_boundaries_cannot_be_shifted_by_a_crafted_subject() {
    // The attack length prefixes exist to stop. With `|`-joined fields, a subject
    // containing the delimiter could make the verifier read a different repository
    // out of a validly signed token.
    let k = key();
    let attacker = MountCapability {
      subject: SubjectId::parse("evil|r-victim|sha1:00").unwrap(),
      ..cap()
    };
    let token = MountCapability::issue(&k, &attacker);
    let decoded = MountCapability::verify(&k, &token, now()).unwrap();
    // The subject is returned intact, and the repository is untouched.
    assert_eq!(decoded.subject.as_str(), "evil|r-victim|sha1:00");
    assert_eq!(decoded.repository_id.as_str(), "r-abc");
    assert_eq!(decoded, attacker);
  }

  #[test]
  fn an_expired_capability_is_reported_as_expired_not_as_invalid() {
    // A client has to be able to tell "re-authenticate" from "this token was
    // never valid"; conflating them makes a renewal loop indistinguishable from
    // an attack.
    let k = key();
    let token = MountCapability::issue(&k, &cap());
    let later = Timestamp::from_secs(cap().expires_at.secs + 1);
    let err = MountCapability::verify(&k, &token, later).unwrap_err();
    assert_eq!(err.code, ErrorCode::Expired);
    // Still valid at exactly its expiry second.
    assert!(MountCapability::verify(&k, &token, cap().expires_at).is_ok());
  }

  #[test]
  fn trailing_bytes_are_rejected() {
    // Two distinct tokens with the same meaning would break any later attempt to
    // revoke or deduplicate one.
    let k = key();
    let mut signed = encode_fields(
      TAG_MOUNT,
      &[
        b"job-123",
        b"r-abc",
        oid(1).to_qualified().as_bytes(),
        b"m-0f3a",
        &2_000_000_000i64.to_be_bytes(),
      ],
    );
    signed.push(0xff);
    let token = assemble(&k, &signed);
    assert!(MountCapability::verify(&k, &token, now()).is_err());
  }

  #[test]
  fn malformed_tokens_are_rejected_without_panicking() {
    let k = key();
    for bad in [
      "",
      ".",
      "..",
      "gfs1",
      "gfs1.",
      "gfs1..",
      "gfs2.AAAA.AAAA",
      "gfs1.!!!!.AAAA",
      "gfs1.AAAA.!!!!",
      "gfs1.AAAA.AAAA.AAAA",
      "gfs1.AAAA.AAAA",
    ] {
      assert!(
        MountCapability::verify(&k, bad, now()).is_err(),
        "{bad:?} must be rejected"
      );
    }
  }

  #[test]
  fn a_generated_key_differs_every_time() {
    let a = CapabilityKey::generate().unwrap();
    let b = CapabilityKey::generate().unwrap();
    let token = MountCapability::issue(&a, &cap());
    // A token from one generated key must not verify under another.
    assert!(MountCapability::verify(&b, &token, now()).is_err());
  }
}
