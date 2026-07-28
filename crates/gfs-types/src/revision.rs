//! Revision selectors.
//!
//! PLAN.md M1.3 says "resolve branch, tag, full commit OID, and allowed
//! abbreviation safely". That list is exhaustive, and this module makes it so by
//! parsing a selector into a closed enumeration *before* libgit2 sees it.
//!
//! The M0 spike passed the caller's string straight to `revparse_single`, which
//! is the right shape for a measurement and the wrong shape for a service.
//! `revparse` accepts an expression language -- `HEAD@{2}`, `main^{tree}`,
//! `v1..v2`, `:/commit message`, `@{-1}` -- with reachability, ordering, and
//! object-type semantics GFS does not intend to expose. `main^{tree}` alone
//! would hand a caller a tree OID where every downstream layer expects a commit.
//! So the grammar is closed here rather than filtered downstream, on the same
//! reasoning ADR 0006 gives for rejecting `refs/gfs/` at the lowest layer: a
//! rule enforced in one place cannot be forgotten in another.

use crate::error::{ErrorCode, GfsError};
use crate::limits;
use crate::oid::{is_hex, HashAlgorithm, ObjectId};

/// The reserved internal reachability namespace (DESIGN.md section 7.1).
pub const RESERVED_REF_PREFIX: &str = "refs/gfs/";

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RevisionSelector {
  /// A full-length hex digest. The algorithm comes from the repository.
  FullOid(ObjectId),
  /// A hex prefix of at least [`limits::MIN_ABBREV_HEX`] characters. Resolution
  /// must fail on ambiguity rather than pick a candidate.
  Abbrev(String),
  /// A fully qualified ref name, `refs/...`.
  FullRef(String),
  /// A branch or tag short name, or the literal `HEAD`.
  ShortName(String),
}

impl RevisionSelector {
  /// Parse and validate a caller-supplied selector.
  ///
  /// `algorithm` is the repository's hash algorithm, needed to decide whether a
  /// hex string is a full object ID or an abbreviation.
  pub fn parse(input: &str, algorithm: HashAlgorithm) -> Result<Self, GfsError> {
    if input.is_empty() {
      return Err(GfsError::invalid("revision selector is empty"));
    }
    if input.len() > limits::MAX_REVISION_SELECTOR_BYTES {
      return Err(GfsError::invalid(format!(
        "revision selector exceeds {} bytes",
        limits::MAX_REVISION_SELECTOR_BYTES
      )));
    }

    // The qualified `algorithm:hex` form, which is what GFS itself emits.
    if let Some((algo, hex)) = input.split_once(':') {
      if HashAlgorithm::from_name(algo).is_some() {
        let oid = ObjectId::parse_qualified(input)?;
        if oid.algorithm() != algorithm {
          return Err(GfsError::invalid(format!(
            "object id is {} but this repository is {algorithm}",
            oid.algorithm()
          )));
        }
        return Ok(RevisionSelector::FullOid(oid));
      }
      // A colon that is not an algorithm prefix is `rev:path` or `:/text`
      // revparse syntax, which the closed grammar does not accept.
      let _ = hex;
      return Err(unsupported_expression(input));
    }

    if is_hex(input) {
      if input.len() == algorithm.hex_len() {
        return Ok(RevisionSelector::FullOid(ObjectId::from_hex(
          algorithm, input,
        )?));
      }
      if input.len() > algorithm.hex_len() {
        return Err(GfsError::invalid(format!(
          "hex digest is longer than {} characters",
          algorithm.hex_len()
        )));
      }
      if input.len() >= limits::MIN_ABBREV_HEX {
        return Ok(RevisionSelector::Abbrev(input.to_owned()));
      }
      // Short enough to be a ref name that happens to be all hex, such as a
      // branch called `abc`. Fall through and treat it as a name; the
      // ambiguity is Git's, and refusing to guess is the safe half of it.
    }

    // Namespace before well-formedness: a selector that names `refs/gfs/` gets
    // that diagnostic whether or not it is also malformed, so probing the
    // namespace always produces one answer. ADR 0002 already records that
    // hiding the namespace is not an authorization boundary, so the specific
    // error costs nothing and saves a caller from a misleading one.
    reject_reserved_namespace(input)?;
    reject_expression_syntax(input)?;

    if let Some(rest) = input.strip_prefix("refs/") {
      validate_ref_name(input)?;
      if rest.is_empty() {
        return Err(GfsError::invalid("ref name is just `refs/`"));
      }
      return Ok(RevisionSelector::FullRef(input.to_owned()));
    }

    if input == "HEAD" {
      return Ok(RevisionSelector::ShortName(input.to_owned()));
    }
    // Other all-caps pseudo-refs (`ORIG_HEAD`, `FETCH_HEAD`, `MERGE_HEAD`) name
    // per-worktree state that has no meaning in a server-side bare mirror.
    // Rejecting them keeps a caller from silently resolving whatever a
    // maintenance operation happened to leave behind.
    if input.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
      return Err(GfsError::invalid(format!(
        "{input:?} is a per-worktree pseudo-ref and has no meaning in a bare mirror"
      )));
    }
    validate_ref_name(input)?;
    Ok(RevisionSelector::ShortName(input.to_owned()))
  }

  /// The string form to hand to ref resolution. Always one of the four accepted
  /// shapes, never an expression.
  pub fn as_str(&self) -> &str {
    match self {
      RevisionSelector::FullOid(_) => "",
      RevisionSelector::Abbrev(s)
      | RevisionSelector::FullRef(s)
      | RevisionSelector::ShortName(s) => s,
    }
  }
}

