//! Trigram posting lists: creation, merging, and lookup.
//!
//! One Roaring bitmap of blob keys per trigram. A query intersects the postings
//! of the trigrams its literal requires, and then intersects the result with the
//! snapshot's membership bitmap — **before any blob is read**. That order is the
//! whole design: a query over one commit never inflates content belonging to
//! another.
//!
//! # There are no segments, so there is no segment compaction
//!
//! PLAN.md M4.3 asks for posting creation, compaction, and lookup. This store
//! merges each batch into the single bitmap for its trigram at write time, so a
//! trigram has exactly one posting list at all times and there is nothing to
//! compact later. The cost that buys is a read-modify-write per touched trigram
//! per batch, amortized by making batches large; the cost it avoids is a query
//! that has to union an unbounded number of segments and a background process
//! that must run for queries to stay fast.
//!
//! ADR 0004 flagged the trigram builder as the one component measured slower
//! than the Tantivy alternative, with obvious headroom. This is where that
//! headroom is: batching is implemented, parallel merge is not.

use std::collections::HashMap;
use std::sync::Arc;

use roaring::RoaringBitmap;
use rusqlite::OptionalExtension;
use xvfs_types::error::XvfsError;
use xvfs_types::RepositoryId;

use crate::registry::BlobKey;
use crate::store::{db_error, SearchStore};
use crate::trigram;

/// Trigrams accumulated in memory before being merged into the store.
///
/// Deliberately a separate type from the store. An indexer holds one of these
/// across a batch of blobs and flushes once; making it the same object as the
/// store would invite a flush per blob, which is the shape that makes trigram
/// indexing slow.
#[derive(Debug, Default)]
pub struct PostingBatch {
  by_trigram: HashMap<u32, RoaringBitmap>,
  keys: Vec<BlobKey>,
  bytes: u64,
}

impl PostingBatch {
  pub fn new() -> PostingBatch {
    PostingBatch::default()
  }

  /// Record one blob's trigrams.
  pub fn add(&mut self, key: BlobKey, content: &[u8]) {
    for t in trigram::trigrams(content) {
      self.by_trigram.entry(t).or_default().insert(key);
    }
    self.keys.push(key);
    self.bytes += content.len() as u64;
  }

  pub fn is_empty(&self) -> bool {
    self.keys.is_empty()
  }

  pub fn blob_count(&self) -> usize {
    self.keys.len()
  }

  pub fn trigram_count(&self) -> usize {
    self.by_trigram.len()
  }

  pub fn bytes(&self) -> u64 {
    self.bytes
  }

  pub fn keys(&self) -> &[BlobKey] {
    &self.keys
  }
}

/// One repository's posting lists.
#[derive(Debug)]
pub struct PostingStore {
  store: Arc<SearchStore>,
  repository: RepositoryId,
}

impl PostingStore {
  pub fn new(store: Arc<SearchStore>, repository: RepositoryId) -> PostingStore {
    PostingStore { store, repository }
  }

  /// Merge a batch into the store, in one transaction.
  ///
  /// Atomic per batch on purpose: a crash halfway through would otherwise leave
  /// some of a blob's trigrams present and some absent, which is worse than the
  /// blob being absent entirely — a partially indexed blob answers "no match"
  /// for the trigrams it is missing, and nothing marks it as incomplete. With a
  /// transaction, the blob is either fully indexed or not indexed at all, and
  /// `blobs.indexed` (set by the caller after this returns) records which.
  pub fn merge(&self, batch: &PostingBatch) -> Result<(), XvfsError> {
    if batch.is_empty() {
      return Ok(());
    }
    let repository = self.repository.as_str().to_owned();
    self.store.with_tx(|tx| {
      let mut read = tx
        .prepare("SELECT keys FROM postings WHERE repository_id = ?1 AND trigram = ?2")
        .map_err(db_error)?;
      let mut write = tx
        .prepare(
          "INSERT INTO postings (repository_id, trigram, keys) VALUES (?1, ?2, ?3)
           ON CONFLICT (repository_id, trigram) DO UPDATE SET keys = ?3",
        )
        .map_err(db_error)?;

      for (t, additions) in &batch.by_trigram {
        let existing: Option<Vec<u8>> = read
          .query_row(rusqlite::params![&repository, *t as i64], |r| r.get(0))
          .optional()
          .map_err(db_error)?;
        let mut merged = match existing {
          Some(bytes) => decode_bitmap(&bytes)?,
          None => RoaringBitmap::new(),
        };
        merged |= additions;
        write
          .execute(rusqlite::params![
            &repository,
            *t as i64,
            encode_bitmap(&merged)
          ])
          .map_err(db_error)?;
      }
      Ok(())
    })
  }

