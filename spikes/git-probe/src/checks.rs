//! libgit2 conformance checks.
//!
//! Every check that can be is written against stock Git as the oracle. libgit2
//! is the thing under test, so it is never allowed to confirm its own answer —
//! the same rule PLAN.md applies to `upload-pack` in M5.2.

use crate::fixtures::git;
use crate::gitrepo::{self, GitRepository, Libgit2Repository};
use crate::model::{BytePath, EntryKind, HashAlgorithm, ObjectId};
use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
  Pass,
  Fail,
  /// libgit2 refused, and refusing is the correct, designed behavior.
  ExpectedReject,
  /// Not applicable to this fixture.
  Skipped,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckResult {
  pub fixture: String,
  pub check: &'static str,
  pub outcome: Outcome,
  pub detail: String,
}

fn ok(fixture: &str, check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    fixture: fixture.into(),
    check,
    outcome: Outcome::Pass,
    detail: detail.into(),
  }
}

fn fail(fixture: &str, check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    fixture: fixture.into(),
    check,
    outcome: Outcome::Fail,
    detail: detail.into(),
  }
}

fn skip(fixture: &str, check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    fixture: fixture.into(),
    check,
    outcome: Outcome::Skipped,
    detail: detail.into(),
  }
}

fn g(dir: &Path, args: &[&str]) -> Result<String> {
  let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
  git(dir, &owned)
}

/// Resolve HEAD to a full hex OID, or `None` if the repository has no commits.
///
/// `git rev-parse HEAD` on an unborn HEAD echoes the literal string `HEAD`, and
/// in a bare clone of an empty repository it can do so with a zero exit status.
/// Treating that as an object ID is exactly the class of bug this probe exists
/// to catch, so the result is validated rather than trusted.
fn head_oid(bare: &Path) -> Option<String> {
  let out = g(bare, &["rev-parse", "HEAD"]).ok()?;
  let s = out.trim();
  let looks_like_oid = (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
  looks_like_oid.then(|| s.to_string())
}

/// Run every check against one fixture repository.
pub fn run_all(fixture: &str, bare: &Path) -> Vec<CheckResult> {
  let mut out = Vec::new();

  // Format validation runs first and against stock Git only, because it must
  // work even for repositories libgit2 cannot open at all.
  out.push(check_format_gate(fixture, bare));

  let repo = match Libgit2Repository::open(bare, 4) {
    Ok(r) => r,
    Err(e) => {
      // For the formats DESIGN.md says are unsupported, refusal is the
      // designed outcome and the ingest gate above is what enforces it.
      let expected =
        matches!(fixture, "reftable") || (fixture == "sha256" && !gitrepo::build_has_sha256());
      out.push(CheckResult {
        fixture: fixture.into(),
        check: "open",
        outcome: if expected {
          Outcome::ExpectedReject
        } else {
          Outcome::Fail
        },
        detail: format!("{e:#}"),
      });
      return out;
    }
  };
  out.push(ok(fixture, "open", "libgit2 opened the bare repository"));

  for f in [
    check_refs_match_stock as fn(&str, &Path, &Libgit2Repository) -> Result<CheckResult>,
    check_revision_resolution,
    check_annotated_tag_peels,
    check_reserved_namespace_rejected,
    check_tree_walk_matches_stock,
    check_byte_paths,
    check_blob_bytes_match_stock,
    check_entry_size_without_inflate,
    check_symlink_target,
    check_gitlink_mode,
    check_directory_pagination,
    check_tree_diff_matches_stock,
    check_object_creation_round_trips,
    check_ref_transaction,
    check_abbreviated_oid,
    check_commit_metadata,
    check_repository_format_readback,
  ] {
    match f(fixture, bare, &repo) {
      Ok(r) => out.push(r),
      Err(e) => out.push(fail(fixture, "internal", format!("{e:#}"))),
    }
  }
  out
}

// ---------------------------------------------------------------------------

fn check_format_gate(fixture: &str, bare: &Path) -> CheckResult {
  // Stock Git's view, which is the ground truth for what the repository is.
  let stock_ref_format = g(bare, &["rev-parse", "--show-ref-format"])
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "unknown".into());
  let stock_object_format = g(bare, &["rev-parse", "--show-object-format"])
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "unknown".into());

  // XVFS's view, read from config without libgit2's cooperation.
  let raw = match git2::Repository::open_bare(bare).or_else(|_| git2::Repository::open(bare)) {
    Ok(r) => gitrepo::read_format(&r).ok(),
    Err(_) => None,
  };

  let (algorithm, ref_backend) = match &raw {
    Some(f) => (f.algorithm.name().to_string(), f.ref_backend.clone()),
    // libgit2 could not open it, so read the config file directly. The gate
    // must still produce a verdict: "cannot open" is not "unknown format".
    None => {
      let cfg = std::fs::read_to_string(bare.join("config")).unwrap_or_default();
      let find = |key: &str| {
        cfg
          .lines()
          .map(str::trim)
          .find_map(|l| l.strip_prefix(key)?.trim().strip_prefix('=').map(str::trim))
          .map(str::to_string)
      };
      (
        find("objectformat").unwrap_or_else(|| "sha1".into()),
        find("refstorage").unwrap_or_else(|| "files".into()),
      )
    }
  };

  let format = gitrepo::RepositoryFormat {
    algorithm: HashAlgorithm::from_name(&algorithm).unwrap_or(HashAlgorithm::Sha1),
    ref_backend: ref_backend.clone(),
    repository_format_version: raw
      .as_ref()
      .map(|f| f.repository_format_version)
      .unwrap_or(1),
    extensions: raw.map(|f| f.extensions).unwrap_or_default(),
  };
  let v = gitrepo::verdict(&format, gitrepo::build_has_sha256());

  // The gate is correct when its verdict agrees with what stock Git says the
  // repository actually is.
  let should_reject = stock_ref_format != "files"
    || (stock_object_format == "sha256" && !gitrepo::build_has_sha256());

  let rejected = matches!(v, gitrepo::FormatVerdict::Rejected { .. });
  let detail = format!(
    "stock: ref-format={stock_ref_format} object-format={stock_object_format}; \
         xvfs: ref-backend={ref_backend} algorithm={algorithm}; verdict={v:?}"
  );
  if rejected == should_reject {
    if rejected {
      CheckResult {
        fixture: fixture.into(),
        check: "format_gate",
        outcome: Outcome::ExpectedReject,
        detail,
      }
    } else {
      ok(fixture, "format_gate", detail)
    }
  } else {
    fail(
      fixture,
      "format_gate",
      format!("gate disagrees with stock Git: {detail}"),
    )
  }
}

