//! The blob registry: stable, repository-scoped keys, assigned transactionally.
//!
//! DESIGN.md section 6.5's whole economy rests on this table. A repository's
//! unique blobs are indexed once and referenced by every snapshot that contains
//! them, so successive commits — which ADR 0004 measured as adding ~4 (vscode) to
//! ~39 (linux) new blobs each — cost almost nothing to prepare. That only works
//! if a key means the same blob forever, which is why keys are allocated inside
//! the same transaction as the row that uses them and are never reused.
//!
//! # Idempotence is the property, not an optimization
//!
//! `intern` on a known OID returns its existing key and changes nothing.
//! `ingest` on a known OID reads no bytes at all. A manifest build that is
//! retried after a crash, or two mounts preparing the same commit at once,
//! therefore converge instead of duplicating work or — worse — producing two
//! keys for one blob and a bitmap that is missing half its members.
//!
//! # The budget is the sandbox
//!
//! PLAN.md M4.1 asks for parsing to be rate-limited and sandboxed. There is no
//! parser here to sandbox: classification is a NUL scan plus a UTF-8 validation,
//! and trigram extraction is a three-byte window. Both are linear, allocate
//! nothing proportional to the input beyond the blob itself, and cannot recurse.
//! What *can* run away is the aggregate — a repository of 8 MiB generated files
//! will happily inflate gigabytes — so [`IngestBudget`] bounds bytes and blob
//! count per batch and the caller resumes on the next one. That is the real
//! control, and stating it plainly is better than gesturing at a sandbox that
//! would have nothing to contain.

use std::collections::BTreeMap;
use std::sync::Arc;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{ObjectId, RepositoryId};
use rusqlite::OptionalExtension;

use crate::classify::{classify_content, classify_size, ContentClass, CorpusPolicy};
use crate::store::{db_error, SearchStore};
use crate::BlobSource;

/// A blob's stable numeric identity within one repository.
///
/// `u32` because it indexes a Roaring bitmap, which is a `u32` set. Four billion
/// unique blobs per repository is far past anything the corpus contains — linux
/// has 94 751 tip files — and the registry range-checks rather than wrapping.
pub type BlobKey = u32;

/// A blob as a tree walk found it: an object ID and the size Git recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobFact {
  pub oid: ObjectId,
  pub size: u64,
}

/// What the registry knows about one blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRecord {
  pub key: BlobKey,
  pub oid: ObjectId,
  pub size: u64,
  /// `None` until the bytes have been examined.
  ///
  /// A manifest build interns every blob it walks; classification happens in
  /// batches behind it. A query that reaches an unclassified blob has an **index
  /// gap**, which is a coverage exclusion an agent must be told about — not the
  /// same thing as a binary file, and emphatically not the same thing as a file
  /// with no matches.
  pub class: Option<ContentClass>,
  /// Whether posting lists exist for this key.
  ///
  /// Distinct from `class.is_indexable()` for the same reason: a blob can be
  /// classified as text before the posting builder has reached it.
  pub indexed: bool,
}

impl BlobRecord {
  /// Whether a query may rely on the index for this blob.
  pub fn is_searchable_through_the_index(&self) -> bool {
    self.indexed && self.class.is_some_and(|c| c.is_indexable())
  }
}

/// Bounds on one ingestion batch.
#[derive(Clone, Copy, Debug)]
pub struct IngestBudget {
  /// Never inflate a blob larger than this. Equal to the policy's cap by
  /// default; separate so a caller can be stricter without redefining the
  /// corpus, which would change what is *reported* as excluded.
  pub max_blob_bytes: u64,
  /// Stop the batch once this many bytes have been inflated.
  pub max_bytes_per_batch: u64,
  pub max_blobs_per_batch: usize,
}

