//! Bringing an upstream repository under GFS management.
//!
//! This is what `gfs clone` runs. The pieces it sequences all existed --
//! [`crate::mirror::init_bare`], [`crate::mirror::fetch`],
//! [`crate::catalog::Catalog::create_repository`], [`crate::Registry::activate`]
//! -- but nothing called them together, so a repository could only reach the
//! catalog through the server binary's `--import`, from a path already on disk.
//!
//! # One clone, many views
//!
//! The mirror this creates is *the* clone. A mount is a view onto it, and there
//! may be thousands of views over one mirror -- the same relationship `git
//! worktree` has to a repository, except that a view materializes no working
//! files at all. So ingest is keyed by *upstream URL*, not by caller: asking to
//! clone a URL that is already here is a **sync**, not a second copy.
//!
//! # The identifier is derived, not chosen
//!
//! A repository id has to survive being a directory name and a path component of
//! an HTTP route, so [`RepositoryId`] admits only letters, digits, `.`, `_` and
//! `-` -- no slashes. `https://github.com/pallets/flask.git` therefore cannot be
//! its own id. [`repository_id_for`] folds the host and path into
//! `github.com_pallets_flask`, which keeps two upstreams that differ only by
//! organisation apart -- the collision a bare `flask` would have caused.
//!
//! The *directory* is a separate question with a different answer: `git clone`
//! makes `flask`, so [`directory_for`] does too.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gfs_types::error::GfsError;
use gfs_types::{DisplayName, RepositoryId};

use crate::catalog::repositories::{NewRepository, RepositoryRecord};
use crate::catalog::Catalog;
use crate::mirror;
use crate::registry::Registry;

/// Where ingest puts mirrors, and which Git it runs.
#[derive(Clone, Debug)]
pub struct IngestConfig {
  /// The directory holding one bare mirror per repository.
  pub repos_root: PathBuf,
  pub git_binary: PathBuf,
}

impl IngestConfig {
  pub fn new(repos_root: impl Into<PathBuf>) -> IngestConfig {
    IngestConfig {
      repos_root: repos_root.into(),
      git_binary: PathBuf::from("git"),
    }
  }

  fn mirror_path(&self, id: &RepositoryId) -> PathBuf {
    self.repos_root.join(format!("{id}.git"))
  }
}

/// What one ingest did.
#[derive(Clone, Debug)]
pub struct IngestOutcome {
  pub repository_id: RepositoryId,
  /// False when the repository was already here and this call only synced it.
  pub created: bool,
  /// The upstream's `HEAD` branch, which is what a fresh mount should show.
  pub default_branch: String,
  /// Stock Git's fetch summary, already bounded.
  pub summary: String,
}

/// The identifier GFS knows an upstream URL by.
///
/// Stable across calls, because it is the key that makes a second `gfs clone` of
/// the same URL a sync rather than a duplicate mirror.
pub fn repository_id_for(url: &str) -> Result<RepositoryId, GfsError> {
  let trimmed = url.trim();
  if trimmed.is_empty() {
    return Err(GfsError::invalid("an upstream URL is required"));
  }
  // Everything after the scheme, with `user@host:path` (scp syntax) folded into
  // the same shape as `host/path` so both spellings of one upstream agree.
  let after_scheme = trimmed
    .split_once("://")
    .map(|(_, rest)| rest)
    .unwrap_or(trimmed);
  let after_userinfo = after_scheme
    .rsplit_once('@')
    .map(|(_, rest)| rest)
    .unwrap_or(after_scheme);
  let normalized = after_userinfo.replace(':', "/");
  let body = normalized.trim_matches('/');
  let body = body.strip_suffix(".git").unwrap_or(body);

  let mut id = String::with_capacity(body.len());
  let mut last_was_sep = false;
  for c in body.chars() {
    if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
      id.push(c);
      last_was_sep = false;
    } else if !last_was_sep {
      // Every run of unusable characters collapses to one `_`, so `a//b` and
      // `a/b` cannot become two different identifiers for one upstream.
      id.push('_');
      last_was_sep = true;
    }
  }
  let id = id.trim_matches('_').trim_matches('.');
  if id.is_empty() {
    return Err(GfsError::invalid(format!(
      "cannot derive a repository id from {url:?}"
    )));
  }
  // `..` would be rejected by `RepositoryId::parse` anyway, but it can only
  // arise here from a path like `a/../b`, and silently keeping the `..` would
  // make the error name a character the caller never typed.
  RepositoryId::parse(&id.replace("..", "._"))
}