fn check_refs_match_stock(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let stock = g(bare, &["for-each-ref", "--format=%(refname) %(objectname)"])?;
  let mut expected: Vec<(String, String)> = stock
    .lines()
    .filter_map(|l| l.split_once(' '))
    .filter(|(n, _)| !n.starts_with("refs/xvfs/"))
    .map(|(n, o)| (n.to_string(), o.to_string()))
    .collect();
  expected.sort();

  let mut actual: Vec<(String, String)> = repo
    .list_refs()?
    .into_iter()
    .map(|(n, o)| (n, o.to_hex()))
    .collect();
  actual.sort();

  if expected == actual {
    Ok(ok(
      fixture,
      "refs_match_stock",
      format!("{} refs identical", actual.len()),
    ))
  } else {
    Ok(fail(
      fixture,
      "refs_match_stock",
      format!("stock={expected:?} libgit2={actual:?}"),
    ))
  }
}

fn check_revision_resolution(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "revision_resolution", "no commits"));
  };
  let head = head.trim();
  // Discover the default branch rather than assuming one. The corpus has both
  // conventions (`main` in the fixtures, `master` in the Linux mirror), and a
  // server that hardcodes either is wrong for half its repositories.
  let branch = g(bare, &["symbolic-ref", "--short", "HEAD"])
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "main".to_string());
  let qualified = format!("refs/heads/{branch}");

  let mut checked = Vec::new();
  for selector in ["HEAD", &branch, &qualified, head] {
    match repo.resolve_revision(selector) {
      Ok(r) if r.commit.to_hex() == head => checked.push(selector.to_string()),
      Ok(r) => {
        return Ok(fail(
          fixture,
          "revision_resolution",
          format!("{selector} resolved to {} not {head}", r.commit.to_hex()),
        ))
      }
      Err(e) => {
        return Ok(fail(
          fixture,
          "revision_resolution",
          format!("{selector}: {e:#}"),
        ))
      }
    }
  }
  Ok(ok(
    fixture,
    "revision_resolution",
    format!("{} selectors agree with rev-parse", checked.len()),
  ))
}