impl Default for IngestBudget {
  fn default() -> Self {
    IngestBudget {
      max_blob_bytes: gfs_types::limits::MAX_SEARCHABLE_BLOB_BYTES,
      // 1 GiB per batch: large enough that a full linux index is a handful of
      // batches, small enough that a cancelled preparation gives a thread back
      // in seconds rather than minutes.
      max_bytes_per_batch: 1 << 30,
      max_blobs_per_batch: 50_000,
    }
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IngestReport {
  /// Blobs that already had a key and a class; no bytes were read for them.
  pub already_known: usize,
  /// Blobs classified in this batch.
  pub newly_classified: usize,
  pub bytes_read: u64,
  pub by_class: BTreeMap<ContentClass, usize>,
  /// True when the batch stopped on a budget rather than on the input. The
  /// caller must resume; treating this as "done" would silently leave a
  /// half-indexed corpus, which is the shape of failure M4's exit gate is
  /// entirely about.
  pub budget_exhausted: bool,
}

/// One repository's blob registry.
#[derive(Debug)]
pub struct BlobRegistry {
  store: Arc<SearchStore>,
  repository: RepositoryId,
  policy: CorpusPolicy,
}

impl BlobRegistry {
  pub fn new(store: Arc<SearchStore>, repository: RepositoryId, policy: CorpusPolicy) -> Self {
    BlobRegistry {
      store,
      repository,
      policy,
    }
  }

  pub fn policy(&self) -> &CorpusPolicy {
    &self.policy
  }

  pub fn repository(&self) -> &RepositoryId {
    &self.repository
  }

  /// Assign keys to blobs, returning one key per input in order.
  ///
  /// Everything happens in one transaction: the allocator is read, the new rows
  /// are inserted, and the allocator is advanced. A concurrent call blocks on
  /// SQLite's writer lock rather than observing a stale `next_key`, so two
  /// simultaneous manifest builds of sibling commits cannot hand the same key to
  /// two different blobs.
  ///
  /// Interning does **not** classify. A new row's `content_class` is NULL until
  /// [`BlobRegistry::ingest`] examines the bytes, so a blob that a manifest walk
  /// has seen but the classifier has not is distinguishable from ordinary text.
  /// Guessing a default here is how an unexamined blob ends up silently treated
  /// as a searched one.
  pub fn intern(&self, facts: &[BlobFact]) -> Result<Vec<BlobKey>, GfsError> {
    if facts.is_empty() {
      return Ok(Vec::new());
    }
    let repository = self.repository.as_str().to_owned();
    self.store.with_tx(|tx| {
      let mut next: i64 = tx
        .query_row(
          "SELECT next_key FROM blob_key_seq WHERE repository_id = ?1",
          [&repository],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?
        .unwrap_or(0);

      let mut lookup = tx
        .prepare("SELECT blob_key FROM blobs WHERE repository_id = ?1 AND oid = ?2")
        .map_err(db_error)?;
      let mut insert = tx
        .prepare(
          "INSERT INTO blobs (repository_id, oid, blob_key, size, content_class, indexed)
           VALUES (?1, ?2, ?3, ?4, NULL, 0)",
        )
        .map_err(db_error)?;

      let mut out = Vec::with_capacity(facts.len());
      // Within one batch the same OID can appear at many paths; remember what
      // was just allocated so a repeated blob does not get a second key.
      let mut fresh: std::collections::HashMap<String, BlobKey> = std::collections::HashMap::new();

      for fact in facts {
        let oid = fact.oid.to_qualified();
        if let Some(key) = fresh.get(&oid) {
          out.push(*key);
          continue;
        }
        let existing: Option<i64> = lookup
          .query_row(rusqlite::params![&repository, &oid], |r| r.get(0))
          .optional()
          .map_err(db_error)?;
        if let Some(key) = existing {
          let key = to_key(key)?;
          fresh.insert(oid, key);
          out.push(key);
          continue;
        }

        let key = to_key(next)?;
        next += 1;
        insert
          .execute(rusqlite::params![
            &repository,
            &oid,
            key as i64,
            fact.size as i64,
          ])
          .map_err(db_error)?;
        fresh.insert(oid, key);
        out.push(key);
      }

      drop(lookup);
      drop(insert);
      tx.execute(
        "INSERT INTO blob_key_seq (repository_id, next_key) VALUES (?1, ?2)
         ON CONFLICT (repository_id) DO UPDATE SET next_key = ?2",
        rusqlite::params![&repository, next],
      )
      .map_err(db_error)?;
      Ok(out)
    })
  }

  /// Classify every not-yet-classified blob in `facts`, reading content only
  /// where the size does not already decide it.
  ///
  /// `sink` sees each indexable blob's bytes exactly once, which is where M4.3
  /// hangs trigram extraction. It is a callback rather than a returned buffer
  /// because holding a batch of inflated blobs in memory is precisely the cost
  /// this design exists to avoid.
  pub fn ingest(
    &self,
    source: &dyn BlobSource,
    facts: &[BlobFact],
    budget: &IngestBudget,
    mut sink: impl FnMut(BlobKey, &[u8]) -> Result<(), GfsError>,
  ) -> Result<IngestReport, GfsError> {
    let keys = self.intern(facts)?;
    let mut report = IngestReport::default();
    // Classified rows, applied in one transaction at the end so a batch is
    // atomic: either the whole batch's classifications are durable or none are,
    // and a retry re-reads the same blobs rather than a subset.
    let mut updates: Vec<(BlobKey, ContentClass)> = Vec::new();
    let known = self.classified_keys(&keys)?;

    for (fact, key) in facts.iter().zip(keys.iter()) {
      if known.contains(key) {
        report.already_known += 1;
        continue;
      }
      if updates.iter().any(|(k, _)| k == key) {
        // A repeated blob inside one batch. Already handled.
        report.already_known += 1;
        continue;
      }
      if report.newly_classified >= budget.max_blobs_per_batch
        || report.bytes_read >= budget.max_bytes_per_batch
      {
        report.budget_exhausted = true;
        break;
      }

      // Size first: an oversized blob is excluded without an inflate, which is
      // what ADR 0004's cost model assumes.
      if let Some(class) = classify_size(&self.policy, fact.size) {
        updates.push((*key, class));
        report.newly_classified += 1;
        *report.by_class.entry(class).or_default() += 1;
        continue;
      }
      if fact.size > budget.max_blob_bytes {
        updates.push((*key, ContentClass::Oversized));
        report.newly_classified += 1;
        *report.by_class.entry(ContentClass::Oversized).or_default() += 1;
        continue;
      }

      let content = source.read(&fact.oid)?;
      report.bytes_read += content.len() as u64;
      let class = classify_content(&self.policy, &content);
      if class.is_indexable() {
        sink(*key, &content)?;
      }
      updates.push((*key, class));
      report.newly_classified += 1;
      *report.by_class.entry(class).or_default() += 1;
    }

    self.apply_classifications(&updates)?;
    Ok(report)
  }

  /// Mark keys as having posting lists. Called by the index builder.
  pub fn mark_indexed(&self, keys: &[BlobKey]) -> Result<(), GfsError> {
    if keys.is_empty() {
      return Ok(());
    }
    let repository = self.repository.as_str().to_owned();
    self.store.with_tx(|tx| {
      let mut stmt = tx
        .prepare("UPDATE blobs SET indexed = 1 WHERE repository_id = ?1 AND blob_key = ?2")
        .map_err(db_error)?;
      for key in keys {
        stmt
          .execute(rusqlite::params![&repository, *key as i64])
          .map_err(db_error)?;
      }
      Ok(())
    })
  }

  pub fn record_for_oid(&self, oid: &ObjectId) -> Result<Option<BlobRecord>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let oid_text = oid.to_qualified();
    self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT blob_key, oid, size, content_class, indexed
           FROM blobs WHERE repository_id = ?1 AND oid = ?2",
          rusqlite::params![&repository, &oid_text],
          row_to_record,
        )
        .optional()
        .map_err(db_error)?
        .transpose()
    })
  }

  /// Records for a set of keys, in key order.
  ///
  /// Chunked, because SQLite's default parameter limit is well below the number
  /// of blob keys a snapshot bitmap can hold.
  pub fn records_for_keys(&self, keys: &[BlobKey]) -> Result<Vec<BlobRecord>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let mut out = Vec::with_capacity(keys.len());
    self.store.with_conn(|conn| {
      for chunk in keys.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
          .collect::<Vec<_>>()
          .join(",");
        let sql = format!(
          "SELECT blob_key, oid, size, content_class, indexed
           FROM blobs WHERE repository_id = ?1 AND blob_key IN ({placeholders})
           ORDER BY blob_key"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_error)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(repository.clone())];
        for key in chunk {
          params.push(Box::new(*key as i64));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
          .query_map(refs.as_slice(), row_to_record)
          .map_err(db_error)?;
        for row in rows {
          out.push(row.map_err(db_error)??);
        }
      }
      Ok(())
    })?;
    Ok(out)
  }

  pub fn len(&self) -> Result<u64, GfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT COUNT(*) FROM blobs WHERE repository_id = ?1",
          [&repository],
          |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(db_error)
    })
  }

  pub fn is_empty(&self) -> Result<bool, GfsError> {
    Ok(self.len()? == 0)
  }

  /// Which of these keys have already been classified.
  fn classified_keys(
    &self,
    keys: &[BlobKey],
  ) -> Result<std::collections::HashSet<BlobKey>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let mut out = std::collections::HashSet::new();
    self.store.with_conn(|conn| {
      for chunk in keys.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
          .collect::<Vec<_>>()
          .join(",");
        let sql = format!(
          "SELECT blob_key FROM blobs
           WHERE repository_id = ?1 AND blob_key IN ({placeholders})
             AND content_class IS NOT NULL"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_error)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(repository.clone())];
        for key in chunk {
          params.push(Box::new(*key as i64));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
          .query_map(refs.as_slice(), |r| r.get::<_, i64>(0))
          .map_err(db_error)?;
        for row in rows {
          out.insert(to_key(row.map_err(db_error)?)?);
        }
      }
      Ok(())
    })?;
    Ok(out)
  }

  fn apply_classifications(&self, updates: &[(BlobKey, ContentClass)]) -> Result<(), GfsError> {
    if updates.is_empty() {
      return Ok(());
    }
    let repository = self.repository.as_str().to_owned();
    self.store.with_tx(|tx| {
      let mut stmt = tx
        .prepare(
          "UPDATE blobs SET content_class = ?3
           WHERE repository_id = ?1 AND blob_key = ?2",
        )
        .map_err(db_error)?;
      for (key, class) in updates {
        stmt
          .execute(rusqlite::params![&repository, *key as i64, class.as_str()])
          .map_err(db_error)?;
      }
      Ok(())
    })
  }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<BlobRecord, GfsError>> {
  let key: i64 = row.get(0)?;
  let oid: String = row.get(1)?;
  let size: i64 = row.get(2)?;
  let class: Option<String> = row.get(3)?;
  let indexed: i64 = row.get(4)?;
  Ok((|| {
    let class = match class {
      None => None,
      Some(name) => Some(
        ContentClass::from_wire_name(&name)
          .ok_or_else(|| GfsError::internal(format!("unknown stored content class {name:?}")))?,
      ),
    };
    Ok(BlobRecord {
      key: to_key(key)?,
      oid: ObjectId::parse_qualified(&oid)
        .map_err(|e| GfsError::internal(format!("stored object ID is unreadable: {e}")))?,
      size: size as u64,
      class,
      indexed: indexed != 0,
    })
  })())
}

