//! `.gitattributes` resolution for the `filter` attribute.
//!
//! ADR 0012 needs exactly one question answered at a revision: does this path
//! carry `filter=lfs`? Answering it takes real attribute-stack semantics —
//! per-directory files, deeper files overriding shallower ones, last matching
//! line winning within a file — but only for the one attribute, so this module
//! deliberately parses and stores nothing else. Macros (`[attr]`), quoted
//! patterns, and the global/info files are out of scope: a bare mirror has no
//! working tree or `info/attributes`, and git-lfs's own `.gitattributes` lines
//! (`*.bin filter=lfs diff=lfs merge=lfs -text`) use none of those features.
//!
//! Pattern semantics follow `gitattributes(5)`: the `.gitignore` grammar, with
//! three exceptions — negative patterns are forbidden (the line is ignored, as
//! Git does), directory patterns (trailing `/`) never match a file, and a
//! slash-free pattern matches the basename only, not every path component.

use gfs_types::glob::Glob;

/// The resolved state of the `filter` attribute for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterState {
  /// `filter` — set, with no value. Not an LFS assignment.
  Set,
  /// `-filter` — explicitly unset.
  Unset,
  /// `!filter` — forced back to unspecified, overriding shallower files.
  Unspecified,
  /// `filter=value`.
  Value(String),
}

/// One parsed line that assigns `filter` something.
#[derive(Debug)]
struct Rule {
  pattern: Glob,
  /// True when the pattern contains no `/`, so it matches the basename.
  basename_only: bool,
  state: FilterState,
}

/// A parsed `.gitattributes` blob, keeping only the lines that touch `filter`.
#[derive(Debug, Default)]
pub struct AttributeFile {
  rules: Vec<Rule>,
}

impl AttributeFile {
  pub fn parse(content: &[u8]) -> AttributeFile {
    let mut rules = Vec::new();
    for line in content.split(|b| *b == b'\n') {
      let line = trim_ascii(line);
      if line.is_empty() || line[0] == b'#' {
        continue;
      }
      // Negative patterns are forbidden in .gitattributes; Git ignores the line.
      if line[0] == b'!' {
        continue;
      }
      let mut fields = line.split(|b| b.is_ascii_whitespace()).filter(|f| !f.is_empty());
      let Some(pattern) = fields.next() else {
        continue;
      };
      // A directory pattern can never match a file, and attributes only apply
      // to files, so the line is dead.
      if pattern.ends_with(b"/") {
        continue;
      }
      let mut state = None;
      for attr in fields {
        state = match attr {
          b"filter" => Some(FilterState::Set),
          b"-filter" => Some(FilterState::Unset),
          b"!filter" => Some(FilterState::Unspecified),
          _ => match attr.strip_prefix(b"filter=") {
            Some(v) => Some(FilterState::Value(
              String::from_utf8_lossy(v).into_owned(),
            )),
            None => state,
          },
        };
      }
      let Some(state) = state else {
        continue;
      };
      let glob = Glob::from_bytes(pattern);
      rules.push(Rule {
        basename_only: !pattern.contains(&b'/'),
        pattern: glob,
        state,
      });
    }
    AttributeFile { rules }
  }

  pub fn is_empty(&self) -> bool {
    self.rules.is_empty()
  }

  /// The `filter` state this file assigns to `rel_path` (relative to the
  /// directory holding the file), or `None` when no line matches.
  ///
  /// The last matching line wins, per `gitattributes(5)`.
  fn filter_for(&self, rel_path: &[u8]) -> Option<&FilterState> {
    let basename = rel_path
      .rsplit(|b| *b == b'/')
      .next()
      .unwrap_or(rel_path);
    self
      .rules
      .iter()
      .rev()
      .find(|r| {
        if r.basename_only {
          r.pattern.matches(basename)
        } else {
          r.pattern.matches(rel_path)
        }
      })
      .map(|r| &r.state)
  }
}