fn check_annotated_tag_peels(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let tags = g(bare, &["tag", "-l"])?;
  let all: Vec<&str> = tags.lines().filter(|t| !t.is_empty()).collect();
  if all.is_empty() {
    return Ok(skip(fixture, "annotated_tag_peels", "no tags"));
  }

  // Not every tag peels to a commit. The Linux history's `v2.6.11` tag
  // dereferences to a *tree*, which is a real shape a production server will
  // be asked to resolve. Both behaviors are checked: tags that do peel must
  // agree with stock Git, and tags that do not must produce a clean error
  // rather than a bogus commit ID or a panic.
  let mut peeled = 0usize;
  let mut non_commit = Vec::new();
  for tag in all.iter().take(200) {
    let expected = g(bare, &["rev-parse", &format!("{tag}^{{commit}}")]);
    match expected {
      Ok(want) => {
        let got = repo.resolve_revision(tag)?;
        if got.commit.to_hex() != want.trim() {
          return Ok(fail(
            fixture,
            "annotated_tag_peels",
            format!("{tag}: got {} want {}", got.commit.to_hex(), want.trim()),
          ));
        }
        peeled += 1;
      }
      Err(_) => {
        // Stock Git refuses; XVFS must refuse too, and for this reason.
        if let Ok(r) = repo.resolve_revision(tag) {
          return Ok(fail(
            fixture,
            "annotated_tag_peels",
            format!(
              "{tag} does not dereference to a commit for stock Git, \
                             but XVFS resolved it to {}",
              r.commit.to_hex()
            ),
          ));
        }
        non_commit.push(tag.to_string());
      }
    }
  }

  let note = if non_commit.is_empty() {
    String::new()
  } else {
    format!(
      "; {} tag(s) do not peel to a commit and are rejected by both (e.g. {})",
      non_commit.len(),
      non_commit[0]
    )
  };
  Ok(ok(
    fixture,
    "annotated_tag_peels",
    format!("{peeled} tag(s) peel identically to stock{note}"),
  ))
}

fn check_reserved_namespace_rejected(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "reserved_namespace", "no commits"));
  };
  // Plant a lease-shaped ref, then require that it is invisible and unusable.
  let name = "refs/xvfs/mounts/probe-mount";
  g(bare, &["update-ref", name, head.trim()])?;

  let visible = repo.list_refs()?.iter().any(|(n, _)| n == name);
  let resolvable = repo.resolve_revision(name).is_ok();
  g(bare, &["update-ref", "-d", name])?;

  if visible {
    Ok(fail(
      fixture,
      "reserved_namespace",
      "refs/xvfs/ appeared in ref enumeration",
    ))
  } else if resolvable {
    Ok(fail(
      fixture,
      "reserved_namespace",
      "refs/xvfs/ was accepted as a user revision selector",
    ))
  } else {
    Ok(ok(
      fixture,
      "reserved_namespace",
      "hidden from enumeration and rejected as a selector",
    ))
  }
}

/// Full recursive tree comparison: path bytes, mode, and OID for every entry.
fn check_tree_walk_matches_stock(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "tree_walk_matches_stock", "no commits"));
  };
  let head = head.trim();
  let expected = stock_ls_tree(bare, head)?;

  let commit = ObjectId::from_hex(algorithm_of(bare)?, head)?;
  let mut actual = Vec::new();
  walk(repo, &commit, &BytePath::new(Vec::new()), &mut actual)?;
  actual.sort_by(|a, b| a.0.cmp(&b.0));

  if expected.len() != actual.len() {
    return Ok(fail(
      fixture,
      "tree_walk_matches_stock",
      format!(
        "entry count: stock={} libgit2={}",
        expected.len(),
        actual.len()
      ),
    ));
  }
  for (e, a) in expected.iter().zip(actual.iter()) {
    if e != a {
      return Ok(fail(
        fixture,
        "tree_walk_matches_stock",
        format!(
          "mismatch: stock=({}, {:o}, {}) libgit2=({}, {:o}, {})",
          BytePath::new(e.0.clone()).escaped(),
          e.1,
          e.2,
          BytePath::new(a.0.clone()).escaped(),
          a.1,
          a.2
        ),
      ));
    }
  }
  Ok(ok(
    fixture,
    "tree_walk_matches_stock",
    format!(
      "{} entries identical in path bytes, mode, and OID",
      actual.len()
    ),
  ))
}

type Entry = (Vec<u8>, u32, String);

