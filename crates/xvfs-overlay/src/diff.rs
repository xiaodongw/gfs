//! Unified diffs and Git-compatible patches.
//!
//! The output is fed to `git apply`, so it is not "a diff" in the loose sense: it
//! is the byte-level format Git parses, including the `diff --git` header, the
//! `old mode`/`new mode` lines, `rename from`/`rename to`, the
//! `GIT binary patch`-shaped refusal for non-text content, and the `\ No newline
//! at end of file` marker. M3.3's verifier applies the result to a clean checkout
//! and compares trees, so anything approximate here fails there.
//!
//! # Why the diff is written rather than taken from a crate
//!
//! Myers' algorithm over lines is about eighty lines of code, and the dependency
//! it would replace has to be audited, licensed, and pinned like every other
//! entry in ADR 0001's table. The greedy forward variant with a linear-space
//! trace is what Git itself uses by default, so matching its *output* is easier
//! from the same algorithm than from a different one.
//!
//! # Binary is a decision, not a fallback
//!
//! A NUL byte in the first 8000 bytes means binary, which is Git's own rule. A
//! binary change is recorded as a header plus a note rather than as a
//! patch — a textual diff of a JPEG is worse than useless, and DESIGN.md section
//! 8.5 requires binary changes to be "represented explicitly".

use crate::status::{Change, ChangeKind};

/// Git's own binary sniff: a NUL in the first 8000 bytes.
const SNIFF: usize = 8000;

pub fn is_binary(bytes: &[u8]) -> bool {
  bytes[..bytes.len().min(SNIFF)].contains(&0)
}

/// The bytes on both sides of one change.
#[derive(Clone, Debug, Default)]
pub struct Sides {
  pub old: Vec<u8>,
  pub new: Vec<u8>,
}

/// Render one change as a `diff --git` section.
pub fn git_patch_section(change: &Change, sides: &Sides) -> Vec<u8> {
  let mut out = Vec::new();
  let old_path = change.from.as_ref().unwrap_or(&change.path);
  let a = quoted(b"a/", old_path.as_bytes());
  let b = quoted(b"b/", change.path.as_bytes());

  out.extend_from_slice(b"diff --git ");
  out.extend_from_slice(&a);
  out.push(b' ');
  out.extend_from_slice(&b);
  out.push(b'\n');

  match change.kind {
    ChangeKind::Added => {
      push_line(
        &mut out,
        format!("new file mode {:06o}", mode(change.new_mode)),
      );
    }
    ChangeKind::Deleted => {
      push_line(
        &mut out,
        format!("deleted file mode {:06o}", mode(change.old_mode)),
      );
    }
    ChangeKind::Renamed => {
      push_line(&mut out, "similarity index 100%".to_owned());
      out.extend_from_slice(b"rename from ");
      out.extend_from_slice(old_path.as_bytes());
      out.push(b'\n');
      out.extend_from_slice(b"rename to ");
      out.extend_from_slice(change.path.as_bytes());
      out.push(b'\n');
    }
    ChangeKind::ModeChanged | ChangeKind::Modified | ChangeKind::TypeChanged => {
      if change.old_mode != change.new_mode {
        push_line(&mut out, format!("old mode {:06o}", mode(change.old_mode)));
        push_line(&mut out, format!("new mode {:06o}", mode(change.new_mode)));
      }
    }
  }

  if sides.old == sides.new {
    // A pure mode change or a pure rename has no hunk, and emitting an empty one
    // makes `git apply` reject the whole patch.
    return out;
  }

  if is_binary(&sides.old) || is_binary(&sides.new) {
    // Deliberately not a `GIT binary patch`: that encoding is a delta or a
    // zlib-compressed literal, and producing one that `git apply` accepts is a
    // separate piece of work. The JSON export carries the bytes; the patch says
    // so rather than pretending the file is text.
    out.extend_from_slice(b"Binary files ");
    out.extend_from_slice(&a);
    out.extend_from_slice(b" and ");
    out.extend_from_slice(&b);
    out.extend_from_slice(b" differ\n");
    return out;
  }

  out.extend_from_slice(b"--- ");
  out.extend_from_slice(if change.kind == ChangeKind::Added {
    b"/dev/null".as_slice()
  } else {
    &a
  });
  out.push(b'\n');
  out.extend_from_slice(b"+++ ");
  out.extend_from_slice(if change.kind == ChangeKind::Deleted {
    b"/dev/null".as_slice()
  } else {
    &b
  });
  out.push(b'\n');
  out.extend_from_slice(&unified(&sides.old, &sides.new, 3));
  out
}

fn mode(value: Option<u32>) -> u32 {
  value.unwrap_or(xvfs_types::mode::REGULAR)
}

fn push_line(out: &mut Vec<u8>, line: String) {
  out.extend_from_slice(line.as_bytes());
  out.push(b'\n');
}

