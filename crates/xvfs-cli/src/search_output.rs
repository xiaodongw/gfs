//! Rendering a search result, and the exit code that goes with it.
//!
//! ADR 0004's table is the agent-facing contract:
//!
//! | Outcome | Exit |
//! | --- | ---: |
//! | Complete, matches found | 0 |
//! | Complete, no matches | 1 |
//! | Missing terminal message / transport failure | 2 |
//! | Execution truncated | 3 |
//! | Coverage gap under `--require-exhaustive` | 4 |
//!
//! The mapping itself lives in `xvfs_search::exit_code` and is called from here,
//! so `xvfs search` and `xvfs-rg` cannot drift apart and neither can invent its
//! own interpretation.
//!
//! # Coverage goes to stderr, results go to stdout
//!
//! An agent pipes stdout into a parser. A coverage warning on stdout would
//! corrupt that parse, and a coverage warning suppressed entirely would hide the
//! thing the contract exists to surface. stderr is the only channel that is both
//! visible and out of the way — and in `--json` mode the same information is a
//! field, because a JSON consumer is not reading stderr.

use std::io::Write;

use xvfs_fuse::search::SearchReport;
use xvfs_search::query::{ExecutionStatus, SearchOutcome};

/// Text output, ripgrep-shaped.
///
/// `path:line:column:text`, which is `rg --vimgrep`'s form rather than its
/// default. The default groups by file and prints a header, which is nicer to
/// read and harder to consume; every agent-facing tool here optimizes for the
/// consumer.
///
/// A line cut to `max_line_bytes` gets a trailing marker, after the text rather
/// than inside it, so a reader is not shown a prefix as though it were the
/// line. `--json` carries the same fact as `line_truncated` instead, because a
/// JSON consumer parses a field and would have to strip a marker back out.
pub fn print_text(report: &SearchReport, out: &mut impl Write) -> std::io::Result<()> {
  let SearchOutcome::Completed(result) = &report.outcome else {
    return Ok(());
  };
  for m in &result.matches {
    out.write_all(&m.path)?;
    write!(out, ":{}:{}:", m.line, m.column)?;
    out.write_all(&m.line_text)?;
    if m.line_truncated {
      out.write_all(b" [... line truncated]")?;
    }
    out.write_all(b"\n")?;
  }
  Ok(())
}

/// The coverage and execution report, for stderr.
///
/// Printed when there is something to say and silent otherwise, so a clean
/// search produces no noise and a warning means something.
pub fn print_diagnostics(report: &SearchReport, err: &mut impl Write) -> std::io::Result<()> {
  match &report.outcome {
    SearchOutcome::FailedBeforeCompletion(reason) => {
      writeln!(
        err,
        "xvfs search: the search did not complete: {reason}\n\
         The results above, if any, are not an answer -- the server never \
         reported how complete they are."
      )?;
    }
    SearchOutcome::Completed(result) => {
      let completion = &result.completion;
      if completion.execution_status == ExecutionStatus::Truncated {
        let reason = completion
          .truncation
          .map(|t| format!("{t:?}"))
          .unwrap_or_else(|| "unspecified".to_owned());
        writeln!(
          err,
          "xvfs search: TRUNCATED ({reason}). This is not an exhaustive answer."
        )?;
        if let Some(budget) = &completion.stop_budget {
          writeln!(err, "  stopped by: {budget}")?;
        }
      }
      let gaps: Vec<String> = completion
        .coverage
        .excluded
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(reason, n)| format!("{n} {reason}"))
        .collect();
      if !gaps.is_empty() {
        writeln!(
          err,
          "xvfs search: {} of {} paths in scope were not searched: {}",
          gaps
            .iter()
            .filter_map(|g| g.split_whitespace().next())
            .filter_map(|n| n.parse::<u64>().ok())
            .sum::<u64>(),
          completion.coverage.eligible_paths + completion.coverage.excluded.values().sum::<u64>(),
          gaps.join(", ")
        )?;
      }
    }
  }
  Ok(())
}

/// The exit code, from ADR 0004's table.
pub fn exit_code(report: &SearchReport, require_exhaustive: bool) -> i32 {
  xvfs_search::exit_code(&report.outcome, require_exhaustive)
}

/// The JSON form.
///
/// One object with the matches, the completion, and the base commit. Byte
/// fields are base64url, because a path and a line of source are bytes that need
/// not be UTF-8 and `serde_json` cannot hold a non-UTF-8 string — a lossy
/// conversion would put U+FFFD into the one field an agent uses to open a file.
pub fn to_json(report: &SearchReport) -> serde_json::Result<String> {
  serde_json::to_string_pretty(report)
}

#[cfg(test)]
mod tests {
  use super::*;
  use xvfs_search::query::{Completion, Coverage, Match, SearchResult, TruncationReason};

  fn report(outcome: SearchOutcome) -> SearchReport {
    SearchReport {
      base_commit: "sha1:0000000000000000000000000000000000000000".to_owned(),
      ref_name: Some("refs/heads/main".to_owned()),
      local_matches: 0,
      outcome,
    }
  }