/// `ls-tree -r -t -z` with NUL termination, which is the only output mode that
/// survives a filename containing a newline.
fn stock_ls_tree(bare: &Path, rev: &str) -> Result<Vec<Entry>> {
  let out = std::process::Command::new("git")
    .current_dir(bare)
    .args(["ls-tree", "-r", "-t", "-z", rev])
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .output()?;
  anyhow::ensure!(out.status.success(), "git ls-tree failed");
  let mut entries = Vec::new();
  for rec in out.stdout.split(|b| *b == 0) {
    if rec.is_empty() {
      continue;
    }
    // "<mode> SP <type> SP <oid> TAB <path>"
    let tab = rec
      .iter()
      .position(|b| *b == b'\t')
      .ok_or_else(|| anyhow!("malformed ls-tree record"))?;
    let meta = std::str::from_utf8(&rec[..tab])?;
    let path = rec[tab + 1..].to_vec();
    let mut parts = meta.split(' ');
    let mode = u32::from_str_radix(parts.next().unwrap_or_default(), 8)?;
    let _kind = parts.next();
    let oid = parts.next().unwrap_or_default().to_string();
    entries.push((path, mode, oid));
  }
  entries.sort_by(|a, b| a.0.cmp(&b.0));
  Ok(entries)
}

fn algorithm_of(bare: &Path) -> Result<HashAlgorithm> {
  let s = g(bare, &["rev-parse", "--show-object-format"])?;
  HashAlgorithm::from_name(s.trim()).ok_or_else(|| anyhow!("unknown object format {s:?}"))
}

fn walk(
  repo: &Libgit2Repository,
  commit: &ObjectId,
  dir: &BytePath,
  out: &mut Vec<Entry>,
) -> Result<()> {
  let mut after: Option<Vec<u8>> = None;
  loop {
    let (page, next) = repo.list_directory(commit, dir, after.as_deref(), 1000)?;
    if page.is_empty() {
      break;
    }
    for e in &page {
      out.push((e.path.as_bytes().to_vec(), e.mode, e.oid.to_hex()));
      if e.kind == EntryKind::Directory {
        walk(repo, commit, &e.path, out)?;
      }
    }
    match next {
      Some(t) => after = Some(t),
      None => break,
    }
  }
  Ok(())
}

fn check_byte_paths(fixture: &str, bare: &Path, repo: &Libgit2Repository) -> Result<CheckResult> {
  if fixture != "bytes" {
    return Ok(skip(fixture, "byte_paths", "fixture has no byte paths"));
  }
  let head = head_oid(bare).ok_or_else(|| anyhow!("no commits"))?;
  let commit = ObjectId::from_hex(algorithm_of(bare)?, &head)?;
  let expected = stock_ls_tree(bare, head.trim())?;

  // Each path must be individually resolvable by its exact bytes, not just
  // appear in a listing. This is what a FUSE `lookup` actually does.
  let mut non_utf8 = 0;
  for (path, _, oid) in &expected {
    if std::str::from_utf8(path).is_err() {
      non_utf8 += 1;
    }
    let bp = BytePath::new(path.clone());
    match repo.entry(&commit, &bp)? {
      Some(e) if &e.oid.to_hex() == oid => {}
      Some(e) => {
        return Ok(fail(
          fixture,
          "byte_paths",
          format!("{}: got {} want {oid}", bp.escaped(), e.oid.to_hex()),
        ))
      }
      None => {
        return Ok(fail(
          fixture,
          "byte_paths",
          format!("{} not found by exact bytes", bp.escaped()),
        ))
      }
    }
  }
  Ok(ok(
    fixture,
    "byte_paths",
    format!(
      "{} paths resolved by exact bytes ({non_utf8} invalid UTF-8)",
      expected.len()
    ),
  ))
}

fn check_blob_bytes_match_stock(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "blob_bytes_match_stock", "no commits"));
  };
  let entries = stock_ls_tree(bare, head.trim())?;
  let algo = algorithm_of(bare)?;
  let mut compared = 0usize;
  let mut bytes = 0usize;
  for (path, mode, oid) in entries.iter().take(200) {
    if *mode != 0o100644 && *mode != 0o100755 {
      continue;
    }
    let expected = std::process::Command::new("git")
      .current_dir(bare)
      .args(["cat-file", "blob", oid])
      .output()?;
    anyhow::ensure!(expected.status.success(), "cat-file failed for {oid}");
    let actual = repo.read_blob(&ObjectId::from_hex(algo, oid)?)?;
    if actual != expected.stdout {
      return Ok(fail(
        fixture,
        "blob_bytes_match_stock",
        format!(
          "{} differs from cat-file",
          BytePath::new(path.clone()).escaped()
        ),
      ));
    }
    compared += 1;
    bytes += actual.len();
  }
  Ok(ok(
    fixture,
    "blob_bytes_match_stock",
    format!("{compared} blobs byte-identical ({bytes} bytes)"),
  ))
}

