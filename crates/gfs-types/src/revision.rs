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
//!
//! # Ancestry is an extension of the closed grammar, not a hole in it
//!
//! [`RevisionExpression`] accepts `HEAD~1`, `main^`, `main~3`, `abc1234^2` --
//! and nothing else new. The suffix is parsed *here*, into an explicit list of
//! [`AncestryStep`]s that the repository layer applies with `commit.parent(n)`,
//! so no operator ever reaches libgit2's parser. That is the whole difference
//! between this and reopening `revparse`: `main^{tree}` is still rejected,
//! because `^` may only be followed by digits or by nothing.
//!
//! The reason to have it at all is that reviewing a chain of commits without it
//! means reading full 40-character IDs out of `gfs log --format=%P` and
//! threading them by hand, which is what the 2026-07-29 agent report was doing.

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

/// One ancestry hop, as `git rev-parse` spells it.
///
/// Two operators, and the difference is the one people get wrong: `^n` picks
/// *which* parent of one commit, `~n` walks n commits along first parents. On a
/// non-merge commit they coincide, which is why `HEAD^` and `HEAD~` usually look
/// interchangeable and stop being so at the first merge.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AncestryStep {
  /// `^n`. `^` alone means `^1`. `^0` is the commit itself, as Git defines it.
  Parent(u32),
  /// `~n`. `~` alone means `~1`. `~0` is the commit itself.
  Ancestor(u32),
}

/// A selector plus an ancestry walk: `HEAD~1`, `main^`, `abc1234^2~3`.
///
/// This is the type every *read* path parses, and [`RevisionSelector`] is what
/// it resolves through. Keeping them separate is deliberate: the selector is the
/// thing that names an object in the repository, and the steps are applied
/// afterwards by walking parent pointers. Nothing here is handed to `revparse`.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct RevisionExpression {
  pub base: RevisionSelector,
  /// Applied left to right, in the order they were written.
  pub steps: Vec<AncestryStep>,
}

impl RevisionExpression {
  /// Parse `<selector>` optionally followed by a chain of `~n` and `^n`.
  ///
  /// The split is unambiguous because Git already forbids `~` and `^` in a ref
  /// name, so the first one that appears can only be an operator.
  pub fn parse(input: &str, algorithm: HashAlgorithm) -> Result<Self, GfsError> {
    if input.len() > limits::MAX_REVISION_SELECTOR_BYTES {
      return Err(GfsError::invalid(format!(
        "revision expression exceeds {} bytes",
        limits::MAX_REVISION_SELECTOR_BYTES
      )));
    }
    let split = input.find(['~', '^']).unwrap_or(input.len());
    let (base, suffix) = input.split_at(split);
    if base.is_empty() {
      return Err(GfsError::invalid(format!(
        "revision expression {input:?} has no revision before {suffix:?}"
      )));
    }
    Ok(RevisionExpression {
      base: RevisionSelector::parse(base, algorithm)?,
      steps: parse_ancestry(input, suffix)?,
    })
  }

  /// True when this is a bare selector, so a caller that only understands
  /// selectors can use it unchanged.
  pub fn is_bare(&self) -> bool {
    self.steps.is_empty()
  }
}

