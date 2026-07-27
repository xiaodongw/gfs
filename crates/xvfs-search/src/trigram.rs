//! Trigrams, and the literal analysis that decides which ones a match must have.
//!
//! # Byte trigrams
//!
//! Not character trigrams. Paths and contents are bytes, and a UTF-8-aware split
//! would make the index disagree with a byte-oriented matcher on exactly the
//! inputs that matter — a file with a Latin-1 byte in it, which `rg` searches
//! happily.
//!
//! # The part that has to be sound
//!
//! A trigram index proposes candidates; the matcher verifies them. Proposing too
//! many costs time. Proposing too *few* silently loses matches, and a search tool
//! that silently loses matches is worse than no search tool, because an agent
//! believes it.
//!
//! So [`required_literals`] answers one question and answers it conservatively:
//! *is there a set of byte strings such that every match must contain at least
//! one of them?* Alternation is the case that makes this subtle. The M0 spike
//! took the longest literal run of `foo|bar` — `foo` — and required its trigrams,
//! which drops every match of `bar`. This module returns **both** alternatives,
//! and returns nothing at all when it cannot prove the set is required.
//!
//! Nothing at all is a legitimate answer with a defined consequence: ADR 0004
//! makes a pattern with no usable literal `TRUNCATED` rather than `COMPLETE`,
//! because such a query is bounded by a scan budget rather than by the index.

use regex_syntax::hir::{Hir, HirKind};

/// The trigrams of a byte string, sorted and deduplicated.
pub fn trigrams(content: &[u8]) -> Vec<u32> {
  if content.len() < 3 {
    return Vec::new();
  }
  let mut out: Vec<u32> = content.windows(3).map(pack).collect();
  out.sort_unstable();
  out.dedup();
  out
}

/// Pack three bytes into the index's trigram key.
pub fn pack(window: &[u8]) -> u32 {
  ((window[0] as u32) << 16) | ((window[1] as u32) << 8) | window[2] as u32
}

/// The ASCII case variants of a trigram.
///
/// Case-insensitive search would otherwise have no usable literal at all, since
/// the index is byte-exact. Eight lists per trigram at worst is a bounded cost
/// and keeps `-i` — which agents use constantly — on the indexed path instead of
/// the scan path.
///
/// ASCII only. Unicode case folding is not a byte-local operation (`ß` folds to
/// `ss`, and Turkish dotless `ı` folds differently again), so a literal with a
/// non-ASCII byte in it is rejected for case-insensitive use by
/// [`required_literals`] rather than folded incorrectly here.
pub fn case_variants(trigram: u32) -> Vec<u32> {
  let bytes = [(trigram >> 16) as u8, (trigram >> 8) as u8, trigram as u8];
  let mut out = vec![0u32];
  for b in bytes {
    let choices: Vec<u8> = if b.is_ascii_alphabetic() {
      vec![b.to_ascii_lowercase(), b.to_ascii_uppercase()]
    } else {
      vec![b]
    };
    let mut next = Vec::with_capacity(out.len() * choices.len());
    for prefix in &out {
      for c in &choices {
        next.push((prefix << 8) | *c as u32);
      }
    }
    out = next;
  }
  out.sort_unstable();
  out.dedup();
  out
}

/// A set of byte strings such that **every** match contains at least one of
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiredLiterals(Vec<Vec<u8>>);

impl RequiredLiterals {
  pub fn alternatives(&self) -> &[Vec<u8>] {
    &self.0
  }
}

/// Shortest literal the index can bound a query with.
const MIN_LITERAL: usize = 3;

/// A hard cap on how many alternatives are worth taking to the index.
///
/// A pattern like `[a-z]{3}` expands combinatorially once a caller starts
/// enumerating classes. This module never enumerates classes, but an alternation
/// written out by hand can still be long, and past a few dozen lists the union
/// costs more than the scan it saves.
const MAX_ALTERNATIVES: usize = 32;

/// Analyse a pattern for literals every match must contain.
///
/// `literal` treats the pattern as a plain byte string rather than a regex.
/// `case_insensitive` rejects non-ASCII literals; see [`case_variants`].
///
/// `None` means the index cannot bound this query, which the caller must report
/// as an execution truncation rather than silently scanning.
pub fn required_literals(
  pattern: &str,
  literal: bool,
  case_insensitive: bool,
) -> Option<RequiredLiterals> {
  let alternatives = if literal {
    vec![pattern.as_bytes().to_vec()]
  } else {
    let hir = regex_syntax::ParserBuilder::new()
      .utf8(false)
      .build()
      .parse(pattern)
      .ok()?;
    required_of(&hir)?
  };
  finish(alternatives, case_insensitive)
}

fn finish(alternatives: Vec<Vec<u8>>, case_insensitive: bool) -> Option<RequiredLiterals> {
  if alternatives.is_empty() || alternatives.len() > MAX_ALTERNATIVES {
    return None;
  }
  // Every alternative must be usable. One short alternative makes the whole set
  // unusable, because a match could take that branch and carry no required
  // trigram at all.
  for alt in &alternatives {
    if alt.len() < MIN_LITERAL {
      return None;
    }
    if case_insensitive && alt.iter().any(|b| !b.is_ascii()) {
      return None;
    }
  }
  let mut out = alternatives;
  out.sort();
  out.dedup();
  Some(RequiredLiterals(out))
}

