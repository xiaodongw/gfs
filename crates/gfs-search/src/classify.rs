//! The searchable-corpus policy.
//!
//! ADR 0004 fixed two of these rules with measurements and the rest follow from
//! PLAN.md M4.1:
//!
//! * **binary** is a NUL byte in the first 8 KiB, which is ripgrep's rule. It has
//!   to be ripgrep's rule, because `gfs rg` claims to substitute for `rg` and a
//!   different corpus boundary would make that claim false on exactly the files
//!   where it matters.
//! * **oversized** is 8 MiB (`gfs_types::limits::MAX_SEARCHABLE_BLOB_BYTES`).
//! * **invalid UTF-8** is detected and recorded but **not excluded by default**,
//!   again because `rg` searches such files happily. Recording it without acting
//!   on it is the point: the coverage contract can name the reason if an operator
//!   ever turns the exclusion on.
//! * **generated** and **vendored** are path classifications, not content ones,
//!   and are off by default.
//!
//! ADR 0004 measured the excluded share at 0.02 % of unique blobs for linux and
//! 1.40 % for vscode. That is what makes `--require-exhaustive` usable and stops
//! the default coverage warning becoming noise an agent learns to ignore.
//!
//! # Why the exclusion is a property of the policy object
//!
//! PLAN.md M4.1: "any exclusion must be declared in coverage metadata". The
//! cheapest way to guarantee the declaration matches the behaviour is to have one
//! object do both — [`CorpusPolicy::excludes`] is what the indexer consults and
//! what the completion message reports.

use crate::glob::Glob;

/// What a blob's *bytes* are.
///
/// Stored once per blob key. Never mixed with a path classification: the same
/// bytes can live at a vendored path in one snapshot and a hand-written path in
/// another, and folding the two would let one snapshot's layout exclude
/// another's file.
#[derive(
  Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContentClass {
  /// Searchable text. May still contain invalid UTF-8; see [`ContentClass::InvalidUtf8`].
  Text,
  /// A NUL byte in the probe window.
  Binary,
  /// Larger than the policy's cap. Detected from the size alone, so an oversized
  /// blob is never inflated to be excluded.
  Oversized,
  /// Searchable bytes that are not valid UTF-8. A distinct class rather than a
  /// flag on `Text` so the store can answer "how much of the corpus is this?"
  /// without a second column, and so a policy that excludes it has a reason to
  /// name.
  InvalidUtf8,
}

impl ContentClass {
  pub fn as_str(self) -> &'static str {
    match self {
      ContentClass::Text => "TEXT",
      ContentClass::Binary => "BINARY",
      ContentClass::Oversized => "OVERSIZED",
      ContentClass::InvalidUtf8 => "INVALID_UTF8",
    }
  }

  pub fn from_wire_name(s: &str) -> Option<Self> {
    match s {
      "TEXT" => Some(ContentClass::Text),
      "BINARY" => Some(ContentClass::Binary),
      "OVERSIZED" => Some(ContentClass::Oversized),
      "INVALID_UTF8" => Some(ContentClass::InvalidUtf8),
      _ => None,
    }
  }

  /// Whether trigrams should be extracted. `Binary` and `Oversized` produce no
  /// postings; invalid UTF-8 does, because byte trigrams do not care.
  pub fn is_indexable(self) -> bool {
    matches!(self, ContentClass::Text | ContentClass::InvalidUtf8)
  }
}

/// What a *path* is, independent of its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PathClass {
  Ordinary,
  /// Machine-produced: minified bundles, protobuf output, lockfiles.
  Generated,
  /// Third-party code checked into the tree.
  Vendored,
}

/// Why a path is not in the searchable corpus.
///
/// One vocabulary over both classification kinds, because a coverage report has
/// to list them side by side and an agent reading it should not have to know
/// which half of the classifier produced which reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum ExclusionReason {
  Binary,
  Oversized,
  InvalidUtf8,
  Generated,
  Vendored,
  /// The index has no answer for this path: its blob was interned but never
  /// classified, was classified but has no posting lists yet, or vanished from
  /// the object database under a running query.
  ///
  /// Not a policy exclusion — it is an index *state*, and it never appears in
  /// [`CorpusPolicy::declared_exclusions`]. It shares this vocabulary because a
  /// coverage report has to list it beside the policy reasons: from the caller's
  /// side, "the index cannot answer for this file" and "policy excluded this
  /// file" have the same consequence, and both are the opposite of "no match
  /// here".
  IndexGap,
}

impl ExclusionReason {
  /// The stable wire name. Appears in completion messages and in `--json`, so it
  /// is part of the agent-facing contract and not a debug string.
  pub fn as_str(self) -> &'static str {
    match self {
      ExclusionReason::Binary => "binary",
      ExclusionReason::Oversized => "oversized",
      ExclusionReason::InvalidUtf8 => "invalid_utf8",
      ExclusionReason::Generated => "generated",
      ExclusionReason::Vendored => "vendored",
      ExclusionReason::IndexGap => "index_gap",
    }
  }
}

