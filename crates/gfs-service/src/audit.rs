//! Audit records.
//!
//! ADR 0006 fixes the content rule: audit records carry repository, commit,
//! subject, and job -- **never file content, tokens, or unvalidated paths**.
//!
//! That rule is enforced by construction rather than by review. [`AuditRecord`]
//! has no field that can hold content or a credential, and the one field that can
//! hold caller-supplied bytes -- the path -- is typed as [`BytePath`] and is passed
//! through [`gfs_types::redact::path`] on the way out, so it is escaped and
//! bounded before it reaches a log line. A raw path in a structured log is log
//! injection, and it is worse in a structured log than a plain one because the
//! injected text looks like a field.

use gfs_types::error::ErrorCode;
use gfs_types::{redact, BytePath, MountId, ObjectId, RepositoryId, SubjectId};

/// What was attempted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
  ResolveRevision,
  ListRefs,
  GetCommit,
  GetEntry,
  ListDirectory,
  BatchGetEntry,
  Log,
  DiffCommits,
  Blame,
  PrepareSnapshot,
  ReadBlob,
  CreateMount,
  RenewMount,
  ReleaseMount,
  Search,
  GitFetch,
  GitPush,
}

impl Action {
  pub fn as_str(self) -> &'static str {
    match self {
      Action::ResolveRevision => "resolve_revision",
      Action::ListRefs => "list_refs",
      Action::GetCommit => "get_commit",
      Action::GetEntry => "get_entry",
      Action::ListDirectory => "list_directory",
      Action::BatchGetEntry => "batch_get_entry",
      Action::Log => "log",
      Action::DiffCommits => "diff_commits",
      Action::Blame => "blame",
      Action::PrepareSnapshot => "prepare_snapshot",
      Action::ReadBlob => "read_blob",
      Action::CreateMount => "create_mount",
      Action::RenewMount => "renew_mount",
      Action::ReleaseMount => "release_mount",
      Action::Search => "search",
      Action::GitFetch => "git_fetch",
      Action::GitPush => "git_push",
    }
  }
}

/// One audited event.
///
/// Built with `..Default::default()` so a new field does not have to be threaded
/// through every call site, and so a call site that has nothing to say about a
/// field cannot be tempted to put something else there.
#[derive(Clone, Debug, Default)]
pub struct AuditRecord<'a> {
  pub subject: Option<&'a SubjectId>,
  pub repository_id: Option<&'a RepositoryId>,
  pub commit: Option<&'a ObjectId>,
  pub mount_id: Option<&'a MountId>,
  /// A caller-supplied path. Redacted on the way out; never logged raw.
  pub path: Option<&'a BytePath>,
  /// Whether access rested on a mount capability rather than ref reachability.
  /// Recorded because it is the interesting case: the commit was not publicly
  /// reachable.
  pub via_capability: bool,
  /// The request identifier, so an audit line joins to a trace.
  pub request_id: Option<&'a str>,
}

/// Record a successful action.
pub fn success(action: Action, record: &AuditRecord<'_>) {
  emit(action, record, "ok", None);
}

/// Record a failed action.
///
/// Only the error *code* is recorded, never the message. A message can contain a
/// path or another caller-supplied value, and an audit log is exactly the place
/// where that must not accumulate.
pub fn failure(action: Action, record: &AuditRecord<'_>, code: ErrorCode) {
  emit(action, record, "error", Some(code));
}

