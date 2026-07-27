//! The `git` front-end shim.
//!
//! Installed early in `PATH` inside an agent image, under the name `git`.
//!
//! # It is a correctness requirement, not a convenience
//!
//! ADR 0005 measured what stock Git does against the synthesized `.git` surface:
//! `rev-parse` and `symbolic-ref` work; `status`, `log`, `show`, and `cat-file`
//! fail visibly; and **`ls-files` and `diff` exit 0 with empty output**. A tool
//! asking "what files are tracked?" is told "none", and one asking "what
//! changed?" is told "nothing". Both are wrong answers that look like right ones,
//! which is the exact failure mode the design set out to avoid. Intercepting them
//! is the fix.
//!
//! # The grammar is frozen, and everything outside it fails
//!
//! ADR 0005's table, reproduced exactly. There is no `--hydrate` escape hatch,
//! because under the synthesized surface there is no object database to delegate
//! to. Every other form exits non-zero with a message naming what *is* supported,
//! which is the difference between a tool that stops and a tool that proceeds on
//! an approximation.
//!
//! # No network, no credential
//!
//! Everything is answered from the mount, from `.git/xvfs.json`, and from the
//! daemon's local control socket. Nothing here holds a token: `log -1` reads the
//! commit metadata the daemon embedded at mount time, and `status` and `diff` ask
//! the daemon — over a `SOCK_STREAM` at mode 0600 on the same host — for the
//! overlay journal it already owns. A shim that called the *server* would need
//! the mount capability, and putting a credential in a `PATH`-installed wrapper
//! that any process can invoke is a worse trade than a local socket.
//!
//! # `status` is cheap because it is not a scan
//!
//! ADR 0005 measured stock `git status` against a partial clone of the Linux
//! kernel stat'ing all 94 850 index entries; inside a mount that is a full
//! metadata sweep of the monorepo, and an agent runs `git status` constantly.
//! This one is a single control round trip and touches no base metadata at all.

use std::io::Write;
use std::path::{Path, PathBuf};

use xvfs_fuse::control::{self, Request, Response};
use xvfs_types::path::b64url_decode;

const SUPPORTED: &str = "\
xvfs `git` shim: the supported grammar (ADR 0005) is

  status        --porcelain | --porcelain=v1 | --short | -z | (plain)
  diff          (plain) | --stat | --name-only | --name-status | --cached | -- <pathspec>
  rev-parse     HEAD | --show-toplevel | --git-dir | --abbrev-ref HEAD
                | --is-inside-work-tree | --verify HEAD
  ls-files      (plain) | -z | --cached | -- <pathspec>
  show          HEAD:<path>
  log           -1 [--format=<fmt> | --pretty=<fmt>]
  symbolic-ref  --short HEAD | HEAD

Anything else is refused rather than approximated: this workspace has no object
database, so there is nothing to delegate to.";

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  match run(&args) {
    Ok(()) => {}
    Err(Failure::Unsupported(what)) => {
      eprintln!("{what}\n\n{SUPPORTED}");
      // 2, not 1: `git diff --quiet` uses 1 to mean "there were differences", so
      // a refusal that exited 1 would be read as a successful non-empty diff.
      std::process::exit(2);
    }
    Err(Failure::Fatal(message)) => {
      eprintln!("fatal: {message}");
      std::process::exit(128);
    }
  }
}

enum Failure {
  /// Outside the frozen grammar.
  Unsupported(String),
  /// Inside the grammar, but the request cannot be answered.
  Fatal(String),
}

fn unsupported<T>(what: impl std::fmt::Display) -> Result<T, Failure> {
  Err(Failure::Unsupported(format!("unsupported: {what}")))
}

fn fatal<T>(what: impl std::fmt::Display) -> Result<T, Failure> {
  Err(Failure::Fatal(what.to_string()))
}

/// What `.git/xvfs.json` says about the workspace.
struct Workspace {
  toplevel: PathBuf,
  git_dir: PathBuf,
  commit: String,
  ref_name: Option<String>,
  commit_meta: Option<serde_json::Value>,
  control_socket: Option<PathBuf>,
}