  /// The posting list for one trigram.
  pub fn posting(&self, t: u32) -> Result<Option<RoaringBitmap>, XvfsError> {
    let repository = self.repository.as_str().to_owned();
    let bytes: Option<Vec<u8>> = self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT keys FROM postings WHERE repository_id = ?1 AND trigram = ?2",
          rusqlite::params![&repository, t as i64],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)
    })?;
    match bytes {
      Some(bytes) => Ok(Some(decode_bitmap(&bytes)?)),
      None => Ok(None),
    }
  }

  /// The union of a trigram's posting list with those of its ASCII case
  /// variants.
  fn posting_folded(&self, t: u32, case_insensitive: bool) -> Result<RoaringBitmap, XvfsError> {
    if !case_insensitive {
      return Ok(self.posting(t)?.unwrap_or_default());
    }
    let mut union = RoaringBitmap::new();
    for variant in trigram::case_variants(t) {
      if let Some(p) = self.posting(variant)? {
        union |= p;
      }
    }
    Ok(union)
  }

  /// Candidate blob keys for a required-literal set, already intersected with a
  /// snapshot's membership.
  ///
  /// The intersection with `members` happens **first**, against the smallest
  /// posting list, so the working set never grows to the size of the repository
  /// when the query is scoped to one commit.
  ///
  /// A trigram absent from the index yields an empty candidate set for its
  /// alternative — which is a correct negative, not a gap: nothing in the
  /// repository contains those three bytes.
  pub fn candidates(
    &self,
    literals: &trigram::RequiredLiterals,
    members: &RoaringBitmap,
    case_insensitive: bool,
  ) -> Result<RoaringBitmap, XvfsError> {
    let mut union = RoaringBitmap::new();
    for alternative in literals.alternatives() {
      let required = trigram::trigrams(alternative);
      if required.is_empty() {
        // Cannot happen: `required_literals` rejects anything under three bytes.
        // If it ever did, returning everything is the safe direction.
        return Ok(members.clone());
      }
      let mut lists = Vec::with_capacity(required.len());
      for t in &required {
        lists.push(self.posting_folded(*t, case_insensitive)?);
      }
      // Rarest first, so the intersection shrinks fastest.
      lists.sort_by_key(|l| l.len());
      let mut acc = &lists[0] & members;
      for list in &lists[1..] {
        if acc.is_empty() {
          break;
        }
        acc &= list;
      }
      union |= acc;
    }
    Ok(union)
  }

  /// Total serialized posting bytes, for capacity reporting.
  pub fn stored_bytes(&self) -> Result<u64, XvfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT COALESCE(SUM(LENGTH(keys)), 0) FROM postings WHERE repository_id = ?1",
          [&repository],
          |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(db_error)
    })
  }

  pub fn trigram_count(&self) -> Result<u64, XvfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT COUNT(*) FROM postings WHERE repository_id = ?1",
          [&repository],
          |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(db_error)
    })
  }
}

fn encode_bitmap(bitmap: &RoaringBitmap) -> Vec<u8> {
  let mut out = Vec::with_capacity(bitmap.serialized_size());
  // Only fails on a write error, and a `Vec` has none.
  bitmap.serialize_into(&mut out).expect("in-memory write");
  out
}

fn decode_bitmap(bytes: &[u8]) -> Result<RoaringBitmap, XvfsError> {
  RoaringBitmap::deserialize_from(bytes)
    .map_err(|e| XvfsError::internal(format!("a stored posting list is unreadable: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::trigram::required_literals;

  fn store() -> PostingStore {
    PostingStore::new(
      Arc::new(SearchStore::open_in_memory().unwrap()),
      RepositoryId::parse("r-test").unwrap(),
    )
  }

  fn all(keys: &[u32]) -> RoaringBitmap {
    keys.iter().copied().collect()
  }

  #[test]
  fn a_literal_finds_the_blobs_that_contain_it() {
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"fn authorize_request() {}\n");
    batch.add(2, b"fn unrelated() {}\n");
    s.merge(&batch).unwrap();

    let lits = required_literals("authorize", true, false).unwrap();
    let got = s.candidates(&lits, &all(&[1, 2]), false).unwrap();
    assert_eq!(got, all(&[1]));
  }

  #[test]
  fn candidates_never_leave_the_snapshot() {
    // The property the whole representation exists for: a query over one commit
    // does not propose content belonging to another.
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"needle\n");
    batch.add(2, b"needle\n");
    s.merge(&batch).unwrap();

    let lits = required_literals("needle", true, false).unwrap();
    assert_eq!(s.candidates(&lits, &all(&[2]), false).unwrap(), all(&[2]));
  }

  #[test]
  fn an_absent_trigram_is_a_correct_negative_not_a_gap() {
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"hello\n");
    s.merge(&batch).unwrap();

    let lits = required_literals("zzzzzz", true, false).unwrap();
    assert!(s.candidates(&lits, &all(&[1]), false).unwrap().is_empty());
  }

  #[test]
  fn alternation_unions_the_branches() {
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"alpha\n");
    batch.add(2, b"bravo\n");
    batch.add(3, b"charlie\n");
    s.merge(&batch).unwrap();

    let lits = required_literals("alpha|bravo", false, false).unwrap();
    assert_eq!(
      s.candidates(&lits, &all(&[1, 2, 3]), false).unwrap(),
      all(&[1, 2])
    );
  }

  #[test]
  fn case_insensitive_lookup_finds_the_other_casing() {
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"NeedleInHaystack\n");
    s.merge(&batch).unwrap();

    let lits = required_literals("needle", true, true).unwrap();
    assert_eq!(s.candidates(&lits, &all(&[1]), true).unwrap(), all(&[1]));
    // And a case-sensitive lookup of the same literal does not.
    assert!(s.candidates(&lits, &all(&[1]), false).unwrap().is_empty());
  }

  #[test]
  fn merging_two_batches_unions_rather_than_replaces() {
    // The failure this guards is silent: the second batch overwriting the first
    // would make every blob indexed before it invisible, and nothing would
    // report a gap.
    let s = store();
    let mut first = PostingBatch::new();
    first.add(1, b"shared token here\n");
    s.merge(&first).unwrap();

    let mut second = PostingBatch::new();
    second.add(2, b"shared token there\n");
    s.merge(&second).unwrap();

    let lits = required_literals("shared", true, false).unwrap();
    assert_eq!(
      s.candidates(&lits, &all(&[1, 2]), false).unwrap(),
      all(&[1, 2])
    );
  }

  #[test]
  fn a_blob_shorter_than_a_trigram_contributes_nothing_and_breaks_nothing() {
    let s = store();
    let mut batch = PostingBatch::new();
    batch.add(1, b"ab");
    s.merge(&batch).unwrap();
    assert_eq!(s.trigram_count().unwrap(), 0);
  }
}