/// A path in a `diff --git` header, quoted the way Git quotes one.
///
/// Git C-quotes a path containing a space, a quote, a backslash, or a byte
/// outside printable ASCII. A workspace path is bytes, not text, so the
/// non-UTF-8 names in the corpus land here.
fn quoted(prefix: &[u8], path: &[u8]) -> Vec<u8> {
  let needs_quoting = path
    .iter()
    .any(|b| *b < 0x20 || *b >= 0x7f || *b == b' ' || *b == b'"' || *b == b'\\');
  if !needs_quoting {
    let mut out = prefix.to_vec();
    out.extend_from_slice(path);
    return out;
  }
  let mut out = vec![b'"'];
  out.extend_from_slice(prefix);
  for byte in path {
    match *byte {
      b'"' => out.extend_from_slice(b"\\\""),
      b'\\' => out.extend_from_slice(b"\\\\"),
      b'\n' => out.extend_from_slice(b"\\n"),
      b'\t' => out.extend_from_slice(b"\\t"),
      b if (0x20..0x7f).contains(&b) => out.push(b),
      b => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
    }
  }
  out.push(b'"');
  out
}

/// Split into lines, keeping the information about a missing final newline.
fn lines(bytes: &[u8]) -> (Vec<&[u8]>, bool) {
  if bytes.is_empty() {
    return (Vec::new(), true);
  }
  let complete = bytes.last() == Some(&b'\n');
  let body = if complete {
    &bytes[..bytes.len() - 1]
  } else {
    bytes
  };
  (body.split(|b| *b == b'\n').collect(), complete)
}

/// A unified diff with `context` lines of context.
pub fn unified(old: &[u8], new: &[u8], context: usize) -> Vec<u8> {
  let (old_lines, old_complete) = lines(old);
  let (new_lines, new_complete) = lines(new);
  let script = myers(&old_lines, &new_lines);

  let mut out = Vec::new();
  let mut index = 0;
  while index < script.len() {
    // Find the next change and the hunk that surrounds it.
    let Some(first) = (index..script.len()).find(|i| !matches!(script[*i], Edit::Keep(_))) else {
      break;
    };
    let start = first.saturating_sub(context);
    let mut end = first;
    let mut run = 0;
    for (offset, edit) in script[first..].iter().enumerate() {
      if matches!(edit, Edit::Keep(_)) {
        run += 1;
        // Two changed regions separated by more than 2*context unchanged lines
        // belong to different hunks, which is what keeps a patch readable and
        // what Git does.
        if run > context * 2 {
          break;
        }
      } else {
        run = 0;
        end = first + offset;
      }
    }
    let end = (end + context + 1).min(script.len());

    let (mut old_start, mut new_start) = (0usize, 0usize);
    for edit in &script[..start] {
      match edit {
        Edit::Keep(_) => {
          old_start += 1;
          new_start += 1;
        }
        Edit::Remove(_) => old_start += 1,
        Edit::Insert(_) => new_start += 1,
      }
    }
    let (mut old_count, mut new_count) = (0usize, 0usize);
    for edit in &script[start..end] {
      match edit {
        Edit::Keep(_) => {
          old_count += 1;
          new_count += 1;
        }
        Edit::Remove(_) => old_count += 1,
        Edit::Insert(_) => new_count += 1,
      }
    }

    push_line(
      &mut out,
      format!(
        "@@ -{},{} +{},{} @@",
        if old_count == 0 {
          old_start
        } else {
          old_start + 1
        },
        old_count,
        if new_count == 0 {
          new_start
        } else {
          new_start + 1
        },
        new_count
      ),
    );
    for (offset, edit) in script[start..end].iter().enumerate() {
      let position = start + offset;
      let (marker, line, last_of_side) = match edit {
        Edit::Keep(a) => (b' ', *a, position + 1 == script.len()),
        Edit::Remove(a) => (b'-', *a, is_last(&script[position + 1..], true)),
        Edit::Insert(b) => (b'+', *b, is_last(&script[position + 1..], false)),
      };
      out.push(marker);
      out.extend_from_slice(line);
      out.push(b'\n');
      let complete = match marker {
        b'+' => new_complete,
        b'-' => old_complete,
        _ => old_complete && new_complete,
      };
      if last_of_side && !complete {
        out.extend_from_slice(b"\\ No newline at end of file\n");
      }
    }
    index = end;
  }
  out
}