/// The required-literal set of an HIR node, or `None` when there is not one.
///
/// The recursion is the proof:
///
/// * a literal requires itself;
/// * a concatenation requires whatever any one of its parts requires, so the
///   most selective part is chosen;
/// * an alternation requires something only if **every** branch does, and then
///   the union of the branches' requirements;
/// * a repetition requires its inner expression only when it must occur at least
///   once;
/// * everything else — classes, look-arounds, empty — requires nothing.
fn required_of(hir: &Hir) -> Option<Vec<Vec<u8>>> {
  match hir.kind() {
    HirKind::Literal(lit) => Some(vec![lit.0.to_vec()]),
    HirKind::Capture(capture) => required_of(&capture.sub),
    HirKind::Concat(parts) => {
      // Adjacent literals are already merged by the parser, so each part is
      // considered on its own. The best part is the one whose *worst*
      // alternative is longest: a set is only as selective as its weakest
      // branch, since a match may take that branch.
      let mut best: Option<Vec<Vec<u8>>> = None;
      for part in parts {
        let Some(candidate) = required_of(part) else {
          continue;
        };
        let score = |set: &Vec<Vec<u8>>| set.iter().map(|a| a.len()).min().unwrap_or(0);
        if best.as_ref().is_none_or(|b| score(&candidate) > score(b)) {
          best = Some(candidate);
        }
      }
      best
    }
    HirKind::Alternation(branches) => {
      let mut union = Vec::new();
      for branch in branches {
        // One branch with no requirement means the alternation has none: a match
        // could take that branch.
        let alts = required_of(branch)?;
        union.extend(alts);
        if union.len() > MAX_ALTERNATIVES {
          return None;
        }
      }
      Some(union)
    }
    HirKind::Repetition(rep) => {
      if rep.min >= 1 {
        required_of(&rep.sub)
      } else {
        None
      }
    }
    HirKind::Empty | HirKind::Look(_) | HirKind::Class(_) => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn alts(pattern: &str) -> Option<Vec<Vec<u8>>> {
    required_literals(pattern, false, false).map(|r| r.0)
  }

  #[test]
  fn trigrams_are_byte_oriented_and_deduplicated() {
    assert!(trigrams(b"ab").is_empty());
    assert_eq!(trigrams(b"aaaa"), vec![0x61_61_61]);
    // A non-UTF-8 byte still produces a trigram rather than being skipped.
    assert_eq!(trigrams(b"\xff\xfe\xfd").len(), 1);
  }

  #[test]
  fn a_plain_literal_requires_itself() {
    assert_eq!(alts("authorize"), Some(vec![b"authorize".to_vec()]));
  }

  #[test]
  fn alternation_requires_every_branch_not_the_longest_one() {
    // The M0 spike took the longest run and would have dropped every `bar`
    // match. This is the case that makes a search tool quietly wrong.
    let got = alts("foo|barbaz").unwrap();
    assert!(got.contains(&b"foo".to_vec()));
    assert!(got.contains(&b"barbaz".to_vec()));
  }

  #[test]
  fn one_short_branch_makes_the_whole_alternation_unusable() {
    // A match could take the `ba` branch and carry none of `foo`'s trigrams.
    assert_eq!(alts("foo|ba"), None);
  }

  #[test]
  fn an_optional_suffix_does_not_become_required() {
    // `colors?` must contain `color`, not `colors`.
    assert_eq!(alts("colors?"), Some(vec![b"color".to_vec()]));
  }

  #[test]
  fn a_concatenation_picks_the_most_selective_part() {
    assert_eq!(
      alts(r"fn\s+authorize_"),
      Some(vec![b"authorize_".to_vec()]),
      "`fn` is two bytes and cannot bound anything"
    );
  }

  #[test]
  fn a_pattern_with_no_usable_literal_says_so() {
    assert_eq!(alts("a.b"), None);
    assert_eq!(alts(r"\w+"), None);
    assert_eq!(alts("^$"), None);
  }

  #[test]
  fn a_repetition_of_at_least_one_keeps_its_requirement() {
    assert_eq!(alts("(?:abc)+"), Some(vec![b"abc".to_vec()]));
    assert_eq!(alts("(?:abc)*"), None);
  }

  #[test]
  fn a_literal_query_is_taken_verbatim_including_regex_metacharacters() {
    // In literal mode `a.b` is three bytes to find, not a wildcard.
    assert_eq!(
      required_literals("a.b", true, false).map(|r| r.0),
      Some(vec![b"a.b".to_vec()])
    );
  }

  #[test]
  fn case_variants_cover_ascii_and_nothing_else() {
    let abc = pack(b"abc");
    assert_eq!(case_variants(abc).len(), 8);
    let mixed = pack(b"a1c");
    assert_eq!(case_variants(mixed).len(), 4);
    let digits = pack(b"123");
    assert_eq!(case_variants(digits), vec![digits]);
  }

  #[test]
  fn a_non_ascii_literal_is_not_used_for_a_case_insensitive_query() {
    // Byte-level case folding is wrong outside ASCII, so the honest answer is
    // that the index cannot bound this query.
    assert!(required_literals("café", false, false).is_some());
    assert_eq!(required_literals("café", false, true), None);
  }

  #[test]
  fn an_unparseable_pattern_yields_no_literals_rather_than_a_panic() {
    assert_eq!(alts("("), None);
  }
}