impl Workspace {
  /// Walk up from the working directory looking for the synthesized surface.
  ///
  /// The same search stock Git does, and it stops at the first `.git` holding an
  /// `xvfs.json`: a shim that answered for an ordinary Git repository would be
  /// lying about a repository that works perfectly well without it.
  fn discover() -> Result<Self, Failure> {
    let start = std::env::current_dir()
      .map_err(|e| Failure::Fatal(format!("cannot determine the working directory: {e}")))?;
    let mut current: Option<&Path> = Some(&start);
    while let Some(directory) = current {
      let git_dir = directory.join(".git");
      let descriptor = git_dir.join("xvfs.json");
      if descriptor.is_file() {
        let bytes = std::fs::read(&descriptor)
          .map_err(|e| Failure::Fatal(format!("reading {}: {e}", descriptor.display())))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
          .map_err(|e| Failure::Fatal(format!("{} is unreadable: {e}", descriptor.display())))?;
        return Ok(Workspace {
          toplevel: directory.to_path_buf(),
          git_dir,
          commit: value["commit"].as_str().unwrap_or_default().to_owned(),
          ref_name: value["ref"].as_str().map(str::to_owned),
          commit_meta: value.get("commit_meta").cloned().filter(|v| !v.is_null()),
          control_socket: value["control_socket"].as_str().map(PathBuf::from),
        });
      }
      current = directory.parent();
    }
    fatal("not an XVFS workspace (no .git/xvfs.json in this directory or any parent)")
  }

  /// The commit's hex digest, without the algorithm prefix, as Git prints it.
  fn commit_hex(&self) -> &str {
    self
      .commit
      .split_once(':')
      .map(|(_, hex)| hex)
      .unwrap_or(&self.commit)
  }

  fn branch(&self) -> Option<&str> {
    self
      .ref_name
      .as_deref()
      .and_then(|name| name.strip_prefix("refs/heads/"))
  }
}

impl Workspace {
  /// Ask the daemon a question. The one place the shim talks to anything.
  fn ask(&self, request: Request) -> Result<Response, Failure> {
    let Some(socket) = &self.control_socket else {
      return fatal("this workspace predates the control socket; status and diff are unavailable");
    };
    control::call(socket, &request)
      .and_then(control::Response::into_result)
      .map_err(|e| {
        Failure::Fatal(format!(
          "the XVFS daemon did not answer ({}): {}",
          e.code.as_str(),
          e.message
        ))
      })
  }

  fn status(&self) -> Result<control::StatusReport, Failure> {
    match self.ask(Request::Status)? {
      Response::Status(report) => Ok(*report),
      _ => fatal("the daemon answered a status request with something else"),
    }
  }
}

fn run(args: &[String]) -> Result<(), Failure> {
  let Some((subcommand, rest)) = args.split_first() else {
    return unsupported("no subcommand");
  };
  // Global options are not part of the frozen grammar. `-C <dir>` and
  // `--git-dir=` in particular would change which workspace answers, and
  // silently answering for a different one is the class of wrong answer this
  // shim exists to prevent.
  if subcommand.starts_with('-') {
    return unsupported(format!("global option `{subcommand}`"));
  }

  let workspace = Workspace::discover()?;
  match subcommand.as_str() {
    "status" => status(&workspace, rest),
    "diff" => diff(&workspace, rest),
    "rev-parse" => rev_parse(&workspace, rest),
    "ls-files" => ls_files(&workspace, rest),
    "show" => show(&workspace, rest),
    "log" => log(&workspace, rest),
    "symbolic-ref" => symbolic_ref(&workspace, rest),
    other => unsupported(format!("`git {other}`")),
  }
}

// ---------------------------------------------------------------------------
// status and diff
// ---------------------------------------------------------------------------