fn to_key(value: i64) -> Result<BlobKey, GfsError> {
  BlobKey::try_from(value).map_err(|_| {
    GfsError::new(
      ErrorCode::ResourceLimit,
      "the repository has more unique blobs than the index can key",
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;
  use std::collections::HashMap;

  struct MapSource(HashMap<String, Vec<u8>>);

  impl BlobSource for MapSource {
    fn size(&self, oid: &ObjectId) -> Result<u64, GfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .map(|b| b.len() as u64)
        .ok_or_else(|| GfsError::not_found("no such blob"))
    }
    fn read(&self, oid: &ObjectId) -> Result<Vec<u8>, GfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .cloned()
        .ok_or_else(|| GfsError::not_found("no such blob"))
    }
  }

  fn oid(n: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[n; 20]).unwrap()
  }

  fn registry() -> BlobRegistry {
    BlobRegistry::new(
      Arc::new(SearchStore::open_in_memory().unwrap()),
      RepositoryId::parse("r-test").unwrap(),
      CorpusPolicy::default(),
    )
  }

  fn source(entries: &[(ObjectId, &[u8])]) -> MapSource {
    MapSource(
      entries
        .iter()
        .map(|(o, b)| (o.to_qualified(), b.to_vec()))
        .collect(),
    )
  }

  #[test]
  fn keys_are_stable_and_interning_is_idempotent() {
    let reg = registry();
    let facts = vec![
      BlobFact {
        oid: oid(1),
        size: 3,
      },
      BlobFact {
        oid: oid(2),
        size: 4,
      },
    ];
    let first = reg.intern(&facts).unwrap();
    let second = reg.intern(&facts).unwrap();
    assert_eq!(first, second);
    assert_eq!(first, vec![0, 1]);
    assert_eq!(reg.len().unwrap(), 2);
  }

  #[test]
  fn a_blob_repeated_within_one_batch_gets_one_key() {
    // The case that would otherwise put two keys in a snapshot bitmap for one
    // blob, so the membership set no longer means what a posting intersection
    // assumes it means.
    let reg = registry();
    let facts = vec![
      BlobFact {
        oid: oid(1),
        size: 3,
      },
      BlobFact {
        oid: oid(1),
        size: 3,
      },
    ];
    let keys = reg.intern(&facts).unwrap();
    assert_eq!(keys, vec![0, 0]);
    assert_eq!(reg.len().unwrap(), 1);
  }

  #[test]
  fn ingestion_reads_each_oid_once_however_often_it_appears() {
    let reg = registry();
    let src = source(&[(oid(1), b"hello world\n"), (oid(2), b"x\0y")]);
    let facts = vec![
      BlobFact {
        oid: oid(1),
        size: 12,
      },
      BlobFact {
        oid: oid(2),
        size: 3,
      },
      BlobFact {
        oid: oid(1),
        size: 12,
      },
    ];

    let mut seen = Vec::new();
    let report = reg
      .ingest(&src, &facts, &IngestBudget::default(), |key, content| {
        seen.push((key, content.len()));
        Ok(())
      })
      .unwrap();

    assert_eq!(report.newly_classified, 2);
    assert_eq!(report.already_known, 1);
    assert_eq!(
      seen,
      vec![(0, 12)],
      "the binary blob is not offered for indexing"
    );
    assert_eq!(report.by_class[&ContentClass::Text], 1);
    assert_eq!(report.by_class[&ContentClass::Binary], 1);

    // A second pass reads nothing: this is what makes preparing a sibling commit
    // cost only its new blobs.
    let mut seen_again = 0;
    let again = reg
      .ingest(&src, &facts, &IngestBudget::default(), |_, _| {
        seen_again += 1;
        Ok(())
      })
      .unwrap();
    assert_eq!(again.bytes_read, 0);
    assert_eq!(seen_again, 0);
  }

  #[test]
  fn an_oversized_blob_is_excluded_without_being_read() {
    let reg = registry();
    // The source has nothing in it, so reading would fail. Passing proves the
    // size path never reached the source.
    let src = source(&[]);
    let facts = vec![BlobFact {
      oid: oid(9),
      size: gfs_types::limits::MAX_SEARCHABLE_BLOB_BYTES + 1,
    }];
    let report = reg
      .ingest(&src, &facts, &IngestBudget::default(), |_, _| Ok(()))
      .unwrap();
    assert_eq!(report.bytes_read, 0);
    assert_eq!(report.by_class[&ContentClass::Oversized], 1);
  }

  #[test]
  fn a_batch_budget_stops_the_work_and_says_so() {
    let reg = registry();
    let src = source(&[(oid(1), b"aaaa"), (oid(2), b"bbbb")]);
    let facts = vec![
      BlobFact {
        oid: oid(1),
        size: 4,
      },
      BlobFact {
        oid: oid(2),
        size: 4,
      },
    ];
    let budget = IngestBudget {
      max_blobs_per_batch: 1,
      ..IngestBudget::default()
    };
    let report = reg.ingest(&src, &facts, &budget, |_, _| Ok(())).unwrap();
    assert_eq!(report.newly_classified, 1);
    assert!(
      report.budget_exhausted,
      "a caller that treated this as done would leave a half-indexed corpus"
    );
  }

  #[test]
  fn indexed_is_separate_from_classified() {
    // An interned, text-classified blob with no postings yet is an index gap,
    // which the coverage contract has to be able to report as distinct from a
    // binary exclusion.
    let reg = registry();
    let src = source(&[(oid(1), b"needle\n")]);
    let facts = vec![BlobFact {
      oid: oid(1),
      size: 7,
    }];
    reg
      .ingest(&src, &facts, &IngestBudget::default(), |_, _| Ok(()))
      .unwrap();
    let record = reg.record_for_oid(&oid(1)).unwrap().unwrap();
    assert_eq!(record.class, Some(ContentClass::Text));
    assert!(!record.indexed);

    reg.mark_indexed(&[record.key]).unwrap();
    assert!(reg.record_for_oid(&oid(1)).unwrap().unwrap().indexed);
  }

  #[test]
  fn keys_are_scoped_to_one_repository() {
    // A bitmap is a set of keys. If key 0 meant different blobs in two
    // repositories, a manifest that crossed one would silently match the wrong
    // content.
    let store = Arc::new(SearchStore::open_in_memory().unwrap());
    let a = BlobRegistry::new(
      Arc::clone(&store),
      RepositoryId::parse("r-a").unwrap(),
      CorpusPolicy::default(),
    );
    let b = BlobRegistry::new(
      Arc::clone(&store),
      RepositoryId::parse("r-b").unwrap(),
      CorpusPolicy::default(),
    );
    let facts = vec![BlobFact {
      oid: oid(1),
      size: 1,
    }];
    assert_eq!(a.intern(&facts).unwrap(), vec![0]);
    assert_eq!(b.intern(&facts).unwrap(), vec![0]);
    assert_eq!(a.len().unwrap(), 1);
    assert_eq!(b.len().unwrap(), 1);
  }

  #[test]
  fn records_for_keys_survives_more_keys_than_sqlite_takes_parameters() {
    let reg = registry();
    let facts: Vec<BlobFact> = (0..600)
      .map(|i| BlobFact {
        oid: ObjectId::from_raw(HashAlgorithm::Sha1, &{
          let mut raw = [0u8; 20];
          raw[0] = (i % 256) as u8;
          raw[1] = (i / 256) as u8;
          raw
        })
        .unwrap(),
        size: 1,
      })
      .collect();
    let keys = reg.intern(&facts).unwrap();
    let records = reg.records_for_keys(&keys).unwrap();
    assert_eq!(records.len(), 600);
  }
}