fn unsupported_expression(input: &str) -> GfsError {
  GfsError::invalid(format!(
    "revision selector {input:?} is not a branch, tag, object id, or allowed \
     abbreviation; revision expressions are not supported"
  ))
}

/// Reject the characters that make a string a `revparse` expression rather than
/// a name. Also rejects the characters `git check-ref-format` forbids, so the
/// two rules do not have to be kept in sync in two places.
fn reject_expression_syntax(input: &str) -> Result<(), GfsError> {
  if input.starts_with('-') {
    // Would become an option if it ever reached a command line. DESIGN.md
    // section 10 item 4 requires this be impossible by validation.
    return Err(GfsError::invalid(
      "revision selector may not start with `-`",
    ));
  }
  if input.starts_with('.') || input.ends_with('.') {
    return Err(GfsError::invalid(
      "revision selector may not start or end with `.`",
    ));
  }
  if input.contains("..") || input.contains("@{") || input.contains("//") {
    return Err(unsupported_expression(input));
  }
  if input.ends_with(".lock") || input.ends_with('/') || input.starts_with('/') {
    return Err(GfsError::invalid(format!(
      "{input:?} is not a valid ref name"
    )));
  }
  if input == "@" {
    return Err(unsupported_expression(input));
  }
  for c in input.chars() {
    match c {
      // revparse operators.
      '~' | '^' | ':' | '@' => return Err(unsupported_expression(input)),
      // Forbidden by git check-ref-format, and glob characters would otherwise
      // let one selector name a set of refs.
      '?' | '*' | '[' | ']' | '\\' | '\x7f' => {
        return Err(GfsError::invalid(format!(
          "{input:?} is not a valid ref name"
        )))
      }
      c if c.is_whitespace() || c.is_control() => {
        return Err(GfsError::invalid(format!(
          "{input:?} is not a valid ref name"
        )))
      }
      _ => {}
    }
  }
  Ok(())
}

/// Reject anything that would resolve into the reserved namespace.
///
/// Checking only the `refs/gfs/` prefix is not enough. Git resolves a short
/// name by trying `refs/<name>` among other prefixes, so the short name
/// `gfs/mounts/<id>` reaches `refs/gfs/mounts/<id>` -- a lease anchor -- while
/// looking nothing like the reserved prefix. Both spellings are rejected here.
fn reject_reserved_namespace(input: &str) -> Result<(), GfsError> {
  let reserved = input.starts_with(RESERVED_REF_PREFIX)
    || format!("refs/{input}").starts_with(RESERVED_REF_PREFIX);
  if reserved {
    return Err(GfsError::new(
      ErrorCode::ReservedNamespace,
      format!("{RESERVED_REF_PREFIX} is an internal namespace and cannot be named as a revision"),
    ));
  }
  Ok(())
}

fn validate_ref_name(input: &str) -> Result<(), GfsError> {
  for component in input.split('/') {
    if component.is_empty() {
      return Err(GfsError::invalid(format!(
        "{input:?} is not a valid ref name"
      )));
    }
    if component.starts_with('.') || component.ends_with(".lock") {
      return Err(GfsError::invalid(format!(
        "{input:?} is not a valid ref name"
      )));
    }
  }
  Ok(())
}

/// Whether a ref name belongs to the reserved internal namespace. Used by ref
/// enumeration and the upload-pack hidden-ref policy.
pub fn is_reserved_ref(name: &str) -> bool {
  name.starts_with(RESERVED_REF_PREFIX)
}

/// The lease anchor ref name for a mount (DESIGN.md section 7.1).
pub fn lease_anchor_ref(mount_id: &str) -> String {
  format!("{RESERVED_REF_PREFIX}mounts/{mount_id}")
}

#[cfg(test)]
mod tests {
  use super::*;

  const SHA1: HashAlgorithm = HashAlgorithm::Sha1;