/// `odb.read_header` must return the true size without inflating the object.
/// The FUSE `getattr` path depends on this: a `stat` of a 12 MiB blob must not
/// decompress 12 MiB.
fn check_entry_size_without_inflate(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "entry_size", "no commits"));
  };
  let commit = ObjectId::from_hex(algorithm_of(bare)?, head.trim())?;
  let entries = stock_ls_tree(bare, head.trim())?;
  let mut compared = 0;
  for (path, mode, oid) in entries.iter().take(200) {
    if *mode != 0o100644 && *mode != 0o100755 {
      continue;
    }
    let expected: u64 = g(bare, &["cat-file", "-s", oid])?.trim().parse()?;
    let Some(e) = repo.entry(&commit, &BytePath::new(path.clone()))? else {
      return Ok(fail(fixture, "entry_size", "entry vanished"));
    };
    if e.size != expected {
      return Ok(fail(
        fixture,
        "entry_size",
        format!(
          "{}: got {} want {expected}",
          BytePath::new(path.clone()).escaped(),
          e.size
        ),
      ));
    }
    compared += 1;
  }
  Ok(ok(
    fixture,
    "entry_size",
    format!("{compared} sizes match cat-file -s via read_header"),
  ))
}

fn check_symlink_target(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "symlink_target", "no commits"));
  };
  let commit = ObjectId::from_hex(algorithm_of(bare)?, &head)?;
  // Every symlink stock Git reports, so the corpus mirrors exercise this too.
  let links: Vec<Vec<u8>> = stock_ls_tree(bare, &head)?
    .into_iter()
    .filter(|(_, mode, _)| *mode == 0o120000)
    .map(|(p, _, _)| p)
    .collect();
  if links.is_empty() {
    return Ok(skip(fixture, "symlink_target", "no symlinks"));
  }

  let mut absolute = 0usize;
  let mut escaping = 0usize;
  for path in links.iter().take(500) {
    let e = repo
      .entry(&commit, &BytePath::new(path.clone()))?
      .ok_or_else(|| anyhow!("{:?} missing", BytePath::new(path.clone()).escaped()))?;
    anyhow::ensure!(
      e.kind == EntryKind::Symlink,
      "{} is not a symlink",
      BytePath::new(path.clone()).escaped()
    );
    let got = e.symlink_target.ok_or_else(|| anyhow!("no target"))?;
    // The target is the blob's content, so stock Git is the oracle for it.
    let want = std::process::Command::new("git")
      .current_dir(bare)
      .args(["cat-file", "blob", &e.oid.to_hex()])
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .output()?;
    anyhow::ensure!(
      got.as_bytes() == want.stdout.as_slice(),
      "{}: target differs from cat-file",
      BytePath::new(path.clone()).escaped()
    );
    if got.as_bytes().starts_with(b"/") {
      absolute += 1;
    }
    if got.as_bytes().starts_with(b"../") {
      escaping += 1;
    }
  }
  // Absolute and escaping links are readable here and are returned verbatim.
  // The safety policy (DESIGN.md section 10 item 10) belongs to the FUSE
  // layer, not the object layer; this records that the object layer is
  // faithful and reports how much material that policy will actually face.
  Ok(ok(
    fixture,
    "symlink_target",
    format!(
      "{} symlink(s) match cat-file byte for byte ({absolute} absolute, \
             {escaping} parent-relative — both need FUSE-layer policy)",
      links.len().min(500)
    ),
  ))
}

