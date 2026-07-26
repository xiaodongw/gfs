//! Repository-format detection and the ingest gate.
//!
//! ADR 0001 narrowed the supported boundary to the `files` ref backend and SHA-1
//! objects, and requires rejection at *mirror creation* rather than a partial view
//! at serve time. Two details of how that is done matter:
//!
//! * the gate reads `config` directly instead of asking libgit2 to refuse,
//!   because it must produce a verdict even for a repository libgit2 cannot open
//!   at all -- a `reftable` mirror fails `Repository::open` with "unsupported
//!   extension name extensions.refstorage", which is a verdict the operator needs
//!   to see stated rather than a generic open failure;
//! * an unrecognized `extensions.*` is a rejection, not a warning. An unknown
//!   extension means the on-disk meaning is unknown, and serving a repository
//!   whose format you do not understand is how a partial view happens.

use std::path::Path;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::HashAlgorithm;

/// What a mirror must satisfy before the catalog will serve it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepositoryFormat {
  pub algorithm: HashAlgorithm,
  pub ref_backend: String,
  pub repository_format_version: i64,
  pub extensions: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FormatVerdict {
  Supported,
  /// The mirror must be rejected at creation.
  Rejected {
    reason: String,
  },
}

impl FormatVerdict {
  pub fn is_supported(&self) -> bool {
    matches!(self, FormatVerdict::Supported)
  }

  /// Turn a rejection into the typed error the catalog returns.
  pub fn into_result(self) -> Result<(), XvfsError> {
    match self {
      FormatVerdict::Supported => Ok(()),
      FormatVerdict::Rejected { reason } => Err(XvfsError::new(
        ErrorCode::UnsupportedRepositoryFormat,
        reason,
      )),
    }
  }
}

/// The `extensions.*` keys whose meaning is known.
///
/// `compatobjectformat` and `objectformat` are read and then judged by
/// [`verdict`]; being *known* is not the same as being *supported*.
const KNOWN_EXTENSIONS: &[&str] = &[
  "extensions.objectformat",
  "extensions.refstorage",
  "extensions.noop",
  "extensions.compatobjectformat",
];

/// Read the on-disk format from a repository's config, without opening it as a
/// repository.
///
/// Parsing the config file directly is what lets this work on a repository
/// libgit2 refuses to open. `git2::Config::open` reads a config file as a file; it
/// does not require a valid repository around it.
pub fn read_format(repo_path: &Path) -> Result<RepositoryFormat, XvfsError> {
  // A bare repository keeps `config` at its root; a non-bare one keeps it under
  // `.git`. Both are accepted so a developer can point the catalog at a working
  // clone during local development.
  let candidates = [repo_path.join("config"), repo_path.join(".git/config")];
  let config_path = candidates.iter().find(|p| p.is_file()).ok_or_else(|| {
    XvfsError::new(
      ErrorCode::UnsupportedRepositoryFormat,
      "no Git config found; not a repository",
    )
  })?;

  let cfg = git2::Config::open(config_path).map_err(|e| {
    XvfsError::new(
      ErrorCode::UnsupportedRepositoryFormat,
      format!("unreadable Git config: {}", e.message()),
    )
  })?;

  let version = cfg.get_i64("core.repositoryformatversion").unwrap_or(0);

  let mut extensions = Vec::new();
  if let Ok(mut entries) = cfg.entries(Some("extensions.*")) {
    while let Some(Ok(e)) = entries.next() {
      if let (Some(n), Some(v)) = (e.name(), e.value()) {
        extensions.push((n.to_owned(), v.to_owned()));
      }
    }
  }

  let algorithm = extensions
    .iter()
    .find(|(n, _)| n.eq_ignore_ascii_case("extensions.objectformat"))
    .and_then(|(_, v)| HashAlgorithm::from_name(v))
    // No `extensions.objectformat` means SHA-1, which is Git's default and the
    // only algorithm XVFS serves.
    .unwrap_or(HashAlgorithm::Sha1);

  let ref_backend = extensions
    .iter()
    .find(|(n, _)| n.eq_ignore_ascii_case("extensions.refstorage"))
    .map(|(_, v)| v.clone())
    .unwrap_or_else(|| "files".to_owned());

  Ok(RepositoryFormat {
    algorithm,
    ref_backend,
    repository_format_version: version,
    extensions,
  })
}

/// The ingest gate. Called before a mirror is ever served.
pub fn verdict(format: &RepositoryFormat) -> FormatVerdict {
  if format.ref_backend != "files" {
    return FormatVerdict::Rejected {
      reason: format!(
        "ref backend {:?} is unsupported; the pinned libgit2 reads only the \
         `files` backend (ADR 0001)",
        format.ref_backend
      ),
    };
  }

  if format.algorithm == HashAlgorithm::Sha256 {
    // ADR 0001 measured that this is not merely a build-flag away: `git2` 0.20.4
    // fails to compile against a `GIT_EXPERIMENTAL_SHA256` libgit2 with 75
    // errors, all references to `GIT_OID_RAWSZ`/`GIT_OID_HEXSZ` that
    // `libgit2-sys` gates out. So SHA-256 is unreachable through the safe
    // wrapper, not just experimental, and the runtime probe below reflects the
    // build rather than a policy that could be relaxed by configuration.
    return FormatVerdict::Rejected {
      reason: if build_has_sha256() {
        "SHA-256 object format is out of MVP scope (ADR 0006)".to_owned()
      } else {
        "SHA-256 object format is unreachable through git2-rs; see ADR 0001 and \
         spikes/git-probe/sha256-support-check.sh"
          .to_owned()
      },
    };
  }

  for (name, value) in &format.extensions {
    if !KNOWN_EXTENSIONS
      .iter()
      .any(|k| name.eq_ignore_ascii_case(k))
    {
      return FormatVerdict::Rejected {
        reason: format!(
          "unrecognized repository extension {name}={value}; an unknown extension \
           means an unknown on-disk meaning"
        ),
      };
    }
  }

  // `repositoryformatversion` above 1 is a format this Git does not define.
  // Version 1 is required for any `extensions.*` to be honoured at all.
  if format.repository_format_version > 1 {
    return FormatVerdict::Rejected {
      reason: format!(
        "core.repositoryformatversion {} is newer than this build understands",
        format.repository_format_version
      ),
    };
  }

  FormatVerdict::Supported
}