/// `status`, derived from the overlay journal and touching no base metadata.
///
/// M2 has no overlay, so the tree is clean by construction. This is the half of
/// ADR 0005's argument that makes the shim *cheaper* than real Git as well as
/// more correct: stock `git status` against a partial clone of the Linux kernel
/// stats all 94 850 index entries, which inside a mount is a full metadata sweep
/// of the monorepo.
fn status(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let mut porcelain = false;
  let mut nul = false;
  for arg in args {
    match arg.as_str() {
      "--porcelain" | "--porcelain=v1" | "--short" => porcelain = true,
      "-z" => {
        porcelain = true;
        nul = true;
      }
      other => return unsupported(format!("`git status {other}`")),
    }
  }

  let report = workspace.status()?;
  let stdout = std::io::stdout();
  let mut out = stdout.lock();

  if porcelain {
    for change in &report.status.changes {
      // `XY path`. The change is in the X column -- the one that means "differs
      // from HEAD" -- because that is what an XVFS change *is*: there is no
      // index, and every recorded change is relative to the pinned commit.
      // Putting it in the worktree column instead would tell an agent its work
      // is unstaged and invite a `git add` the shim cannot answer.
      let _ = out.write_all(&[change.kind.porcelain(), b' ', b' ']);
      match (&change.from, nul) {
        // Git's two rename forms, and they are not the same order. Plain
        // `--porcelain` writes `old -> new`; `-z` writes the *new* path in the
        // record and the old one in a second NUL-terminated field, because a
        // path containing " -> " would otherwise be unparseable.
        (Some(from), false) => {
          let _ = out.write_all(from.as_bytes());
          let _ = out.write_all(b" -> ");
          let _ = out.write_all(change.path.as_bytes());
        }
        (Some(from), true) => {
          let _ = out.write_all(change.path.as_bytes());
          let _ = out.write_all(&[0]);
          let _ = out.write_all(from.as_bytes());
        }
        (None, _) => {
          let _ = out.write_all(change.path.as_bytes());
        }
      }
      let _ = out.write_all(if nul { &[0] } else { b"\n" });
    }
    return Ok(());
  }

  match workspace.branch() {
    Some(branch) => println!("On branch {branch}"),
    None => println!("HEAD detached at {}", short(workspace.commit_hex())),
  }
  if report.status.is_clean() {
    println!("nothing to commit, working tree clean");
    return Ok(());
  }
  println!("Changes not staged for commit:");
  for change in &report.status.changes {
    let label = match change.kind {
      xvfs_overlay::ChangeKind::Added => "new file",
      xvfs_overlay::ChangeKind::Modified => "modified",
      xvfs_overlay::ChangeKind::Deleted => "deleted",
      xvfs_overlay::ChangeKind::TypeChanged => "typechange",
      xvfs_overlay::ChangeKind::ModeChanged => "mode",
      xvfs_overlay::ChangeKind::Renamed => "renamed",
    };
    let _ = write!(out, "\t{label}:   ");
    if let Some(from) = &change.from {
      let _ = out.write_all(from.as_bytes());
      let _ = out.write_all(b" -> ");
    }
    let _ = out.write_all(change.path.as_bytes());
    let _ = out.write_all(b"\n");
  }
  Ok(())
}

fn diff(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let mut mode = DiffMode::Patch;
  let mut pathspecs: Vec<String> = Vec::new();
  let mut saw_separator = false;
  for arg in args {
    if saw_separator {
      reject_magic_pathspec(arg)?;
      pathspecs.push(arg.clone());
      continue;
    }
    match arg.as_str() {
      "--stat" => mode = DiffMode::Stat,
      "--name-only" => mode = DiffMode::NameOnly,
      "--name-status" => mode = DiffMode::NameStatus,
      // XVFS has no index, so there is nothing staged and `--cached` is empty by
      // definition. Accepted rather than refused because build tooling passes it
      // reflexively, and "no staged changes" is the true answer.
      "--cached" => mode = DiffMode::Cached,
      "--" => saw_separator = true,
      other => return unsupported(format!("`git diff {other}`")),
    }
  }
  if mode == DiffMode::Cached {
    return Ok(());
  }

  let report = workspace.status()?;
  let selected: Vec<&xvfs_overlay::status::Change> = report
    .status
    .changes
    .iter()
    .filter(|change| {
      pathspecs.is_empty()
        || pathspecs
          .iter()
          .any(|spec| matches(spec.as_bytes(), change.path.as_bytes()))
    })
    .collect();

  let stdout = std::io::stdout();
  let mut out = stdout.lock();
  match mode {
    DiffMode::NameOnly => {
      for change in selected {
        let _ = out.write_all(change.path.as_bytes());
        let _ = out.write_all(b"\n");
      }
    }
    DiffMode::NameStatus => {
      for change in selected {
        let _ = out.write_all(&[change.kind.porcelain(), b'\t']);
        let _ = out.write_all(change.path.as_bytes());
        let _ = out.write_all(b"\n");
      }
    }
    DiffMode::Stat => {
      for change in &selected {
        let _ = write!(out, " ");
        let _ = out.write_all(change.path.as_bytes());
        let _ = writeln!(out, " | {} bytes", change.new_size);
      }
      let _ = writeln!(out, " {} file(s) changed", selected.len());
    }
    DiffMode::Patch => {
      // The whole patch, then filtered by pathspec if one was given. Filtering
      // the *sections* rather than asking for a narrower diff keeps the daemon's
      // side to one shape, and a pathspec on a workspace's edit set is small.
      let Response::Diff { patch_b64url } = workspace.ask(Request::Diff)? else {
        return fatal("the daemon answered a diff request with something else");
      };
      let patch = b64url_decode(&patch_b64url).map_err(|e| {
        Failure::Fatal(format!(
          "the daemon returned an undecodable patch: {}",
          e.message
        ))
      })?;
      if pathspecs.is_empty() {
        let _ = out.write_all(&patch);
      } else {
        let _ = out.write_all(&filter_patch(&patch, &selected));
      }
    }
    DiffMode::Cached => unreachable!("returned above"),
  }
  Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
  Patch,
  Stat,
  NameOnly,
  NameStatus,
  Cached,
}