/// Whether nothing later in the script touches this side.
fn is_last(rest: &[Edit<'_>], old_side: bool) -> bool {
  !rest.iter().any(|edit| match edit {
    Edit::Keep(_) => true,
    Edit::Remove(_) => old_side,
    Edit::Insert(_) => !old_side,
  })
}

#[derive(Clone, Debug)]
enum Edit<'a> {
  Keep(&'a [u8]),
  Remove(&'a [u8]),
  Insert(&'a [u8]),
}

/// Greedy forward Myers with a recorded trace.
///
/// `O(ND)` time and `O(D^2)` space for the trace, which for source files is the
/// tradeoff Git itself makes. A pathological pair of large, wholly different
/// files degrades to a delete-all/insert-all script rather than to a hang,
/// because `max` bounds the search.
fn myers<'a>(old: &[&'a [u8]], new: &[&'a [u8]]) -> Vec<Edit<'a>> {
  let (n, m) = (old.len(), new.len());
  let max = n + m;
  if max == 0 {
    return Vec::new();
  }
  let offset = max as isize;
  let mut v = vec![0usize; 2 * max + 1];
  let mut trace: Vec<Vec<usize>> = Vec::new();

  for d in 0..=max {
    trace.push(v.clone());
    let d = d as isize;
    let mut k = -d;
    while k <= d {
      let index = (k + offset) as usize;
      let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
        v[index + 1]
      } else {
        v[index - 1] + 1
      };
      let mut y = (x as isize - k) as usize;
      while x < n && y < m && old[x] == new[y] {
        x += 1;
        y += 1;
      }
      v[index] = x;
      if x >= n && y >= m {
        return backtrack(old, new, &trace, offset);
      }
      k += 2;
    }
  }
  Vec::new()
}

fn backtrack<'a>(
  old: &[&'a [u8]],
  new: &[&'a [u8]],
  trace: &[Vec<usize>],
  offset: isize,
) -> Vec<Edit<'a>> {
  let mut script = Vec::new();
  let (mut x, mut y) = (old.len(), new.len());
  for (d, v) in trace.iter().enumerate().rev() {
    let d = d as isize;
    // `d == 0` is the whole-diagonal case and has to be handled before the
    // general step: there is no previous `k`, and computing one yields
    // `previous_y == -1`, which as a `usize` is enormous and silently stops the
    // diagonal walk. The symptom is a hunk missing its leading context line.
    if d == 0 {
      while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        script.push(Edit::Keep(old[x]));
      }
      break;
    }
    let k = x as isize - y as isize;
    let index = (k + offset) as usize;
    let previous_k = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
      k + 1
    } else {
      k - 1
    };
    let previous_x = v[(previous_k + offset) as usize];
    let previous_y = (previous_x as isize - previous_k) as usize;

    while x > previous_x && y > previous_y {
      x -= 1;
      y -= 1;
      script.push(Edit::Keep(old[x]));
    }
    if x > previous_x {
      x -= 1;
      script.push(Edit::Remove(old[x]));
    } else if y > previous_y {
      y -= 1;
      script.push(Edit::Insert(new[y]));
    }
  }
  script.reverse();
  script
}

#[cfg(test)]
mod tests {
  use super::*;

  fn text(diff: &[u8]) -> String {
    String::from_utf8_lossy(diff).into_owned()
  }

  #[test]
  fn an_unchanged_file_produces_no_hunks() {
    assert!(unified(b"a\nb\n", b"a\nb\n", 3).is_empty());
  }

  #[test]
  fn a_one_line_change_produces_one_hunk_with_context() {
    let diff = text(&unified(b"a\nb\nc\n", b"a\nB\nc\n", 3));
    assert!(diff.starts_with("@@ -1,3 +1,3 @@\n"), "{diff}");
    assert!(diff.contains("\n-b\n"), "{diff}");
    assert!(diff.contains("\n+B\n"), "{diff}");
  }

  #[test]
  fn a_missing_final_newline_is_marked_on_the_side_that_lacks_it() {
    // The case that makes a patch apply cleanly or corrupt the last line.
    let diff = text(&unified(b"a\n", b"a\nb", 3));
    assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    let both = text(&unified(b"a\nb\n", b"a\nB\n", 3));
    assert!(!both.contains("No newline"), "{both}");
  }

  #[test]
  fn appending_to_an_empty_file_is_all_insertions() {
    let diff = text(&unified(b"", b"one\ntwo\n", 3));
    assert!(diff.starts_with("@@ -0,0 +1,2 @@\n"), "{diff}");
  }

  #[test]
  fn deleting_everything_is_all_removals() {
    let diff = text(&unified(b"one\ntwo\n", b"", 3));
    assert!(diff.starts_with("@@ -1,2 +0,0 @@\n"), "{diff}");
  }

  #[test]
  fn binary_detection_matches_gits_rule() {
    assert!(is_binary(b"abc\0def"));
    assert!(!is_binary(b"abc\ndef\n"));
    // A NUL beyond the sniff window is not binary, which is Git's behaviour and
    // therefore what the oracle in the tests will agree with.
    let mut late = vec![b'x'; SNIFF + 10];
    late[SNIFF + 5] = 0;
    assert!(!is_binary(&late));
  }

  #[test]
  fn a_path_needing_quotes_is_quoted_the_way_git_quotes_it() {
    assert_eq!(quoted(b"a/", b"src/main.rs"), b"a/src/main.rs");
    assert_eq!(quoted(b"a/", b"with space"), b"\"a/with space\"");
    // A non-UTF-8 name, which the `bytes` fixture has and every naive
    // implementation turns into U+FFFD.
    assert_eq!(quoted(b"a/", b"caf\xe9"), b"\"a/caf\\351\"");
  }

  #[test]
  fn a_large_rewrite_terminates_rather_than_hanging() {
    let old: Vec<u8> = (0..400)
      .flat_map(|i| format!("old {i}\n").into_bytes())
      .collect();
    let new: Vec<u8> = (0..400)
      .flat_map(|i| format!("new {i}\n").into_bytes())
      .collect();
    assert!(!unified(&old, &new, 3).is_empty());
  }
}