impl std::fmt::Display for ExclusionReason {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// The declared corpus policy.
///
/// Both the thing the indexer enforces and the thing the completion message
/// reports. Cloneable and cheap to pass around; the glob lists are compiled once.
#[derive(Clone, Debug)]
pub struct CorpusPolicy {
  /// Blobs larger than this are `Oversized`. ADR 0004: 8 MiB.
  pub max_blob_bytes: u64,
  /// How much of a blob is examined for a NUL byte. ADR 0004: 8 KiB, ripgrep's
  /// window. A NUL past it is not seen, and that is deliberate — matching `rg`
  /// matters more than catching every binary.
  pub binary_probe_bytes: usize,
  pub exclude_binary: bool,
  pub exclude_oversized: bool,
  /// Off. `rg` searches invalid UTF-8, so excluding it would break the claim
  /// that GFS search substitutes for `rg`.
  pub exclude_invalid_utf8: bool,
  /// Off. PLAN.md M4.1: a classification, not a default exclusion.
  pub exclude_generated: bool,
  /// Off, for the same reason.
  pub exclude_vendored: bool,
  generated: Vec<Glob>,
  vendored: Vec<Glob>,
}

/// Path patterns that mark machine-produced content.
///
/// Deliberately short. A long heuristic list is how a search tool starts
/// silently hiding a file someone hand-wrote; these are the shapes that are
/// unambiguous, and the classification is not an exclusion by default anyway.
const GENERATED_PATTERNS: &[&str] = &[
  "*.min.js",
  "*.min.css",
  "*.map",
  "*_pb2.py",
  "*_pb.go",
  "*.pb.go",
  "*.pb.cc",
  "*.pb.h",
  "Cargo.lock",
  "package-lock.json",
  "yarn.lock",
  "pnpm-lock.yaml",
  "go.sum",
];

/// Path patterns that mark third-party code checked into the tree.
const VENDORED_PATTERNS: &[&str] = &[
  "vendor/",
  "vendored/",
  "third_party/",
  "thirdparty/",
  "node_modules/",
  "external/",
];

impl Default for CorpusPolicy {
  fn default() -> Self {
    CorpusPolicy {
      max_blob_bytes: gfs_types::limits::MAX_SEARCHABLE_BLOB_BYTES,
      binary_probe_bytes: 8192,
      exclude_binary: true,
      exclude_oversized: true,
      exclude_invalid_utf8: false,
      exclude_generated: false,
      exclude_vendored: false,
      generated: GENERATED_PATTERNS.iter().map(|p| Glob::new(p)).collect(),
      vendored: VENDORED_PATTERNS.iter().map(|p| Glob::new(p)).collect(),
    }
  }
}

impl CorpusPolicy {
  /// Whether a content class is excluded by this policy.
  pub fn excludes_content(&self, class: ContentClass) -> Option<ExclusionReason> {
    match class {
      ContentClass::Text => None,
      ContentClass::Binary => self.exclude_binary.then_some(ExclusionReason::Binary),
      ContentClass::Oversized => self.exclude_oversized.then_some(ExclusionReason::Oversized),
      ContentClass::InvalidUtf8 => self
        .exclude_invalid_utf8
        .then_some(ExclusionReason::InvalidUtf8),
    }
  }

  pub fn excludes_path(&self, class: PathClass) -> Option<ExclusionReason> {
    match class {
      PathClass::Ordinary => None,
      PathClass::Generated => self.exclude_generated.then_some(ExclusionReason::Generated),
      PathClass::Vendored => self.exclude_vendored.then_some(ExclusionReason::Vendored),
    }
  }

  /// The single verdict for a path with known content.
  ///
  /// Content first: a binary file inside `vendor/` is reported as binary, which
  /// is the reason that would still apply if the vendored exclusion were turned
  /// off.
  pub fn excludes(&self, content: ContentClass, path: PathClass) -> Option<ExclusionReason> {
    self
      .excludes_content(content)
      .or_else(|| self.excludes_path(path))
  }