/// The operator chain. `full` is only used for diagnostics, so an error names
/// what the caller typed rather than the fragment being parsed.
fn parse_ancestry(full: &str, mut suffix: &str) -> Result<Vec<AncestryStep>, GfsError> {
  let mut steps = Vec::new();
  let mut distance: u64 = 0;
  while !suffix.is_empty() {
    let (op, rest) = suffix.split_at(1);
    // The count is the digits that follow, and an absent one is 1. `^` may be
    // followed by digits or by nothing -- never by `{`, which is what keeps
    // `main^{tree}` rejected and a tree OID out of a commit-shaped field.
    let digit_bytes = rest.bytes().take_while(u8::is_ascii_digit).count();
    // Safe to split here without an index: ASCII digits are one byte each, so
    // the boundary is a character boundary by construction.
    let (digits, rest) = rest.split_at(digit_bytes);
    if !rest.is_empty() && !rest.starts_with(['~', '^']) {
      return Err(unsupported_expression(full));
    }
    let n: u32 = if digits.is_empty() {
      1
    } else {
      digits
        .parse()
        .map_err(|_| GfsError::invalid(format!("{digits:?} is not a usable ancestry count")))?
    };
    steps.push(match op {
      "^" => AncestryStep::Parent(n),
      "~" => AncestryStep::Ancestor(n),
      // `find(['~', '^'])` chose the split, so nothing else can be here.
      _ => unreachable!("ancestry operator"),
    });
    distance += u64::from(n);
    suffix = rest;
  }
  if distance > limits::MAX_ANCESTRY_DISTANCE as u64 {
    return Err(GfsError::new(
      ErrorCode::ResourceLimit,
      format!(
        "an ancestry walk may cover at most {} commits",
        limits::MAX_ANCESTRY_DISTANCE
      ),
    ));
  }
  Ok(steps)
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

/// Whether a user-supplied branch name can safely become a ref.
///
/// `gfs switch` and `gfs clone` take a branch name from a command line and turn
/// it into `refs/heads/<name>` or a work ref, so this is a trust boundary rather
/// than a tidiness check. Git's own rules, kept to the ones that matter here:
///
/// * the characters Git forbids outright, because `git check-ref-format` would
///   reject the name and the failure would surface as an opaque subprocess error
///   several layers down;
/// * `..` and `@{`, because both are *revision expression* syntax — a branch
///   named `a..b` turns a later selector into a range;
/// * a leading `-`, because the name reaches a `git` command line and would be
///   read as a flag.
///
/// Deliberately not a check for the reserved namespace: this validates a *branch*
/// name, and the caller decides which namespace it lands in.
pub fn is_valid_branch_name(name: &str) -> bool {
  if name.is_empty() || name.len() > 255 {
    return false;
  }
  if name.starts_with('-') || name.starts_with('/') || name.ends_with('/') {
    return false;
  }
  if name.ends_with('.') || name == "@" {
    return false;
  }
  if name.contains("..") || name.contains("@{") || name.contains("//") {
    return false;
  }
  if name
    .chars()
    .any(|c| c.is_ascii_control() || " ~^:?*[\\".contains(c))
  {
    return false;
  }
  name
    .split('/')
    .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// The ref an unpushed branch lives on.
///
/// Inside the reserved namespace, and that placement is the whole point: the
/// mirror fetch maps `+refs/heads/*:refs/heads/*` and runs `--prune`, so a
/// branch written under `refs/heads/` that upstream does not have is deleted by
/// the next sync -- silently, as routine maintenance, taking the reachability of
/// every commit on it. Nothing fetches into `refs/gfs/`, so work survives there
/// until it is pushed.
///
/// The subject is folded to ref-safe characters rather than validated, because
/// a subject id comes from an identity provider and may legitimately contain a
/// `/`, a space, or an `@` -- none of which can appear in this position without
/// changing the ref's shape.
pub fn work_ref(subject: &str, branch: &str) -> String {
  let mut folded = String::with_capacity(subject.len());
  let mut last_was_sep = false;
  for c in subject.chars() {
    if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
      folded.push(c);
      last_was_sep = false;
    } else if !last_was_sep {
      folded.push('_');
      last_was_sep = true;
    }
  }
  let folded = folded.trim_matches('_').trim_matches('.');
  let owner = if folded.is_empty() {
    "anonymous"
  } else {
    folded
  };
  format!("{RESERVED_REF_PREFIX}work/{owner}/{branch}")
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
    //
    // `HEAD~1` and `main^` are *not* in this list any more -- they are parsed by
    // `RevisionExpression` and applied as explicit parent hops, never by
    // `revparse`. A bare selector still rejects them, which is what keeps the
    // ancestry walk in one place.
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
  fn expressions_accept_ancestry_and_nothing_else() {
    let cases: [(&str, &[AncestryStep]); 6] = [
      ("main", &[]),
      ("HEAD~1", &[AncestryStep::Ancestor(1)]),
      ("main^", &[AncestryStep::Parent(1)]),
      ("main~3", &[AncestryStep::Ancestor(3)]),
      ("main^2", &[AncestryStep::Parent(2)]),
      (
        "main^2~3^",
        &[
          AncestryStep::Parent(2),
          AncestryStep::Ancestor(3),
          AncestryStep::Parent(1),
        ],
      ),
    ];
    for (input, steps) in cases {
      let parsed = RevisionExpression::parse(input, SHA1).expect(input);
      assert_eq!(parsed.steps, steps, "steps for {input:?}");
      assert_eq!(
        parsed.base,
        RevisionSelector::parse(input.split(['~', '^']).next().unwrap(), SHA1).unwrap()
      );
    }
    // A qualified object id keeps working as a base, which is what the daemon
    // sends once it has substituted its pinned commit for `HEAD`.
    let hex = format!("sha1:{}~2", "3a".repeat(20));
    assert_eq!(
      RevisionExpression::parse(&hex, SHA1).unwrap().steps,
      vec![AncestryStep::Ancestor(2)]
    );
  }

  #[test]
  fn expressions_still_reject_everything_revparse_adds() {
    // The point of the whole module. `^` may be followed by digits or by
    // nothing; `^{tree}` is what would otherwise hand back a tree OID.
    for bad in [
      "main^{tree}",
      "main^{commit}",
      "HEAD@{2}",
      "@{-1}",
      "v1..v2",
      ":/fix the bug",
      "main:src/lib.rs",
      "~1",
      "^",
      "main~-1",
      "main~1x",
      "main^{}",
    ] {
      assert!(
        RevisionExpression::parse(bad, SHA1).is_err(),
        "should reject {bad:?}"
      );
    }
    // And an unbounded walk is a resource limit, not an invalid argument: the
    // expression is well formed and the answer is that it is too expensive.
    let err = RevisionExpression::parse("main~9000000", SHA1).unwrap_err();
    assert_eq!(err.code, ErrorCode::ResourceLimit);
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
