//! Redaction helpers for logs, traces, and audit records.
//!
//! ADR 0006 fixes the rule: audit records carry repository, commit, subject, and
//! job -- never file content, tokens, or unvalidated paths. DESIGN.md section 10
//! item 9 says the same for audit generally.
//!
//! "Unvalidated paths" is the subtle one, and it is why these are functions
//! rather than a convention. A caller-supplied path may contain a newline, an
//! ANSI escape, or a terminal control sequence; writing it raw into a log line
//! is log injection, and doing it in a structured log is worse because the
//! injected text looks like a field. So a path is logged escaped and bounded, or
//! not at all.

use crate::oid::ObjectId;
use crate::path::BytePath;

/// Longest path prefix included in a log line. Long enough to identify a file in
/// a monorepo, short enough that one log line cannot be used to exfiltrate a
/// path list.
const PATH_LOG_BUDGET: usize = 128;

/// A path, escaped and truncated, safe for a log line or an audit field.
///
/// Truncation is marked so a reader can tell a truncated path from a short one;
/// silently cutting it would make two different paths log identically.
pub fn path(p: &BytePath) -> String {
  let escaped = p.escaped();
  if escaped.len() <= PATH_LOG_BUDGET {
    return escaped;
  }
  // Truncate by characters rather than by byte index: `escaped` is valid UTF-8
  // by construction, but its byte count and character count differ once a
  // non-UTF-8 path has been escaped, and slicing on a byte offset inside a
  // multi-byte character panics.
  let mut head = String::with_capacity(PATH_LOG_BUDGET + 4);
  for c in escaped.chars() {
    if head.len() + c.len_utf8() > PATH_LOG_BUDGET {
      break;
    }
    head.push(c);
  }
  format!("{head}...[{} bytes]", p.len())
}

/// A credential's fingerprint. Never the credential.
///
/// Enough to correlate two log lines as referring to the same token, and not
/// enough to use it. Truncated to 8 hex characters because correlation is the
/// only purpose and a full digest of a low-entropy token is brute-forceable.
pub fn token_fingerprint(token: &str) -> String {
  // FNV-1a. Not a security primitive and not used as one: the secrecy comes
  // from truncation and from never logging the token, so a keyed MAC here would
  // imply a guarantee this does not make.
  let mut h: u64 = 0xcbf2_9ce4_8422_2325;
  for b in token.as_bytes() {
    h ^= *b as u64;
    h = h.wrapping_mul(0x100_0000_01b3);
  }
  format!("{:08x}", h as u32)
}

/// An object ID, abbreviated for readability.
///
/// Object IDs are not secret -- ADR 0002 records that a repository reader can
/// reach any object in it -- so this is a legibility helper, not a redaction. It
/// lives here so log call sites have one obvious place to look.
pub fn oid(o: &ObjectId) -> String {
  let abbrev: String = o.to_hex().chars().take(12).collect();
  format!("{}:{abbrev}", o.algorithm().name())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::oid::HashAlgorithm;

  #[test]
  fn a_path_with_control_characters_cannot_inject_into_a_log_line() {
    // The attack: a tracked file named so that its path forges a second log
    // line. After escaping there is no raw newline left to end the record.
    let p = BytePath::new(b"src/a\n2026-07-26 ERROR forged\n".to_vec());
    let logged = path(&p);
    assert!(!logged.contains('\n'), "escaped form still has a newline");
    assert!(logged.contains("\\n"));
  }

  #[test]
  fn a_long_path_is_truncated_and_marked() {
    let p = BytePath::new(vec![b'a'; 500]);
    let logged = path(&p);
    assert!(logged.len() < 200);
    assert!(logged.ends_with("...[500 bytes]"));
  }

  #[test]
  fn truncation_does_not_split_an_escaped_multibyte_character() {
    // A path of multi-byte characters whose escaped length crosses the budget.
    // Slicing on a byte index here would panic.
    let p = BytePath::new("é".repeat(200).into_bytes());
    let logged = path(&p);
    assert!(logged.ends_with(&format!("...[{} bytes]", 400)));
  }

  #[test]
  fn a_token_never_appears_in_its_fingerprint() {
    let token = "xvfs-secret-token-value";
    let fp = token_fingerprint(token);
    assert!(!fp.contains("secret"));
    assert_eq!(fp.len(), 8);
    // Same token correlates; different tokens do not.
    assert_eq!(fp, token_fingerprint(token));
    assert_ne!(fp, token_fingerprint("xvfs-secret-token-valuf"));
  }

  #[test]
  fn oid_is_abbreviated_but_keeps_its_algorithm() {
    let o = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
    assert_eq!(oid(&o), "sha1:abababababab");
  }
}