/// Keep only the `diff --git` sections whose destination path was selected.
fn filter_patch(patch: &[u8], selected: &[&xvfs_overlay::status::Change]) -> Vec<u8> {
  let wanted: Vec<&[u8]> = selected.iter().map(|c| c.path.as_bytes()).collect();
  let mut out = Vec::new();
  let mut keep = false;
  for line in patch.split_inclusive(|b| *b == b'\n') {
    if line.starts_with(b"diff --git ") {
      keep = wanted.iter().any(|path| {
        let mut needle = b" b/".to_vec();
        needle.extend_from_slice(path);
        line.windows(needle.len()).any(|w| w == needle.as_slice())
      });
    }
    if keep {
      out.extend_from_slice(line);
    }
  }
  out
}

// ---------------------------------------------------------------------------
// rev-parse and symbolic-ref
// ---------------------------------------------------------------------------

fn rev_parse(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  if args.is_empty() {
    return unsupported("`git rev-parse` with no arguments");
  }
  let mut index = 0;
  while index < args.len() {
    match args[index].as_str() {
      "HEAD" => println!("{}", workspace.commit_hex()),
      "--show-toplevel" => println!("{}", workspace.toplevel.display()),
      "--git-dir" => println!("{}", workspace.git_dir.display()),
      "--is-inside-work-tree" => println!("true"),
      "--abbrev-ref" => {
        // Only `--abbrev-ref HEAD`. `--abbrev-ref <other>` would need a ref
        // database this surface does not have.
        if args.get(index + 1).map(String::as_str) != Some("HEAD") {
          return unsupported("`git rev-parse --abbrev-ref` for anything but HEAD");
        }
        index += 1;
        match workspace.branch() {
          Some(branch) => println!("{branch}"),
          None => println!("HEAD"),
        }
      }
      "--verify" => {
        if args.get(index + 1).map(String::as_str) != Some("HEAD") {
          return unsupported("`git rev-parse --verify` for anything but HEAD");
        }
        index += 1;
        println!("{}", workspace.commit_hex());
      }
      other => return unsupported(format!("`git rev-parse {other}`")),
    }
    index += 1;
  }
  Ok(())
}

fn symbolic_ref(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let (short_form, name) = match args {
    [name] => (false, name.as_str()),
    [flag, name] if flag == "--short" => (true, name.as_str()),
    _ => return unsupported("`git symbolic-ref` outside `--short HEAD` and `HEAD`"),
  };
  if name != "HEAD" {
    return unsupported("`git symbolic-ref` for anything but HEAD");
  }
  match workspace.ref_name.as_deref() {
    Some(full) if full.starts_with("refs/heads/") => {
      if short_form {
        println!("{}", full.trim_start_matches("refs/heads/"));
      } else {
        println!("{full}");
      }
      Ok(())
    }
    // Stock Git exits 1 here, and so does this: a detached HEAD is not an error,
    // it is an answer of "no symbolic ref".
    _ => {
      eprintln!("fatal: ref HEAD is not a symbolic ref");
      std::process::exit(1);
    }
  }
}