  #[test]
  fn accepts_the_four_supported_shapes() {
    let hex = "3a".repeat(20);
    assert!(matches!(
      RevisionSelector::parse(&hex, SHA1).unwrap(),
      RevisionSelector::FullOid(_)
    ));
    assert!(matches!(
      RevisionSelector::parse(&format!("sha1:{hex}"), SHA1).unwrap(),
      RevisionSelector::FullOid(_)
    ));
    assert!(matches!(
      RevisionSelector::parse("3a3a3a3a", SHA1).unwrap(),
      RevisionSelector::Abbrev(_)
    ));
    assert!(matches!(
      RevisionSelector::parse("refs/heads/main", SHA1).unwrap(),
      RevisionSelector::FullRef(_)
    ));
    assert!(matches!(
      RevisionSelector::parse("main", SHA1).unwrap(),
      RevisionSelector::ShortName(_)
    ));
    assert!(matches!(
      RevisionSelector::parse("HEAD", SHA1).unwrap(),
      RevisionSelector::ShortName(_)
    ));
  }

  #[test]
  fn rejects_revparse_expressions() {
    // Every one of these resolves to something under `revparse_single`. The
    // `^{tree}` case is the dangerous one: it yields a tree OID where the whole
    // system expects a commit.
    for bad in [
      "HEAD~1",
      "main^",
      "main^{tree}",
      "main^{commit}",
      "HEAD@{2}",
      "@{-1}",
      "v1..v2",
      "v1...v2",
      ":/fix the bug",
      "main:src/lib.rs",
      "@",
    ] {
      let err = RevisionSelector::parse(bad, SHA1).unwrap_err();
      assert_eq!(
        err.code,
        ErrorCode::InvalidArgument,
        "should reject {bad:?}, got {err}"
      );
    }
  }

  #[test]
  fn rejects_the_reserved_namespace_in_both_spellings() {
    // The short-name spelling is the one that is easy to miss: Git resolves
    // `gfs/mounts/x` by trying `refs/gfs/mounts/x`, which is a live lease
    // anchor.
    for bad in [
      "refs/gfs/mounts/m-1",
      "gfs/mounts/m-1",
      "refs/gfs/anything",
      "gfs/",
    ] {
      let err = RevisionSelector::parse(bad, SHA1).unwrap_err();
      assert_eq!(
        err.code,
        ErrorCode::ReservedNamespace,
        "should reject {bad:?} as reserved, got {err}"
      );
    }
  }

  #[test]
  fn rejects_option_like_and_malformed_names() {
    for bad in [
      "--upload-pack=evil",
      "-x",
      "refs/heads/../../etc",
      "refs/heads/main.lock",
      "refs/heads/.hidden",
      "refs/heads/a b",
      "refs/heads/a*",
      "refs/heads/a\\b",
      "refs/heads/a\nb",
      "refs/",
      "/refs/heads/main",
      "refs/heads/main/",
    ] {
      assert!(
        RevisionSelector::parse(bad, SHA1).is_err(),
        "should reject {bad:?}"
      );
    }
  }

  #[test]
  fn rejects_worktree_pseudo_refs() {
    // These name state a bare mirror does not meaningfully have, and whatever
    // a maintenance operation left in them is not a revision a caller chose.
    for bad in ["ORIG_HEAD", "FETCH_HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
      assert!(
        RevisionSelector::parse(bad, SHA1).is_err(),
        "should reject {bad:?}"
      );
    }
  }

  #[test]
  fn abbreviation_length_is_bounded_below() {
    // Too short to be an abbreviation; treated as a name, not silently resolved
    // against a huge candidate set.
    assert!(matches!(
      RevisionSelector::parse("3a3", SHA1).unwrap(),
      RevisionSelector::ShortName(_)
    ));
    assert!(matches!(
      RevisionSelector::parse(&"3a".repeat(10), SHA1).unwrap(),
      RevisionSelector::Abbrev(_)
    ));
  }

  #[test]
  fn rejects_an_oid_from_the_wrong_algorithm() {
    let sha256 = format!("sha256:{}", "3a".repeat(32));
    let err = RevisionSelector::parse(&sha256, SHA1).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    // And an unqualified 64-character digest is not silently truncated to 40.
    assert!(RevisionSelector::parse(&"3a".repeat(32), SHA1).is_err());
  }

  #[test]
  fn lease_anchor_is_inside_the_reserved_namespace() {
    let anchor = lease_anchor_ref("m-abc123");
    assert!(is_reserved_ref(&anchor));
    // And the anchor it produces cannot be named back as a revision.
    assert_eq!(
      RevisionSelector::parse(&anchor, SHA1).unwrap_err().code,
      ErrorCode::ReservedNamespace
    );
  }
}