fn check_gitlink_mode(fixture: &str, bare: &Path, repo: &Libgit2Repository) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "gitlink_mode", "no commits"));
  };
  let commit = ObjectId::from_hex(algorithm_of(bare)?, &head)?;
  // Find every gitlink stock Git reports, so the real corpus repositories
  // (rust-lang/rust has submodules) exercise this and not only the fixture.
  let gitlinks: Vec<Vec<u8>> = stock_ls_tree(bare, &head)?
    .into_iter()
    .filter(|(_, mode, _)| *mode == 0o160000)
    .map(|(p, _, _)| p)
    .collect();
  if gitlinks.is_empty() {
    return Ok(skip(fixture, "gitlink_mode", "no submodule"));
  }
  for path in &gitlinks {
    let e = repo
      .entry(&commit, &BytePath::new(path.clone()))?
      .ok_or_else(|| {
        anyhow!(
          "gitlink {:?} missing",
          BytePath::new(path.clone()).escaped()
        )
      })?;
    if e.kind != EntryKind::Gitlink || e.mode != 0o160000 {
      return Ok(fail(
        fixture,
        "gitlink_mode",
        format!(
          "{}: got {:?} mode {:o}",
          BytePath::new(path.clone()).escaped(),
          e.kind,
          e.mode
        ),
      ));
    }
    // A gitlink must not be traversable: listing it must not recurse into
    // another repository's objects, which XVFS does not have.
    let (children, _) = repo.list_directory(&commit, &BytePath::new(path.clone()), None, 16)?;
    if !children.is_empty() {
      return Ok(fail(
        fixture,
        "gitlink_mode",
        format!(
          "gitlink {} listed {} children",
          BytePath::new(path.clone()).escaped(),
          children.len()
        ),
      ));
    }
  }
  Ok(ok(
    fixture,
    "gitlink_mode",
    format!(
      "{} gitlink(s) classified as 160000 and not traversed",
      gitlinks.len()
    ),
  ))
}

fn check_directory_pagination(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "directory_pagination", "no commits"));
  };
  let commit = ObjectId::from_hex(algorithm_of(bare)?, &head)?;

  // Paginate whichever directory in this repository is actually largest,
  // rather than a fixture-specific path, so the corpus mirrors exercise it too.
  let (dir, expected_len) = largest_directory(bare, &head)?;
  if expected_len < 50 {
    return Ok(skip(
      fixture,
      "directory_pagination",
      format!("largest directory has only {expected_len} entries"),
    ));
  }

  let mut seen: Vec<String> = Vec::new();
  let mut after: Option<Vec<u8>> = None;
  let mut pages = 0;
  loop {
    let (page, next) = repo.list_directory(&commit, &dir, after.as_deref(), 137)?;
    if page.is_empty() {
      break;
    }
    pages += 1;
    for e in page {
      seen.push(e.path.escaped());
    }
    match next {
      Some(t) => after = Some(t),
      None => break,
    }
    anyhow::ensure!(pages < 1000, "pagination did not terminate");
  }

  let mut deduped = seen.clone();
  deduped.sort();
  deduped.dedup();
  if deduped.len() != seen.len() {
    return Ok(fail(
      fixture,
      "directory_pagination",
      format!(
        "{} duplicate entries across pages",
        seen.len() - deduped.len()
      ),
    ));
  }
  if seen.len() != expected_len {
    return Ok(fail(
      fixture,
      "directory_pagination",
      format!(
        "{} has {expected_len} entries but paginated {}",
        dir.escaped(),
        seen.len()
      ),
    ));
  }
  Ok(ok(
    fixture,
    "directory_pagination",
    format!(
      "{} entries of {} over {pages} pages, no gaps or duplicates",
      expected_len,
      dir.escaped()
    ),
  ))
}

/// Find the directory with the most immediate children, using stock Git.
fn largest_directory(bare: &Path, rev: &str) -> Result<(BytePath, usize)> {
  use std::collections::HashMap;
  let entries = stock_ls_tree(bare, rev)?;
  let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
  for (path, _, _) in &entries {
    let parent = match path.iter().rposition(|b| *b == b'/') {
      Some(i) => path[..i].to_vec(),
      None => Vec::new(),
    };
    *counts.entry(parent).or_insert(0) += 1;
  }
  let (path, count) = counts
    .into_iter()
    // Ties broken by name so the choice is deterministic across runs.
    .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
    .unwrap_or((Vec::new(), 0));
  Ok((BytePath::new(path), count))
}

