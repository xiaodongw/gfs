//! Repository read policy.
//!
//! Deliberately a trait with a simple allow-list implementation. The real policy
//! source in a deployment is the surrounding platform's -- a group membership
//! service, a GitHub team, an IAM binding -- and none of those decisions belong in
//! XVFS. What belongs here is the *shape*: one predicate, consulted identically by
//! every surface, so PLAN.md M1.5's "enforce repository permissions uniformly
//! across Git, RPC, file, and search APIs" is structurally true rather than
//! reviewed for.

use xvfs_types::{RepositoryId, SubjectId};

pub trait RepositoryPolicy: Send + Sync + std::fmt::Debug {
  /// Whether `subject` may read `repository_id`.
  ///
  /// Must not consult the catalog or the object database. The reason is the timing
  /// requirement in M1's exit criteria: this predicate runs *before* anything about
  /// the repository is looked up, so an unauthorized caller's request costs the
  /// same whether or not the repository exists. A policy that itself queried the
  /// catalog would reintroduce the side channel.
  fn may_read(&self, subject: &SubjectId, repository_id: &RepositoryId) -> bool;
}

/// An explicit allow-list of `(subject, repository)` pairs.
#[derive(Debug, Default)]
pub struct AllowList {
  pairs: std::collections::HashSet<(String, String)>,
  /// Subjects allowed to read every repository, for the development stack and
  /// orchestrator-level tooling.
  wildcards: std::collections::HashSet<String>,
}

impl AllowList {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn allow(mut self, subject: &SubjectId, repository_id: &RepositoryId) -> Self {
    self.pairs.insert((
      subject.as_str().to_owned(),
      repository_id.as_str().to_owned(),
    ));
    self
  }

  pub fn allow_all_repositories(mut self, subject: &SubjectId) -> Self {
    self.wildcards.insert(subject.as_str().to_owned());
    self
  }
}

impl RepositoryPolicy for AllowList {
  fn may_read(&self, subject: &SubjectId, repository_id: &RepositoryId) -> bool {
    self.wildcards.contains(subject.as_str())
      || self.pairs.contains(&(
        subject.as_str().to_owned(),
        repository_id.as_str().to_owned(),
      ))
  }
}

/// Denies everything. Useful as an explicit default so a misconfigured deployment
/// fails closed rather than serving every repository to everyone.
#[derive(Debug, Default)]
pub struct DenyAll;

impl RepositoryPolicy for DenyAll {
  fn may_read(&self, _subject: &SubjectId, _repository_id: &RepositoryId) -> bool {
    false
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn subject(s: &str) -> SubjectId {
    SubjectId::parse(s).unwrap()
  }

  fn repo(s: &str) -> RepositoryId {
    RepositoryId::parse(s).unwrap()
  }

  #[test]
  fn an_allow_list_grants_only_the_listed_pair() {
    let policy = AllowList::new().allow(&subject("job-a"), &repo("r-1"));
    assert!(policy.may_read(&subject("job-a"), &repo("r-1")));
    assert!(!policy.may_read(&subject("job-a"), &repo("r-2")));
    assert!(!policy.may_read(&subject("job-b"), &repo("r-1")));
  }

  #[test]
  fn a_wildcard_subject_reads_every_repository() {
    let policy = AllowList::new().allow_all_repositories(&subject("orchestrator"));
    assert!(policy.may_read(&subject("orchestrator"), &repo("r-anything")));
    assert!(!policy.may_read(&subject("job-a"), &repo("r-anything")));
  }

  #[test]
  fn the_default_policy_fails_closed() {
    // A misconfigured deployment must serve nothing, not everything.
    assert!(!DenyAll.may_read(&subject("job-a"), &repo("r-1")));
    assert!(!AllowList::new().may_read(&subject("job-a"), &repo("r-1")));
  }
}