fn emit(action: Action, record: &AuditRecord<'_>, outcome: &str, code: Option<ErrorCode>) {
  tracing::info!(
    target: "gfs::audit",
    action = action.as_str(),
    outcome,
    error_code = code.map(ErrorCode::as_str),
    subject = record.subject.map(SubjectId::as_str),
    repository_id = record.repository_id.map(RepositoryId::as_str),
    commit = record.commit.map(redact::oid),
    mount_id = record.mount_id.map(|m| m.as_str()),
    // Escaped and bounded. The type system cannot stop a caller passing an
    // attacker-controlled path, so the formatting does.
    path = record.path.map(redact::path),
    via_capability = record.via_capability,
    request_id = record.request_id,
    "audit"
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A `tracing` subscriber that captures formatted events, so the test can assert
  /// on what would actually reach a log sink rather than on the call arguments.
  #[derive(Clone, Default)]
  struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

  impl Capture {
    fn lines(&self) -> Vec<String> {
      self.0.lock().unwrap().clone()
    }
  }

  impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
      &self,
      event: &tracing::Event<'_>,
      _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
      struct Visit(String);
      impl tracing::field::Visit for Visit {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
          self.0.push_str(&format!("{}={:?} ", field.name(), value));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
          self.0.push_str(&format!("{}={} ", field.name(), value));
        }
      }
      let mut v = Visit(String::new());
      event.record(&mut v);
      self.0.lock().unwrap().push(v.0);
    }
  }

  fn capture(f: impl FnOnce()) -> Vec<String> {
    use tracing_subscriber::layer::SubscriberExt;
    let cap = Capture::default();
    let subscriber = tracing_subscriber::registry().with(cap.clone());
    tracing::subscriber::with_default(subscriber, f);
    cap.lines()
  }

  #[test]
  fn an_audit_line_carries_the_fields_adr_0006_requires() {
    let subject = SubjectId::parse("job-123").unwrap();
    let repo = RepositoryId::parse("r-abc").unwrap();
    let commit = ObjectId::from_hex(gfs_types::HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
    let mount = MountId::parse("m-1").unwrap();

    let lines = capture(|| {
      success(
        Action::GetEntry,
        &AuditRecord {
          subject: Some(&subject),
          repository_id: Some(&repo),
          commit: Some(&commit),
          mount_id: Some(&mount),
          via_capability: true,
          ..Default::default()
        },
      );
    });

    let line = lines.join("\n");
    assert!(line.contains("job-123"), "{line}");
    assert!(line.contains("r-abc"), "{line}");
    assert!(line.contains("sha1:ababababab"), "{line}");
    assert!(line.contains("m-1"), "{line}");
    assert!(line.contains("get_entry"), "{line}");
    assert!(line.contains("via_capability=true"), "{line}");
  }

  #[test]
  fn a_path_reaches_the_log_escaped_and_cannot_forge_a_record() {
    // The log-injection case. A tracked file can be named so that its path would
    // terminate one log record and start another.
    let path = BytePath::new(b"src/a\n2026-07-26 ERROR forged entry\n".to_vec());
    let lines = capture(|| {
      failure(
        Action::GetEntry,
        &AuditRecord {
          path: Some(&path),
          ..Default::default()
        },
        ErrorCode::NotFound,
      );
    });
    let line = lines.join("\n");
    assert!(line.contains("\\n"), "the path must be escaped: {line}");
    assert!(
      !line.contains("ERROR forged entry\n"),
      "a raw newline survived into the record: {line}"
    );
    assert!(line.contains("NOT_FOUND"), "{line}");
  }

  #[test]
  fn a_failure_records_the_code_and_not_a_message() {
    // An error message can contain a path or another caller-supplied value, and an
    // audit log is exactly where that must not accumulate.
    let lines = capture(|| {
      failure(
        Action::ReadBlob,
        &AuditRecord::default(),
        ErrorCode::PermissionDenied,
      );
    });
    let line = lines.join("\n");
    assert!(line.contains("PERMISSION_DENIED"), "{line}");
    // `tracing` always emits the event's own static text as a `message` field, so
    // its presence proves nothing either way. The property to check is that the
    // only message is that constant, and no error detail rode along with it.
    assert!(line.contains("message=audit"), "{line}");
    assert!(!line.contains("error_message"), "{line}");
    assert_eq!(
      line.matches("message=").count(),
      1,
      "only the constant event message may appear: {line}"
    );
  }

  #[test]
  fn an_audit_record_has_nowhere_to_put_a_token_or_file_content() {
    // The structural half of ADR 0006's rule: this is a compile-time property, and
    // the assertion documents it. `AuditRecord` has no `String` field a caller
    // could fill with a credential or a file body -- only typed identifiers, a
    // `BytePath`, and a request id.
    let record = AuditRecord::default();
    assert!(record.subject.is_none());
    assert!(record.path.is_none());
    assert!(!record.via_capability);
  }
}