  fn completed(matches: Vec<Match>, status: ExecutionStatus, gaps: bool) -> SearchOutcome {
    let mut excluded = std::collections::BTreeMap::new();
    if gaps {
      excluded.insert("binary".to_owned(), 3);
    }
    SearchOutcome::Completed(SearchResult {
      matches,
      completion: Completion {
        execution_status: status,
        truncation: (status == ExecutionStatus::Truncated).then_some(TruncationReason::ResultLimit),
        coverage: Coverage {
          scope: Vec::new(),
          eligible_paths: 10,
          excluded,
          declared_exclusions: vec!["binary".to_owned()],
        },
        index_generation: 1,
        commit: "sha1:0000000000000000000000000000000000000000".to_owned(),
        stop_budget: None,
        candidates_considered: 1,
        bytes_read: 10,
        elapsed_ms: 1,
      },
    })
  }

  fn a_match() -> Match {
    Match {
      path: b"src/main.rs".to_vec(),
      line: 4,
      column: 9,
      matched: b"needle".to_vec(),
      line_text: b"    let needle = 1;".to_vec(),
      before: Vec::new(),
      after: Vec::new(),
      line_truncated: false,
      blob_oid: String::new(),
    }
  }

  #[test]
  fn a_truncated_line_is_marked_rather_than_printed_as_though_it_were_whole() {
    let mut m = a_match();
    m.line_text = b"aaaa".to_vec();
    m.line_truncated = true;
    let mut out = Vec::new();
    print_text(
      &report(completed(vec![m], ExecutionStatus::Complete, false)),
      &mut out,
    )
    .unwrap();
    assert_eq!(out, b"src/main.rs:4:9:aaaa [... line truncated]\n");
  }

  #[test]
  fn text_output_is_path_line_column_text() {
    let mut out = Vec::new();
    print_text(
      &report(completed(vec![a_match()], ExecutionStatus::Complete, false)),
      &mut out,
    )
    .unwrap();
    assert_eq!(out, b"src/main.rs:4:9:    let needle = 1;\n");
  }

  #[test]
  fn a_non_utf8_path_is_written_as_bytes_not_as_replacement_characters() {
    let mut m = a_match();
    m.path = b"src/caf\xe9.rs".to_vec();
    let mut out = Vec::new();
    print_text(
      &report(completed(vec![m], ExecutionStatus::Complete, false)),
      &mut out,
    )
    .unwrap();
    assert!(out.starts_with(b"src/caf\xe9.rs:"));
  }

  #[test]
  fn the_exit_codes_are_adr_0004s_table() {
    let found = report(completed(vec![a_match()], ExecutionStatus::Complete, false));
    let empty = report(completed(Vec::new(), ExecutionStatus::Complete, false));
    let truncated = report(completed(Vec::new(), ExecutionStatus::Truncated, false));
    let gapped = report(completed(vec![a_match()], ExecutionStatus::Complete, true));
    let lost = report(SearchOutcome::FailedBeforeCompletion("reset".to_owned()));

    assert_eq!(exit_code(&found, false), 0);
    assert_eq!(exit_code(&empty, false), 1);
    assert_eq!(exit_code(&lost, false), 2);
    assert_eq!(exit_code(&truncated, false), 3);
    assert_eq!(exit_code(&gapped, false), 0, "gaps warn by default");
    assert_eq!(exit_code(&gapped, true), 4, "--require-exhaustive fails");
    // The pair the whole contract exists for.
    assert_ne!(exit_code(&empty, false), exit_code(&truncated, false));
  }

  #[test]
  fn a_clean_search_prints_no_diagnostics() {
    let mut err = Vec::new();
    print_diagnostics(
      &report(completed(vec![a_match()], ExecutionStatus::Complete, false)),
      &mut err,
    )
    .unwrap();
    assert!(
      err.is_empty(),
      "a warning on every query is a warning agents learn to ignore"
    );
  }

  #[test]
  fn truncation_and_coverage_are_reported_separately() {
    let mut err = Vec::new();
    print_diagnostics(
      &report(completed(Vec::new(), ExecutionStatus::Truncated, true)),
      &mut err,
    )
    .unwrap();
    let text = String::from_utf8(err).unwrap();
    assert!(text.contains("TRUNCATED"), "{text}");
    assert!(text.contains("binary"), "{text}");
  }

  #[test]
  fn a_lost_stream_says_the_results_are_not_an_answer() {
    let mut err = Vec::new();
    print_diagnostics(
      &report(SearchOutcome::FailedBeforeCompletion("reset".to_owned())),
      &mut err,
    )
    .unwrap();
    let text = String::from_utf8(err).unwrap();
    assert!(text.contains("did not complete"), "{text}");
  }

  #[test]
  fn json_carries_the_completion_as_fields() {
    let json = to_json(&report(completed(
      vec![a_match()],
      ExecutionStatus::Truncated,
      true,
    )))
    .unwrap();
    assert!(
      json.contains("\"execution_status\": \"TRUNCATED\""),
      "{json}"
    );
    assert!(json.contains("\"binary\": 3"), "{json}");
    assert!(json.contains("\"base_commit\""), "{json}");
  }
}