// ---------------------------------------------------------------------------
// ls-files
// ---------------------------------------------------------------------------

/// Every path the workspace has, from a walk of the mount.
///
/// A full listing is inherent to what `ls-files` means, so this one *is* a tree
/// walk. It costs metadata and no blob content, and it is still the correct
/// answer where stock Git against this surface reports nothing at all.
///
/// The walk goes through the *mount*, which is already the merged view: a file
/// the job created is there, one it deleted is not, and a renamed one is under
/// its new name. Reading the journal instead and merging it here would be a
/// second implementation of the merge, which is how the two come to disagree.
fn ls_files(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let mut nul = false;
  let mut pathspecs: Vec<String> = Vec::new();
  let mut saw_separator = false;
  for arg in args {
    if saw_separator {
      reject_magic_pathspec(arg)?;
      pathspecs.push(arg.clone());
      continue;
    }
    match arg.as_str() {
      "-z" => nul = true,
      "--cached" => {}
      "--" => saw_separator = true,
      other if !other.starts_with('-') => {
        reject_magic_pathspec(other)?;
        pathspecs.push(other.to_owned());
      }
      other => return unsupported(format!("`git ls-files {other}`")),
    }
  }

  let mut paths = Vec::new();
  collect(&workspace.toplevel, &[], &mut paths)?;
  paths.sort();

  let stdout = std::io::stdout();
  let mut out = stdout.lock();
  for path in paths {
    if !pathspecs.is_empty() && !pathspecs.iter().any(|spec| matches(spec.as_bytes(), &path)) {
      continue;
    }
    let _ = out.write_all(&path);
    let _ = out.write_all(if nul { b"\0" } else { b"\n" });
  }
  Ok(())
}

/// Walk the workspace, skipping the synthesized surface.
///
/// DESIGN.md section 8.6: whatever occupies `.git` is outside change tracking and
/// is not searchable, listable, or exportable.
fn collect(root: &Path, relative: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), Failure> {
  use std::os::unix::ffi::OsStrExt;

  let mut here = root.to_path_buf();
  if !relative.is_empty() {
    here = root.join(std::ffi::OsStr::from_bytes(relative));
  }
  let entries = std::fs::read_dir(&here)
    .map_err(|e| Failure::Fatal(format!("reading {}: {}", here.display(), e.kind())))?;
  for entry in entries.flatten() {
    let name = entry.file_name();
    let name = name.as_bytes();
    if relative.is_empty() && name == b".git" {
      continue;
    }
    let mut child = relative.to_vec();
    if !child.is_empty() {
      child.push(b'/');
    }
    child.extend_from_slice(name);

    let Ok(kind) = entry.file_type() else {
      continue;
    };
    if kind.is_dir() {
      // A gitlink is an empty directory and contributes no paths, which matches
      // what `git ls-files` reports for an uninitialized submodule.
      collect(root, &child, out)?;
    } else {
      out.push(child);
    }
  }
  Ok(())
}

/// Reject magic pathspecs rather than approximating them.
///
/// ADR 0005: `:(glob)` and `:(exclude)` change matching semantics in ways this
/// shim does not implement, and silently ignoring the prefix would return the
/// wrong file set under a syntax the caller believes was honoured.
fn reject_magic_pathspec(spec: &str) -> Result<(), Failure> {
  if spec.starts_with(':') {
    return unsupported(format!("magic pathspec `{spec}`"));
  }
  Ok(())
}

/// Literal paths, directory prefixes, and simple globs.
///
/// `*` matches any bytes including `/`, which is Git's default behaviour without
/// `:(glob)`; `?` matches exactly one byte.
fn matches(spec: &[u8], path: &[u8]) -> bool {
  if spec == path {
    return true;
  }
  if !spec.contains(&b'*') && !spec.contains(&b'?') {
    // A directory prefix: `git ls-files src` lists everything under `src`.
    let mut prefix = spec.to_vec();
    prefix.push(b'/');
    return path.starts_with(&prefix);
  }
  glob(spec, path)
}