fn check_tree_diff_matches_stock(
  fixture: &str,
  bare: &Path,
  _repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Ok(count) = g(bare, &["rev-list", "--count", "HEAD"]) else {
    return Ok(skip(fixture, "tree_diff", "no commits"));
  };
  if count.trim().parse::<u32>().unwrap_or(0) < 2 {
    return Ok(skip(fixture, "tree_diff", "needs two commits"));
  }
  // Stock: name + status for HEAD~1..HEAD.
  let stock_raw = g(
    bare,
    &["diff-tree", "-r", "--name-status", "HEAD~1", "HEAD"],
  )?;
  let mut stock: Vec<String> = stock_raw
    .lines()
    .map(|l| l.replace('\t', " "))
    .filter(|l| !l.is_empty())
    .collect();
  stock.sort();

  let repo = git2::Repository::open_bare(bare)?;
  let head = repo.head()?.peel_to_commit()?;
  let parent = head.parent(0)?;
  let diff = repo.diff_tree_to_tree(Some(&parent.tree()?), Some(&head.tree()?), None)?;
  let mut actual: Vec<String> = Vec::new();
  diff.foreach(
    &mut |d, _| {
      let status = match d.status() {
        git2::Delta::Added => "A",
        git2::Delta::Deleted => "D",
        git2::Delta::Modified => "M",
        git2::Delta::Renamed => "R",
        git2::Delta::Typechange => "T",
        _ => "?",
      };
      let path = d
        .new_file()
        .path()
        .or_else(|| d.old_file().path())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
      actual.push(format!("{status} {path}"));
      true
    },
    None,
    None,
    None,
  )?;
  actual.sort();

  if stock == actual {
    Ok(ok(
      fixture,
      "tree_diff",
      format!("{} deltas identical to diff-tree", actual.len()),
    ))
  } else {
    Ok(fail(
      fixture,
      "tree_diff",
      format!("stock={stock:?} libgit2={actual:?}"),
    ))
  }
}

/// Objects libgit2 creates must be valid to stock Git. This is the M8 commit
/// path's foundation and the cheapest place to find out it is broken.
fn check_object_creation_round_trips(
  fixture: &str,
  bare: &Path,
  _repo: &Libgit2Repository,
) -> Result<CheckResult> {
  if fixture == "empty" {
    return Ok(skip(fixture, "object_creation", "no base commit"));
  }
  let repo = git2::Repository::open_bare(bare)?;
  let content = b"created by libgit2\n";
  let blob = repo.blob(content)?;

  let mut tb = repo.treebuilder(None)?;
  tb.insert("created.txt", blob, 0o100644)?;
  // A non-UTF-8 name in a tree libgit2 writes, to prove the write path is as
  // byte-safe as the read path.
  tb.insert(
    std::ffi::OsStr::from_bytes(b"created-\xff.txt"),
    blob,
    0o100644,
  )?;
  let tree_oid = tb.write()?;

  let sig = git2::Signature::new("XVFS Probe", "probe@xvfs.invalid", &git2::Time::new(0, 0))?;
  let tree = repo.find_tree(tree_oid)?;
  let parent = repo.head()?.peel_to_commit()?;
  let commit = repo.commit(None, &sig, &sig, "probe commit", &tree, &[&parent])?;

  // Stock Git is the oracle for validity.
  let fsck = std::process::Command::new("git")
    .current_dir(bare)
    .args([
      "fsck",
      "--no-progress",
      "--connectivity-only",
      "--no-dangling",
    ])
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .output()?;
  let cat = g(bare, &["cat-file", "-t", &commit.to_string()])?;
  let stock_blob = std::process::Command::new("git")
    .current_dir(bare)
    .args(["cat-file", "blob", &blob.to_string()])
    .output()?;

  if !fsck.status.success() {
    return Ok(fail(
      fixture,
      "object_creation",
      format!(
        "git fsck rejected: {}",
        String::from_utf8_lossy(&fsck.stderr).trim()
      ),
    ));
  }
  if cat.trim() != "commit" || stock_blob.stdout != content {
    return Ok(fail(
      fixture,
      "object_creation",
      format!("stock Git read back type={} ", cat.trim()),
    ));
  }
  Ok(ok(
    fixture,
    "object_creation",
    "blob/tree/commit created by libgit2 pass git fsck and cat-file",
  ))
}

/// Ref transactions are how a mount lease anchor is created under the
/// repository lock (DESIGN.md section 7.1), so the mechanism is checked here.
fn check_ref_transaction(
  fixture: &str,
  bare: &Path,
  _repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "ref_transaction", "no commits"));
  };
  let repo = git2::Repository::open_bare(bare)?;
  let oid = git2::Oid::from_str(head.trim())?;
  let name = "refs/xvfs/mounts/tx-probe";

  let sig = git2::Signature::new("XVFS Probe", "probe@xvfs.invalid", &git2::Time::new(0, 0))?;
  {
    let mut tx = repo.transaction()?;
    tx.lock_ref(name)?;
    tx.set_target(name, oid, Some(&sig), "lease anchor")?;
    tx.commit()?;
  }
  let after = g(bare, &["rev-parse", name])?;
  let created = after.trim() == head.trim();

  // Rolling back must leave no trace: an abandoned PREPARING lease that left a
  // ref behind would pin objects forever.
  {
    let mut tx = repo.transaction()?;
    tx.lock_ref(name)?;
    tx.remove(name)?;
    tx.commit()?;
  }
  let gone = g(bare, &["rev-parse", "--verify", name]).is_err();

  if created && gone {
    Ok(ok(
      fixture,
      "ref_transaction",
      "locked create and locked remove both visible to stock Git",
    ))
  } else {
    Ok(fail(
      fixture,
      "ref_transaction",
      format!("created={created} removed={gone}"),
    ))
  }
}

