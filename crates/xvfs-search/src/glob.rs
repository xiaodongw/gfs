//! A byte-oriented path glob.
//!
//! Written here rather than taken from `globset` for one reason: XVFS paths are
//! bytes and are not required to be UTF-8 (`xvfs_types::BytePath`), while every
//! general-purpose glob crate matches `str` or `Path`. Converting a path to
//! `str` to test it against a pattern would make a Linux kernel filename with a
//! Latin-1 byte in it unmatchable, or — worse — matchable only after a lossy
//! conversion that changes the bytes.
//!
//! # The grammar
//!
//! Git's `.gitignore` and ripgrep's `--glob` share it, so this does too:
//!
//! | Token | Matches |
//! | --- | --- |
//! | `?` | one byte that is not `/` |
//! | `*` | zero or more bytes, none of them `/` |
//! | `**` | zero or more path components, `/` included |
//! | `[abc]`, `[a-z]`, `[!a-z]` | one byte from the class |
//! | `\x` | the literal byte `x` |
//!
//! A pattern with no `/` in it (other than a trailing one) matches against the
//! **file name**, not the whole path, which is what makes `*.min.js` and
//! `node_modules/` behave the way every user of these tools expects.

/// A compiled glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
  tokens: Vec<Token>,
  /// True when the pattern has no interior `/`, so it applies to the file name.
  name_only: bool,
  /// True when the pattern ended in `/`, so it only matches directories — and,
  /// for ignore purposes, everything under them.
  directory_only: bool,
  source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
  Byte(u8),
  /// `?`
  AnyByte,
  /// `*`
  Star,
  /// `**`
  DoubleStar,
  Class {
    negated: bool,
    ranges: Vec<(u8, u8)>,
  },
}

impl Glob {
  /// Compile a pattern. Never fails: an unterminated class or a trailing
  /// backslash is treated as the literal bytes it is made of.
  ///
  /// Deliberately total rather than fallible. These patterns come from
  /// `.gitignore` files inside a repository under test, and a malformed line in
  /// one of them must not be able to fail a search — Git itself does not reject
  /// them either.
  pub fn new(pattern: &str) -> Glob {
    Glob::from_bytes(pattern.as_bytes())
  }

  pub fn from_bytes(pattern: &[u8]) -> Glob {
    let directory_only = pattern.last() == Some(&b'/');
    let body = if directory_only {
      &pattern[..pattern.len() - 1]
    } else {
      pattern
    };
    // A leading `/` anchors to the root and is not itself an interior separator.
    let body = body.strip_prefix(b"/").unwrap_or(body);
    let anchored = body.len() != pattern.len() - usize::from(directory_only);
    let name_only = !anchored && !body.contains(&b'/');

    let mut tokens = Vec::new();
    let mut i = 0;
    while i < body.len() {
      match body[i] {
        b'\\' if i + 1 < body.len() => {
          tokens.push(Token::Byte(body[i + 1]));
          i += 2;
        }
        b'?' => {
          tokens.push(Token::AnyByte);
          i += 1;
        }
        b'*' => {
          if body.get(i + 1) == Some(&b'*') {
            tokens.push(Token::DoubleStar);
            i += 2;
            // `**/` and `/**` both mean "any number of components", so the
            // adjacent separator is absorbed rather than becoming a required
            // literal `/` that `**` matching zero components cannot supply.
            if body.get(i) == Some(&b'/') {
              i += 1;
            }
          } else {
            tokens.push(Token::Star);
            i += 1;
          }
        }
        b'[' => match parse_class(&body[i..]) {
          Some((token, consumed)) => {
            tokens.push(token);
            i += consumed;
          }
          None => {
            tokens.push(Token::Byte(b'['));
            i += 1;
          }
        },
        b => {
          tokens.push(Token::Byte(b));
          i += 1;
        }
      }
    }

    Glob {
      tokens,
      name_only,
      directory_only,
      source: String::from_utf8_lossy(pattern).into_owned(),
    }
  }

  /// The pattern as written, for coverage metadata and error messages.
  pub fn as_str(&self) -> &str {
    &self.source
  }

  pub fn is_directory_only(&self) -> bool {
    self.directory_only
  }

  /// Whether a path matches.
  ///
  /// A name-only pattern is tested against the last component *and* against
  /// every intermediate component, so `node_modules/` matches
  /// `a/node_modules/b/c.js` the way Git's ignore rules do.
  pub fn matches(&self, path: &[u8]) -> bool {
    if self.name_only {
      if self.directory_only {
        // A directory-only name pattern matches a path when any *interior*
        // component matches: `build/` hides `build/x`, but not a file `build`.
        return path
          .split(|b| *b == b'/')
          .take(path.split(|b| *b == b'/').count().saturating_sub(1))
          .any(|c| self.matches_component(c));
      }
      return path
        .split(|b| *b == b'/')
        .any(|c| self.matches_component(c));
    }
    if self.directory_only {
      // Match the whole path or any directory prefix of it.
      if matches_tokens(&self.tokens, path) {
        return true;
      }
      let mut end = 0;
      while let Some(offset) = path[end..].iter().position(|b| *b == b'/') {
        let boundary = end + offset;
        if matches_tokens(&self.tokens, &path[..boundary]) {
          return true;
        }
        end = boundary + 1;
      }
      return false;
    }
    matches_tokens(&self.tokens, path)
  }