/// The directory `git clone` would have created for this URL.
pub fn directory_for(url: &str) -> Result<String, GfsError> {
  let trimmed = url.trim().trim_end_matches('/');
  let tail = trimmed
    .rsplit(['/', ':'])
    .find(|s| !s.is_empty())
    .unwrap_or_default();
  let name = tail.strip_suffix(".git").unwrap_or(tail);
  if name.is_empty() || name == "." || name == ".." {
    return Err(GfsError::invalid(format!(
      "cannot derive a directory name from {url:?}; give one explicitly"
    )));
  }
  Ok(name.to_owned())
}

/// Clone an upstream into a mirror, or sync it if it is already here.
///
/// Ordering is deliberate. The mirror is fetched *before* the catalog row is
/// created, so a fetch that fails against an unreachable or unauthorized
/// upstream leaves no half-registered repository behind for the next caller to
/// trip over. Activation comes last because it applies ADR 0001's format gate,
/// and a repository that fails the gate should be refused with a stated reason
/// rather than left `ACTIVE` and failing at the first read.
pub fn ingest(
  catalog: &Arc<Catalog>,
  registry: &Arc<Registry>,
  config: &IngestConfig,
  url: &str,
  credential: Option<&str>,
) -> Result<IngestOutcome, GfsError> {
  let repository_id = repository_id_for(url)?;
  let existing = catalog.get_repository(&repository_id)?;
  let mirror_path = match &existing {
    // An existing repository keeps its recorded path. Recomputing it would
    // silently relocate a mirror that was imported from somewhere else.
    Some(record) => record.repo_path.clone(),
    None => config.mirror_path(&repository_id),
  };

  if existing.is_none() {
    mirror::init_bare(&mirror_path, &config.git_binary)?;
  }
  let outcome = mirror::fetch(&mirror_path, url, credential, &config.git_binary)?;
  if !outcome.diverged.is_empty() {
    // Fork semantics: the sync is fast-forward-only, so pushed work is never
    // overwritten — but the caller deserves to know their branches are no
    // longer following upstream.
    tracing::warn!(
      repository = %repository_id,
      branches = ?outcome.diverged,
      "branches diverged from upstream were left as they are"
    );
  }

  // Asked of the upstream rather than read from the mirror, because a mirror
  // created by `init_bare` has whatever default branch this Git version picked
  // and that need not be the upstream's.
  let default_branch = mirror::default_branch(url, credential, &config.git_binary)?;
  mirror::set_head(&mirror_path, &default_branch, &config.git_binary)?;

  // The commit-graph is a gateway artifact the projection serves to every mount
  // (ADR 0009): without it a history walk reads commit objects out of the pack,
  // and `--changed-paths` Bloom filters are what let `git log -- <path>` skip
  // commits without loading their trees. Written per sync so it covers what the
  // fetch brought; a failure is logged and not fatal, because a mirror without
  // a commit-graph is slower, not wrong.
  if let Err(e) = mirror::write_commit_graph(&mirror_path, &config.git_binary) {
    tracing::warn!(repository = %repository_id, "commit-graph write failed: {e}");
  }

  let created = existing.is_none();
  if created {
    let format = gfs_git::read_format(&mirror_path)?;
    catalog.create_repository(&NewRepository {
      repository_id: repository_id.clone(),
      display_name: display_name_for(url, &repository_id)?,
      repo_path: mirror_path.clone(),
      algorithm: format.algorithm,
      upstream_url: Some(url.to_owned()),
      credential_ref: None,
    })?;
  }

  let record: RepositoryRecord = registry.activate(&repository_id)?;

  let mut summary = outcome.summary;
  if !outcome.diverged.is_empty() {
    // Carried in the summary the clone response already has, so a client that
    // shows it needs no new field.
    summary.push_str(&format!(
      "\nleft as they are, diverged from upstream: {}",
      outcome.diverged.join(", ")
    ));
  }
  Ok(IngestOutcome {
    repository_id: record.repository_id,
    created,
    default_branch,
    summary,
  })
}