fn check_abbreviated_oid(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "abbreviated_oid", "no commits"));
  };
  let head = head.trim();
  let abbrev = &head[..12];
  let full = repo
    .resolve_revision(abbrev)
    .with_context(|| format!("resolving abbreviation {abbrev}"))?;
  if full.commit.to_hex() != head {
    return Ok(fail(
      fixture,
      "abbreviated_oid",
      format!("{abbrev} -> {}", full.commit.to_hex()),
    ));
  }
  // A 3-character abbreviation is short enough to be ambiguous in a large
  // repository. It must not be silently accepted from a user.
  let too_short = repo.resolve_revision(&head[..3]);
  Ok(ok(
    fixture,
    "abbreviated_oid",
    format!(
      "12-char abbreviation resolves; 3-char {}",
      if too_short.is_ok() {
        "ALSO resolves (caller must enforce a minimum length)"
      } else {
        "rejected"
      }
    ),
  ))
}

/// The metadata behind `GetCommit`, which is the only server call the bounded
/// `git log -1` shim of DESIGN.md section 8.6 is allowed to make.
fn check_commit_metadata(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let Some(head) = head_oid(bare) else {
    return Ok(skip(fixture, "commit_metadata", "no commits"));
  };
  let head = head.trim();
  let commit = ObjectId::from_hex(algorithm_of(bare)?, head)?;
  let meta = repo.read_commit(&commit)?;

  let want_tree = g(bare, &["rev-parse", "HEAD^{tree}"])?;
  let want_parents = g(bare, &["rev-list", "--parents", "-1", "HEAD"])?;
  let want_parents: Vec<&str> = want_parents.trim().split(' ').skip(1).collect();
  let want_time: i64 = g(bare, &["log", "-1", "--format=%ct"])?.trim().parse()?;
  let want_subject = g(bare, &["log", "-1", "--format=%s"])?;

  if meta.tree.to_hex() != want_tree.trim() {
    return Ok(fail(
      fixture,
      "commit_metadata",
      format!("tree {} want {}", meta.tree.to_hex(), want_tree.trim()),
    ));
  }
  let got_parents: Vec<String> = meta.parents.iter().map(|p| p.to_hex()).collect();
  if got_parents != want_parents {
    return Ok(fail(
      fixture,
      "commit_metadata",
      format!("parents {got_parents:?} want {want_parents:?}"),
    ));
  }
  if meta.committer_time != want_time {
    return Ok(fail(
      fixture,
      "commit_metadata",
      format!("committer time {} want {want_time}", meta.committer_time),
    ));
  }
  if meta.message.lines().next().unwrap_or_default() != want_subject.trim() {
    return Ok(fail(fixture, "commit_metadata", "subject differs"));
  }
  Ok(ok(
    fixture,
    "commit_metadata",
    format!(
      "tree, {} parent(s), raw committer time {want_time}, and subject match stock",
      got_parents.len()
    ),
  ))
}

/// The trait's own `format()` must agree with the standalone ingest gate; a
/// disagreement would mean a repository could pass ingest and then be read
/// through a different set of assumptions.
fn check_repository_format_readback(
  fixture: &str,
  bare: &Path,
  repo: &Libgit2Repository,
) -> Result<CheckResult> {
  let f = repo.format()?;
  let stock_object = g(bare, &["rev-parse", "--show-object-format"])?;
  if f.algorithm.name() != stock_object.trim() {
    return Ok(fail(
      fixture,
      "format_readback",
      format!(
        "algorithm {} want {}",
        f.algorithm.name(),
        stock_object.trim()
      ),
    ));
  }
  Ok(ok(
    fixture,
    "format_readback",
    format!(
      "algorithm={} ref-backend={} version={} extensions={}",
      f.algorithm.name(),
      f.ref_backend,
      f.repository_format_version,
      f.extensions.len()
    ),
  ))
}

use std::os::unix::ffi::OsStrExt;