  fn matches_component(&self, component: &[u8]) -> bool {
    matches_tokens(&self.tokens, component)
  }
}

fn parse_class(input: &[u8]) -> Option<(Token, usize)> {
  debug_assert_eq!(input[0], b'[');
  let mut i = 1;
  let negated = matches!(input.get(i), Some(b'!') | Some(b'^'));
  if negated {
    i += 1;
  }
  let mut ranges = Vec::new();
  // A `]` immediately after the opening bracket is a literal, per POSIX.
  if input.get(i) == Some(&b']') {
    ranges.push((b']', b']'));
    i += 1;
  }
  while i < input.len() && input[i] != b']' {
    let lo = input[i];
    if input.get(i + 1) == Some(&b'-') && input.get(i + 2).is_some_and(|b| *b != b']') {
      ranges.push((lo, input[i + 2]));
      i += 3;
    } else {
      ranges.push((lo, lo));
      i += 1;
    }
  }
  if i >= input.len() {
    // Unterminated: the caller falls back to a literal `[`.
    return None;
  }
  Some((Token::Class { negated, ranges }, i + 1))
}

/// Backtracking match. Bounded by construction: `**` is the only token that can
/// consume a separator, and the recursion is on suffixes of the input.
fn matches_tokens(tokens: &[Token], input: &[u8]) -> bool {
  match tokens.first() {
    None => input.is_empty(),
    Some(Token::DoubleStar) => {
      // Try every suffix, including the whole input (zero components).
      if matches_tokens(&tokens[1..], input) {
        return true;
      }
      for i in 0..input.len() {
        if matches_tokens(&tokens[1..], &input[i + 1..]) {
          return true;
        }
      }
      false
    }
    Some(Token::Star) => {
      if matches_tokens(&tokens[1..], input) {
        return true;
      }
      for (i, b) in input.iter().enumerate() {
        if *b == b'/' {
          break;
        }
        if matches_tokens(&tokens[1..], &input[i + 1..]) {
          return true;
        }
      }
      false
    }
    Some(Token::AnyByte) => match input.first() {
      Some(b) if *b != b'/' => matches_tokens(&tokens[1..], &input[1..]),
      _ => false,
    },
    Some(Token::Byte(want)) => match input.first() {
      Some(b) if b == want => matches_tokens(&tokens[1..], &input[1..]),
      _ => false,
    },
    Some(Token::Class { negated, ranges }) => match input.first() {
      Some(b) if *b != b'/' => {
        let hit = ranges.iter().any(|(lo, hi)| *b >= *lo && *b <= *hi);
        if hit != *negated {
          matches_tokens(&tokens[1..], &input[1..])
        } else {
          false
        }
      }
      _ => false,
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_name_only_pattern_matches_any_component() {
    let g = Glob::new("*.min.js");
    assert!(g.matches(b"a/b/jquery.min.js"));
    assert!(g.matches(b"jquery.min.js"));
    assert!(!g.matches(b"a/b/jquery.js"));
  }

  #[test]
  fn a_pattern_with_a_separator_matches_the_whole_path() {
    let g = Glob::new("src/*.rs");
    assert!(g.matches(b"src/main.rs"));
    // `*` does not cross a separator, so a nested file does not match.
    assert!(!g.matches(b"src/a/main.rs"));
    assert!(!g.matches(b"other/src/main.rs"));
  }

  #[test]
  fn double_star_crosses_separators_and_may_match_nothing() {
    let g = Glob::new("src/**/*.rs");
    assert!(
      g.matches(b"src/main.rs"),
      "** must be able to match zero components"
    );
    assert!(g.matches(b"src/a/b/main.rs"));
  }

  #[test]
  fn a_directory_pattern_hides_everything_under_it() {
    let g = Glob::new("node_modules/");
    assert!(g.matches(b"node_modules/react/index.js"));
    assert!(g.matches(b"web/node_modules/react/index.js"));
    // The file `node_modules` itself is not a directory, so it is not hidden.
    assert!(!g.matches(b"node_modules"));
  }

  #[test]
  fn non_utf8_path_bytes_are_matched_rather_than_rejected() {
    // The reason this module exists instead of `globset`. A path byte outside
    // UTF-8 must still be matchable.
    let g = Glob::new("*.rs");
    assert!(g.matches(b"src/\xff\xfe.rs"));
  }

  #[test]
  fn character_classes_and_escapes() {
    assert!(Glob::new("[a-c]at").matches(b"bat"));
    assert!(!Glob::new("[!a-c]at").matches(b"bat"));
    assert!(Glob::new("[!a-c]at").matches(b"hat"));
    // An escaped star is a literal star, not a wildcard.
    assert!(Glob::new(r"a\*b").matches(b"a*b"));
    assert!(!Glob::new(r"a\*b").matches(b"axb"));
  }

  #[test]
  fn an_unterminated_class_is_a_literal_bracket_rather_than_an_error() {
    // A malformed `.gitignore` line must not be able to fail a search.
    let g = Glob::new("[abc");
    assert!(g.matches(b"[abc"));
  }
}