  /// Every exclusion this policy performs, for the coverage declaration.
  ///
  /// Reported even when a particular query excludes nothing, so an agent can
  /// tell "the policy excludes binaries and this scope has none" from "the
  /// policy excludes nothing".
  pub fn declared_exclusions(&self) -> Vec<ExclusionReason> {
    let mut out = Vec::new();
    if self.exclude_binary {
      out.push(ExclusionReason::Binary);
    }
    if self.exclude_oversized {
      out.push(ExclusionReason::Oversized);
    }
    if self.exclude_invalid_utf8 {
      out.push(ExclusionReason::InvalidUtf8);
    }
    if self.exclude_generated {
      out.push(ExclusionReason::Generated);
    }
    if self.exclude_vendored {
      out.push(ExclusionReason::Vendored);
    }
    out
  }
}

/// Classify a blob from its size alone, when that is enough.
///
/// Returns `Some(Oversized)` without reading anything. The indexer calls this
/// first so an oversized blob costs one `read_header` rather than an inflate,
/// which is what ADR 0004's cost model assumes.
pub fn classify_size(policy: &CorpusPolicy, size: u64) -> Option<ContentClass> {
  (size > policy.max_blob_bytes).then_some(ContentClass::Oversized)
}

/// Classify a blob from its bytes.
pub fn classify_content(policy: &CorpusPolicy, content: &[u8]) -> ContentClass {
  if content.len() as u64 > policy.max_blob_bytes {
    return ContentClass::Oversized;
  }
  let probe = &content[..content.len().min(policy.binary_probe_bytes)];
  if memchr::memchr(0, probe).is_some() {
    return ContentClass::Binary;
  }
  // Checked over the whole blob, not the probe. A file whose first 8 KiB are
  // ASCII and whose tail is Latin-1 is invalid UTF-8, and reporting it as clean
  // text would make the recorded classification a guess.
  if std::str::from_utf8(content).is_err() {
    return ContentClass::InvalidUtf8;
  }
  ContentClass::Text
}

/// Classify a path.
pub fn classify_path(policy: &CorpusPolicy, path: &[u8]) -> PathClass {
  if policy.vendored.iter().any(|g| g.matches(path)) {
    return PathClass::Vendored;
  }
  if policy.generated.iter().any(|g| g.matches(path)) {
    return PathClass::Generated;
  }
  PathClass::Ordinary
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classification_matches_the_corpus_adr_0004_measured() {
    let p = CorpusPolicy::default();
    assert_eq!(classify_content(&p, b"hello\n"), ContentClass::Text);
    assert_eq!(classify_content(&p, b"a\0b"), ContentClass::Binary);
    assert_eq!(
      classify_content(&p, &vec![b'x'; p.max_blob_bytes as usize + 1]),
      ContentClass::Oversized
    );
  }

  #[test]
  fn a_nul_past_the_probe_window_is_not_seen_because_ripgrep_does_not_see_it() {
    // Not an oversight. `gfs rg` substitutes for `rg`, and a wider window would
    // exclude files `rg` searches, which is a difference in the answer rather
    // than a difference in thoroughness.
    let p = CorpusPolicy::default();
    let mut late = vec![b'x'; 9000];
    late[8500] = 0;
    assert_eq!(classify_content(&p, &late), ContentClass::Text);
  }

  #[test]
  fn invalid_utf8_is_recorded_but_still_searched() {
    let p = CorpusPolicy::default();
    let class = classify_content(&p, b"caf\xe9 au lait\n");
    assert_eq!(class, ContentClass::InvalidUtf8);
    assert!(
      class.is_indexable(),
      "byte trigrams do not need valid UTF-8"
    );
    assert_eq!(
      p.excludes(class, PathClass::Ordinary),
      None,
      "rg searches these files, so excluding them would break the substitution claim"
    );
  }

  #[test]
  fn generated_and_vendored_are_classifications_not_default_exclusions() {
    // PLAN.md M4.1 states this directly, and it is the kind of default that
    // gets "helpfully" flipped during a performance pass.
    let p = CorpusPolicy::default();
    assert_eq!(
      classify_path(&p, b"web/node_modules/react/index.js"),
      PathClass::Vendored
    );
    assert_eq!(classify_path(&p, b"dist/app.min.js"), PathClass::Generated);
    assert_eq!(p.excludes(ContentClass::Text, PathClass::Vendored), None);
    assert_eq!(p.excludes(ContentClass::Text, PathClass::Generated), None);
  }

  #[test]
  fn a_policy_that_excludes_them_names_the_reason() {
    let p = CorpusPolicy {
      exclude_vendored: true,
      ..CorpusPolicy::default()
    };
    assert_eq!(
      p.excludes(ContentClass::Text, PathClass::Vendored),
      Some(ExclusionReason::Vendored)
    );
    assert!(p.declared_exclusions().contains(&ExclusionReason::Vendored));
  }

  #[test]
  fn content_exclusions_win_over_path_exclusions() {
    // A binary in `vendor/` is reported as binary: that is the reason that
    // survives turning the vendored exclusion off.
    let p = CorpusPolicy {
      exclude_vendored: true,
      ..CorpusPolicy::default()
    };
    assert_eq!(
      p.excludes(ContentClass::Binary, PathClass::Vendored),
      Some(ExclusionReason::Binary)
    );
  }

  #[test]
  fn an_oversized_blob_is_classified_without_being_read() {
    let p = CorpusPolicy::default();
    assert_eq!(
      classify_size(&p, p.max_blob_bytes + 1),
      Some(ContentClass::Oversized)
    );
    assert_eq!(classify_size(&p, 10), None);
  }
}