fn glob(spec: &[u8], path: &[u8]) -> bool {
  // Iterative backtracking rather than recursion: a pathspec is attacker-shaped
  // input in the sense that an agent writes it, and recursion on `*` sequences
  // is how a matcher becomes a stack overflow.
  let (mut s, mut p) = (0usize, 0usize);
  let (mut star, mut mark) = (usize::MAX, 0usize);
  while p < path.len() {
    if s < spec.len() && (spec[s] == b'?' || spec[s] == path[p]) {
      s += 1;
      p += 1;
    } else if s < spec.len() && spec[s] == b'*' {
      star = s;
      mark = p;
      s += 1;
    } else if star != usize::MAX {
      s = star + 1;
      mark += 1;
      p = mark;
    } else {
      return false;
    }
  }
  while s < spec.len() && spec[s] == b'*' {
    s += 1;
  }
  s == spec.len()
}

// ---------------------------------------------------------------------------
// show and log
// ---------------------------------------------------------------------------

/// `show HEAD:<path>` only. Reads the file from the mount.
fn show(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let [spec] = args else {
    return unsupported("`git show` outside `HEAD:<path>`");
  };
  let Some(path) = spec.strip_prefix("HEAD:") else {
    return unsupported(format!(
      "`git show {spec}` (only `HEAD:<path>` is supported)"
    ));
  };
  if path.is_empty() {
    return unsupported("`git show HEAD:` with an empty path");
  }
  let full = workspace.toplevel.join(path);
  let bytes = std::fs::read(&full).map_err(|e| {
    Failure::Fatal(format!(
      "path '{path}' does not exist in HEAD ({})",
      e.kind()
    ))
  })?;
  let _ = std::io::stdout().write_all(&bytes);
  Ok(())
}

/// `log -1`, from the metadata the daemon embedded at mount time.
fn log(workspace: &Workspace, args: &[String]) -> Result<(), Failure> {
  let mut bounded = false;
  let mut format: Option<String> = None;
  for arg in args {
    match arg.as_str() {
      "-1" | "-n1" | "-n" => bounded = true,
      other if other.starts_with("--format=") || other.starts_with("--pretty=") => {
        format = Some(
          other
            .split_once('=')
            .map(|(_, v)| v.to_owned())
            .unwrap_or_default(),
        );
      }
      other => return unsupported(format!("`git log {other}`")),
    }
  }
  if !bounded {
    // Unbounded `log` needs commit history, and this workspace has exactly one
    // commit's worth of metadata. Refusing is the honest answer; printing the
    // single commit would look like a complete history.
    return unsupported("`git log` without `-1` (only the pinned commit is available)");
  }
  let Some(meta) = &workspace.commit_meta else {
    return fatal("commit metadata is unavailable for this mount");
  };

  let author = identity(meta, "author");
  let committer = identity(meta, "committer");
  let message = decode(meta, "message_b64url");
  let subject = message
    .split(|b| *b == b'\n')
    .next()
    .unwrap_or_default()
    .to_vec();

  match format.as_deref() {
    None | Some("medium") => {
      println!("commit {}", workspace.commit_hex());
      println!("Author: {}", String::from_utf8_lossy(&author));
      println!("Date:   {}", timestamp(meta, "author"));
      println!();
      for line in message.split(|b| *b == b'\n') {
        println!("    {}", String::from_utf8_lossy(line));
      }
    }
    Some("oneline") => println!(
      "{} {}",
      workspace.commit_hex(),
      String::from_utf8_lossy(&subject)
    ),
    Some(spec) => {
      let rendered = render(
        spec, workspace, &author, &committer, &subject, &message, meta,
      )?;
      let _ = std::io::stdout().write_all(&rendered);
      println!();
    }
  }
  Ok(())
}

fn decode(meta: &serde_json::Value, key: &str) -> Vec<u8> {
  meta
    .get(key)
    .and_then(|v| v.as_str())
    .and_then(|s| b64url_decode(s).ok())
    .unwrap_or_default()
}

/// `Name <email>` as Git prints it, from the byte-exact encoded fields.
fn identity(meta: &serde_json::Value, which: &str) -> Vec<u8> {
  let Some(signature) = meta.get(which) else {
    return Vec::new();
  };
  let mut out = decode(signature, "name_b64url");
  out.extend_from_slice(b" <");
  out.extend_from_slice(&decode(signature, "email_b64url"));
  out.push(b'>');
  out
}