/// Resolve the `filter` attribute for `path` against an attribute stack.
///
/// `stack` is ordered root-first: element `i` is the `.gitattributes` of the
/// directory made of the first `i` components of `path`. Deeper files take
/// precedence, so resolution walks the stack backwards and stops at the first
/// file with a matching line — including a `!filter` line, which resolves to
/// unspecified rather than falling through to a shallower file.
pub fn resolve_filter<'a>(
  stack: &'a [std::sync::Arc<AttributeFile>],
  path: &[u8],
) -> Option<&'a FilterState> {
  for (depth, file) in stack.iter().enumerate().rev() {
    let rel = strip_components(path, depth);
    if let Some(state) = file.filter_for(rel) {
      return Some(state);
    }
  }
  None
}

/// Whether the resolved state means "this path is LFS-filtered".
pub fn is_lfs(state: Option<&FilterState>) -> bool {
  matches!(state, Some(FilterState::Value(v)) if v == "lfs")
}

/// Drop the first `n` slash-separated components of `path`.
fn strip_components(path: &[u8], n: usize) -> &[u8] {
  let mut rest = path;
  for _ in 0..n {
    match rest.iter().position(|b| *b == b'/') {
      Some(i) => rest = &rest[i + 1..],
      None => return b"",
    }
  }
  rest
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
  while let Some((first, rest)) = b.split_first() {
    if first.is_ascii_whitespace() {
      b = rest;
    } else {
      break;
    }
  }
  while let Some((last, rest)) = b.split_last() {
    if last.is_ascii_whitespace() {
      b = rest;
    } else {
      break;
    }
  }
  b
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  fn file(content: &str) -> Arc<AttributeFile> {
    Arc::new(AttributeFile::parse(content.as_bytes()))
  }

  #[test]
  fn the_git_lfs_line_shape_resolves_to_lfs() {
    let stack = vec![file("*.psd filter=lfs diff=lfs merge=lfs -text\n")];
    assert!(is_lfs(resolve_filter(&stack, b"art/hero.psd")));
    assert!(!is_lfs(resolve_filter(&stack, b"art/hero.png")));
  }

  #[test]
  fn later_lines_override_earlier_ones_within_a_file() {
    let stack = vec![file("*.bin filter=lfs\nlegacy/*.bin -filter\n")];
    assert!(is_lfs(resolve_filter(&stack, b"data.bin")));
    assert_eq!(
      resolve_filter(&stack, b"legacy/old.bin"),
      Some(&FilterState::Unset)
    );
  }

  #[test]
  fn a_deeper_file_overrides_a_shallower_one() {
    // Root says *.bin is LFS; vendored/ opts out; !filter forces unspecified.
    let stack = vec![
      file("*.bin filter=lfs\n"),
      file("*.bin !filter\n"), // .gitattributes inside `vendored/`
    ];
    assert_eq!(
      resolve_filter(&stack, b"vendored/blob.bin"),
      Some(&FilterState::Unspecified)
    );
    assert!(!is_lfs(resolve_filter(&stack, b"vendored/blob.bin")));
  }

  #[test]
  fn slash_patterns_anchor_to_the_attributing_directory() {
    let stack = vec![
      Arc::new(AttributeFile::default()),
      // .gitattributes inside `assets/`: anchored to assets/, so it must match
      // the path *relative to assets/*, not the full path.
      file("models/*.onnx filter=lfs\n"),
    ];
    assert!(is_lfs(resolve_filter(&stack, b"assets/models/net.onnx")));
    assert!(!is_lfs(resolve_filter(&stack, b"assets/other/net.onnx")));
  }

  #[test]
  fn a_slash_free_pattern_matches_the_basename_not_a_directory_component() {
    // `*.psd` must not match a file that merely lives under a directory named
    // like a psd — the gitignore any-component rule does not apply here.
    let stack = vec![file("*.psd filter=lfs\n")];
    assert!(!is_lfs(resolve_filter(&stack, b"weird.psd/readme.txt")));
  }

  #[test]
  fn comments_negations_and_directory_patterns_are_ignored() {
    let f = AttributeFile::parse(b"# comment\n!x.bin filter=lfs\nbuild/ filter=lfs\n");
    assert!(f.is_empty());
  }
}