/// A human-facing name for the repository: the URL, when it fits.
fn display_name_for(url: &str, fallback: &RepositoryId) -> Result<DisplayName, GfsError> {
  DisplayName::parse(url).or_else(|_| DisplayName::parse(fallback.as_str()))
}

/// Ensure a mirror directory is not reachable outside the configured root.
///
/// Not currently reachable -- ids are derived and cannot contain `/` or `..` --
/// but the check is here because the *next* caller of `mirror_path` may take an
/// id from somewhere less careful, and a path escape would let ingest write a
/// bare repository anywhere the server can reach.
pub fn is_inside(root: &Path, candidate: &Path) -> bool {
  let (Ok(root), Ok(candidate)) = (root.canonicalize(), candidate.canonicalize()) else {
    // An un-created path cannot be canonicalized; fall back to a textual check,
    // which is sound for the derived ids this is used with.
    return candidate.starts_with(root);
  };
  candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_id_is_derived_from_the_host_and_path() {
    let id = repository_id_for("https://github.com/pallets/flask.git").unwrap();
    assert_eq!(id.as_str(), "github.com_pallets_flask");
  }

  #[test]
  fn two_organisations_sharing_a_name_do_not_collide() {
    // The failure a bare `flask` would have produced: one mirror serving two
    // different upstreams, with whichever was cloned second silently ignored.
    let a = repository_id_for("https://github.com/pallets/flask.git").unwrap();
    let b = repository_id_for("https://github.com/other/flask.git").unwrap();
    assert_ne!(a.as_str(), b.as_str());
  }

  #[test]
  fn the_same_upstream_spelled_two_ways_gives_one_id() {
    // `git@host:org/repo.git` and `https://host/org/repo.git` are the same
    // repository, and cloning both must not produce two mirrors.
    let scp = repository_id_for("git@github.com:pallets/flask.git").unwrap();
    let https = repository_id_for("https://github.com/pallets/flask.git").unwrap();
    assert_eq!(scp.as_str(), https.as_str());
  }

  #[test]
  fn a_trailing_git_suffix_and_slash_do_not_change_the_id() {
    let bare = repository_id_for("https://github.com/pallets/flask").unwrap();
    let dotgit = repository_id_for("https://github.com/pallets/flask.git/").unwrap();
    assert_eq!(bare.as_str(), dotgit.as_str());
  }

  #[test]
  fn the_directory_is_the_one_git_clone_would_make() {
    for (url, want) in [
      ("https://github.com/pallets/flask.git", "flask"),
      ("https://github.com/pallets/flask", "flask"),
      ("git@github.com:pallets/flask.git", "flask"),
      ("file:///srv/fixtures/basic.git", "basic"),
      ("/srv/fixtures/basic.git/", "basic"),
    ] {
      assert_eq!(directory_for(url).unwrap(), want, "{url}");
    }
  }

  #[test]
  fn a_host_only_url_is_named_after_the_host_the_way_git_does() {
    // `git clone https://example.com/` announces "Cloning into 'example.com'"
    // and only then fails on the fetch. Refusing earlier here would reject a
    // URL that stock Git accepts as far as naming, and the fetch failure is the
    // better error anyway -- it says the repository is not there.
    assert_eq!(
      directory_for("https://example.com/").unwrap(),
      "example.com"
    );
  }

  #[test]
  fn a_url_with_no_usable_name_at_all_is_refused() {
    assert!(directory_for("/").is_err());
    assert!(directory_for("").is_err());
    assert!(repository_id_for("   ").is_err());
  }

  #[test]
  fn a_derived_id_is_always_a_legal_repository_id() {
    // The property that matters: whatever the URL, the id either parses or the
    // call fails -- it never produces something that breaks a route or a path.
    for url in [
      "https://github.com/a/b.git",
      "https://ex.com/../../etc/passwd",
      "ssh://git@h/x/y",
      "https://ex.com/a b/c",
      "https://ex.com/.hidden",
    ] {
      if let Ok(id) = repository_id_for(url) {
        assert!(RepositoryId::parse(id.as_str()).is_ok(), "{url} -> {id}");
      }
    }
  }
}