fn timestamp(meta: &serde_json::Value, which: &str) -> String {
  meta
    .get(which)
    .and_then(|s| s.get("time"))
    .and_then(|t| t.get("secs"))
    .and_then(serde_json::Value::as_i64)
    .map(|secs| format!("{secs} +0000"))
    .unwrap_or_default()
}

/// A bounded placeholder set. Anything else is refused.
///
/// Git's format language is large and its output is parsed by scripts; a
/// placeholder rendered as itself, or silently dropped, would produce a plausible
/// line that is wrong.
fn render(
  spec: &str,
  workspace: &Workspace,
  author: &[u8],
  committer: &[u8],
  subject: &[u8],
  message: &[u8],
  meta: &serde_json::Value,
) -> Result<Vec<u8>, Failure> {
  // Longest first, so `%an` is not read as `%a` followed by a literal `n`.
  const PLACEHOLDERS: &[&str] = &[
    "an", "ae", "ad", "cn", "ce", "cd", "H", "h", "s", "B", "n", "%",
  ];

  let bytes = spec.as_bytes();
  let mut out = Vec::new();
  let mut index = 0;
  while index < bytes.len() {
    if bytes[index] != b'%' {
      out.push(bytes[index]);
      index += 1;
      continue;
    }
    // Matched on bytes rather than on `&str` slices: a `%` is one byte, so
    // `index + 1` is always a character boundary, but saying so to the compiler
    // costs more than simply never leaving the byte domain.
    let rest = &bytes[index + 1..];
    let Some(placeholder) = PLACEHOLDERS.iter().find(|p| rest.starts_with(p.as_bytes())) else {
      let next = String::from_utf8_lossy(&rest[..rest.len().min(2)]).into_owned();
      return unsupported(format!(
        "format placeholder `%{next}` (supported: %H %h %an %ae %ad %cn %ce %cd %s %B %n %%)"
      ));
    };
    match *placeholder {
      "H" => out.extend_from_slice(workspace.commit_hex().as_bytes()),
      "h" => out.extend_from_slice(short(workspace.commit_hex()).as_bytes()),
      "an" => out.extend_from_slice(&field(meta, "author", "name_b64url")),
      "ae" => out.extend_from_slice(&field(meta, "author", "email_b64url")),
      "ad" => out.extend_from_slice(timestamp(meta, "author").as_bytes()),
      "cn" => out.extend_from_slice(&field(meta, "committer", "name_b64url")),
      "ce" => out.extend_from_slice(&field(meta, "committer", "email_b64url")),
      "cd" => out.extend_from_slice(timestamp(meta, "committer").as_bytes()),
      "s" => out.extend_from_slice(subject),
      "B" => out.extend_from_slice(message),
      "n" => out.push(b'\n'),
      _ => out.push(b'%'),
    }
    index += 1 + placeholder.len();
  }
  let _ = (author, committer);
  Ok(out)
}

/// One byte-exact field of an identity line.
fn field(meta: &serde_json::Value, which: &str, key: &str) -> Vec<u8> {
  meta.get(which).map(|s| decode(s, key)).unwrap_or_default()
}

fn short(hex: &str) -> String {
  hex.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn globs_match_the_way_a_default_git_pathspec_does() {
    assert!(matches(b"src/main.rs", b"src/main.rs"));
    // A bare directory name is a prefix, as `git ls-files src` is.
    assert!(matches(b"src", b"src/main.rs"));
    assert!(!matches(b"src", b"srcfile.rs"));
    // `*` crosses `/` without `:(glob)`, which is Git's default.
    assert!(matches(b"*.rs", b"src/lib/util.rs"));
    assert!(matches(b"src/*", b"src/lib/util.rs"));
    assert!(!matches(b"*.rs", b"README.md"));
    assert!(matches(b"READM?.md", b"README.md"));
  }

  #[test]
  fn a_star_heavy_pattern_terminates() {
    // The backtracking matcher's worst case, which a recursive one would blow
    // the stack on.
    assert!(!glob(b"*a*a*a*a*a*a*a*a*b", &b"a".repeat(64)));
  }

  #[test]
  fn magic_pathspecs_are_refused_rather_than_ignored() {
    assert!(reject_magic_pathspec(":(exclude)vendor").is_err());
    assert!(reject_magic_pathspec(":(glob)**/*.rs").is_err());
    assert!(reject_magic_pathspec("vendor").is_ok());
  }
}
