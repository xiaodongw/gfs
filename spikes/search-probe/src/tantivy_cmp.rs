//! Per-snapshot Tantivy index, for the size/time comparison PLAN.md M0.4 asks
//! for.
//!
//! Built the naive way on purpose — one index per snapshot, no blob-key sharing
//! — because that is the alternative being priced. The custom representation's
//! whole claim is that indexing each blob once and filtering by a snapshot
//! bitmap is cheaper than this when many snapshots are retained, and that claim
//! needs a number on the other side of it.

use anyhow::Result;
use std::path::Path;
use std::time::Instant;
use tantivy::doc;
use tantivy::schema::{Schema, FAST, STORED, STRING, TEXT};

pub fn build(repo_path: &Path, rev: &str) -> Result<()> {
    let repo = git2::Repository::open_bare(repo_path).or_else(|_| git2::Repository::open(repo_path))?;
    let oid = repo.revparse_single(rev)?.peel_to_commit()?.id();

    let mut builder = Schema::builder();
    let path_field = builder.add_text_field("path", STRING | STORED);
    let body = builder.add_text_field("body", TEXT);
    // The fast field DESIGN.md section 6.5 describes, so a snapshot filter could
    // be mapped onto segment-local doc ids.
    let blob_key = builder.add_u64_field("blob_key", FAST | STORED);
    let schema = builder.build();

    let dir = std::env::temp_dir().join(format!("xvfs-tantivy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let index = tantivy::Index::create_in_dir(&dir, schema)?;
    let mut writer: tantivy::IndexWriter = index.writer(512 * 1024 * 1024)?;

    let tree = repo.find_commit(oid)?.tree()?;
    let odb = repo.odb()?;
    let mut files: Vec<(Vec<u8>, git2::Oid)> = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |d, e| {
        if e.filemode() == 0o100644 || e.filemode() == 0o100755 {
            let mut p = d.as_bytes().to_vec();
            p.extend_from_slice(e.name_bytes());
            files.push((p, e.id()));
        }
        git2::TreeWalkResult::Ok
    })?;

    let t = Instant::now();
    let mut indexed = 0usize;
    let mut indexed_bytes = 0u64;
    for (i, (path, oid)) in files.iter().enumerate() {
        let blob = odb.read(*oid)?;
        let content = blob.data();
        // The same corpus policy as the custom index, or the comparison is
        // between different amounts of work.
        if crate::index::classify(content).is_some() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        writer.add_document(doc!(
            path_field => String::from_utf8_lossy(path).into_owned(),
            body => text,
            blob_key => i as u64,
        ))?;
        indexed += 1;
        indexed_bytes += content.len() as u64;
    }
    writer.commit()?;
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;

    let size: u64 = walk_size(&dir)?;
    println!("tantivy per-snapshot index");
    println!("  documents           {indexed}");
    println!("  content indexed     {:.1} MiB", indexed_bytes as f64 / 1048576.0);
    println!("  build time          {build_ms:.0} ms");
    println!("  on-disk size        {:.1} MiB", size as f64 / 1048576.0);
    println!(
        "  index bytes per byte of content: {:.2}",
        size as f64 / indexed_bytes.max(1) as f64
    );
    println!();
    println!("Note: this is per snapshot. N retained snapshots cost N times this,");
    println!("whereas the trigram representation pays once per unique blob and");
    println!("adds only a manifest per snapshot.");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn walk_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?;
        total += if m.is_dir() { walk_size(&e.path())? } else { m.len() };
    }
    Ok(total)
}
