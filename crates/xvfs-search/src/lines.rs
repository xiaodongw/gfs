//! Line boundaries over raw bytes.
//!
//! PLAN.md M4.1: "Decode line boundaries without normalizing returned byte
//! offsets." That sentence is doing real work. A search result's column is a
//! byte offset into the line, and an agent uses it to edit the file. If the
//! decoder stripped a `\r`, re-encoded a UTF-16 file, or expanded a tab, every
//! column it reported would be an offset into a line that does not exist on
//! disk, and the edit would land in the wrong place.
//!
//! So the line's byte range is a range into the blob exactly as stored. The
//! terminating `\n` is excluded because it is a separator rather than content; a
//! preceding `\r` is **not**, because it is a byte of the file and removing it
//! would shift every column after it in a CRLF file.
//!
//! [`Line::display`] is offered separately for snippets, where a trailing `\r`
//! would corrupt the terminal rather than inform the reader. Nothing that
//! produces a column ever uses it.

/// One line of a blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Line<'a> {
  /// 1-based, the way every editor and `rg` count.
  pub number: u64,
  /// Byte offset of the first byte of the line within the blob.
  pub start: usize,
  /// The line's bytes, excluding the terminating `\n` and including any `\r`.
  pub bytes: &'a [u8],
}

impl<'a> Line<'a> {
  /// The bytes to show a human, with a CRLF's `\r` removed.
  ///
  /// Never used to compute a column. See the module docs.
  pub fn display(&self) -> &'a [u8] {
    match self.bytes.last() {
      Some(b'\r') => &self.bytes[..self.bytes.len() - 1],
      _ => self.bytes,
    }
  }

  /// The 1-based column of a byte offset measured from the start of the line.
  pub fn column(&self, offset_in_line: usize) -> u64 {
    offset_in_line as u64 + 1
  }
}

/// Iterate a blob's lines.
///
/// A final line without a trailing newline is a line. It is the case that a
/// naive `split(b'\n')` gets right by accident and a `lines.count()` based on
/// counting newlines gets wrong, and the fixture matrix has a file with exactly
/// that shape for this reason.
pub fn lines(content: &[u8]) -> Lines<'_> {
  Lines {
    content,
    offset: 0,
    number: 0,
    done: false,
  }
}

#[derive(Debug)]
pub struct Lines<'a> {
  content: &'a [u8],
  offset: usize,
  number: u64,
  done: bool,
}

impl<'a> Iterator for Lines<'a> {
  type Item = Line<'a>;

  fn next(&mut self) -> Option<Line<'a>> {
    if self.done || self.offset > self.content.len() {
      return None;
    }
    if self.offset == self.content.len() {
      self.done = true;
      // An empty blob has no lines; a blob ending in `\n` has no extra empty
      // line after it. Both fall out of stopping here.
      return None;
    }
    self.number += 1;
    let start = self.offset;
    match memchr::memchr(b'\n', &self.content[start..]) {
      Some(rel) => {
        let end = start + rel;
        self.offset = end + 1;
        Some(Line {
          number: self.number,
          start,
          bytes: &self.content[start..end],
        })
      }
      None => {
        self.done = true;
        Some(Line {
          number: self.number,
          start,
          bytes: &self.content[start..],
        })
      }
    }
  }
}

/// The line containing a byte offset, and the offset within that line.
///
/// Linear, and used only when a caller has an offset from a whole-blob match
/// rather than a per-line one.
pub fn locate(content: &[u8], offset: usize) -> Option<(Line<'_>, usize)> {
  lines(content)
    .find(|line| offset >= line.start && offset <= line.start + line.bytes.len())
    .map(|line| {
      let within = offset - line.start;
      (line, within)
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn numbered(content: &[u8]) -> Vec<(u64, Vec<u8>)> {
    lines(content)
      .map(|l| (l.number, l.bytes.to_vec()))
      .collect()
  }

  #[test]
  fn a_final_line_without_a_newline_is_still_a_line() {
    assert_eq!(
      numbered(b"a\nb"),
      vec![(1, b"a".to_vec()), (2, b"b".to_vec())]
    );
  }

  #[test]
  fn a_trailing_newline_does_not_invent_an_empty_line() {
    assert_eq!(numbered(b"a\n"), vec![(1, b"a".to_vec())]);
    assert!(numbered(b"").is_empty());
  }

  #[test]
  fn an_interior_empty_line_is_kept() {
    assert_eq!(
      numbered(b"a\n\nb\n"),
      vec![(1, b"a".to_vec()), (2, Vec::new()), (3, b"b".to_vec())]
    );
  }

  #[test]
  fn crlf_keeps_the_carriage_return_in_the_offsets() {
    // The property PLAN.md M4.1 asks for. `needle` starts at byte 0 of line 2
    // whether or not the previous line ended CRLF, and the `\r` at the end of
    // line 2 is part of it — an edit driven by these offsets has to agree with
    // the bytes on disk.
    let content = b"first\r\nneedle\r\n";
    let all: Vec<Line<'_>> = lines(content).collect();
    assert_eq!(all[1].start, 7);
    assert_eq!(all[1].bytes, b"needle\r");
    assert_eq!(all[1].display(), b"needle");
    assert_eq!(all[1].column(0), 1);
  }

  #[test]
  fn offsets_are_into_the_blob_not_into_the_line() {
    let content = b"ab\ncd\n";
    let (line, within) = locate(content, 4).unwrap();
    assert_eq!(line.number, 2);
    assert_eq!(within, 1);
    assert_eq!(line.column(within), 2);
  }

  #[test]
  fn invalid_utf8_lines_are_returned_as_bytes() {
    let content = b"caf\xe9\nbar\n";
    let all = numbered(content);
    assert_eq!(all[0].1, b"caf\xe9".to_vec());
  }
}