/// Read the format and apply the gate in one step.
pub fn check(repo_path: &Path) -> Result<RepositoryFormat, XvfsError> {
  let format = read_format(repo_path)?;
  verdict(&format).into_result()?;
  Ok(format)
}

/// Whether the linked libgit2 has experimental SHA-256 support compiled in.
///
/// Deliberately a runtime probe rather than `cfg!(libgit2_experimental_sha256)`.
/// `libgit2-sys` emits that cfg for its own compilation only, so a downstream
/// crate testing it always reads `false` -- and would report "no SHA-256" even in
/// a build that has it. Constructing a 32-byte raw object ID is the observable
/// difference: `GIT_OID_MAX_SIZE` is 20 in a default build and 32 under
/// `GIT_EXPERIMENTAL_SHA256`, and `git_oid_fromraw` length-checks against it.
pub fn build_has_sha256() -> bool {
  git2::Oid::from_bytes(&[0u8; 32]).is_ok()
}

/// The libgit2 version actually linked into this binary.
///
/// Reported at startup so a deployment cannot silently differ from the version
/// ADR 0001 pinned -- the supported-format boundary is a property of this exact
/// build.
pub fn libgit2_version() -> String {
  let (major, minor, patch) = git2::Version::get().libgit2_version();
  format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn format_with(extensions: &[(&str, &str)], version: i64) -> RepositoryFormat {
    let extensions: Vec<(String, String)> = extensions
      .iter()
      .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
      .collect();
    let algorithm = extensions
      .iter()
      .find(|(n, _)| n.eq_ignore_ascii_case("extensions.objectformat"))
      .and_then(|(_, v)| HashAlgorithm::from_name(v))
      .unwrap_or(HashAlgorithm::Sha1);
    let ref_backend = extensions
      .iter()
      .find(|(n, _)| n.eq_ignore_ascii_case("extensions.refstorage"))
      .map(|(_, v)| v.clone())
      .unwrap_or_else(|| "files".to_owned());
    RepositoryFormat {
      algorithm,
      ref_backend,
      repository_format_version: version,
      extensions,
    }
  }

  #[test]
  fn a_plain_sha1_files_repository_is_supported() {
    assert!(verdict(&format_with(&[], 0)).is_supported());
    assert!(verdict(&format_with(&[("extensions.objectformat", "sha1")], 1)).is_supported());
  }

  #[test]
  fn reftable_is_rejected_with_a_reason_naming_the_backend() {
    let v = verdict(&format_with(&[("extensions.refstorage", "reftable")], 1));
    let FormatVerdict::Rejected { reason } = v else {
      panic!("reftable must be rejected");
    };
    assert!(reason.contains("reftable"), "reason was: {reason}");
  }

  #[test]
  fn sha256_is_rejected_and_says_why_it_is_not_just_a_build_flag() {
    let v = verdict(&format_with(&[("extensions.objectformat", "sha256")], 1));
    let FormatVerdict::Rejected { reason } = v else {
      panic!("SHA-256 must be rejected");
    };
    // The operator needs to know this is not fixable by rebuilding libgit2.
    if build_has_sha256() {
      assert!(reason.contains("ADR 0006"));
    } else {
      assert!(reason.contains("git2-rs"), "reason was: {reason}");
    }
  }

  #[test]
  fn an_unknown_extension_is_rejected_rather_than_ignored() {
    let v = verdict(&format_with(&[("extensions.futurething", "1")], 1));
    assert!(!v.is_supported());
  }

  #[test]
  fn a_newer_repository_format_version_is_rejected() {
    assert!(!verdict(&format_with(&[], 2)).is_supported());
  }

  #[test]
  fn a_rejection_becomes_the_typed_error_the_catalog_returns() {
    let err = verdict(&format_with(&[("extensions.refstorage", "reftable")], 1))
      .into_result()
      .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedRepositoryFormat);
  }

  #[test]
  fn the_pinned_build_reports_the_adr_0001_libgit2_version() {
    // ADR 0001 pins libgit2 1.9.6, and the supported-format boundary is a
    // property of that exact build. A silent upgrade would change which
    // repositories the server accepts, so the version is asserted rather than
    // logged and hoped for.
    assert_eq!(libgit2_version(), "1.9.6");
    // And the build must not have experimental SHA-256, which would mean `git2`
    // could not have compiled at all.
    assert!(!build_has_sha256());
  }
}
