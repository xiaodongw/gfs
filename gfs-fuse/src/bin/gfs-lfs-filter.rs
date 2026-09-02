//! The daemon-backed `filter.lfs` clean/smudge shim (ADR 0012).
//!
//! The workspace git config names this binary for both filter directions, and
//! that configuration is a **correctness requirement**, not an optimization:
//! m05d measured that without a clean filter, `git status` misreports every
//! LFS file and `git add` writes the expanded content into the object
//! database — the branch-corrupting move.
//!
//! - **clean** (`gfs-lfs-filter clean <path>`): content arrives on stdin, a
//!   pointer must leave on stdout. The daemon answers the pointer from entry
//!   metadata for base-identical paths — no read, no hash (m05d arm C, 4×
//!   cheaper) — and stdin is drained without looking at it. For a genuinely
//!   edited file the shim hashes stdin and emits a fresh spec v1 pointer,
//!   which is exactly what git-lfs's clean does. Input that already *is* a
//!   pointer passes through unchanged, so a degraded (pointer-text) entry can
//!   never be double-wrapped.
//!
//! - **smudge** (`gfs-lfs-filter smudge <path>`): a pointer arrives on stdin,
//!   the content must leave on stdout. The daemon hydrates through the shared
//!   verified blob cache and answers with the cached file's path; the shim
//!   streams it. When the object is unavailable — a degraded entry, an
//!   unreachable daemon — the pointer passes through unchanged, which is the
//!   truthful LFS behavior for content that is not present.
//!
//! Like the fsmonitor hook, this carries no credential: the control socket is
//! a local 0600 `SOCK_STREAM`, and a process that can open it can already
//! read the workspace.

use std::io::{Read, Write};
use std::path::PathBuf;

use gfs_types::lfs::{parse_pointer, LfsPointer};
use gfs_types::{HashAlgorithm, ObjectId};

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let exit = match (args.get(1).map(String::as_str), args.get(2)) {
    (Some("clean"), Some(path)) => clean(&path.clone()),
    (Some("smudge"), Some(path)) => smudge(&path.clone()),
    // The long-running form (`filter.<driver>.process`). Seeded as the
    // workspace's `filter.lfs.process` because Git prefers the process form
    // over clean/smudge across config scopes: on a host with git-lfs
    // installed, the *global* `filter.lfs.process = git-lfs filter-process`
    // would otherwise hijack the driver, derive a batch endpoint the gateway
    // does not serve, and fail every checkout. Only a local process entry
    // can win that precedence — and a set-but-empty one poisons the driver
    // rather than falling back (measured against Git 2.53).
    (Some("process"), _) => process_loop(),
    _ => {
      eprintln!("usage: gfs-lfs-filter <clean|smudge> <path> | process");
      2
    }
  };
  std::process::exit(exit);
}

// ---------------------------------------------------------------------------
// The filter-process protocol (gitattributes(5) "long running filter process")
// ---------------------------------------------------------------------------

/// pkt-line framing: 4 hex digits of length (including the 4), then payload;
/// `0000` is the flush packet.
fn read_pkt(input: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
  let mut len = [0u8; 4];
  input.read_exact(&mut len)?;
  let len = usize::from_str_radix(
    std::str::from_utf8(&len).map_err(|_| std::io::ErrorKind::InvalidData)?,
    16,
  )
  .map_err(|_| std::io::ErrorKind::InvalidData)?;
  if len == 0 {
    return Ok(None); // flush
  }
  let mut payload = vec![0u8; len - 4];
  input.read_exact(&mut payload)?;
  Ok(Some(payload))
}

fn write_pkt(out: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
  write!(out, "{:04x}", payload.len() + 4)?;
  out.write_all(payload)
}

fn write_text_pkt(out: &mut impl Write, line: &str) -> std::io::Result<()> {
  write_pkt(out, format!("{line}\n").as_bytes())
}

fn write_flush(out: &mut impl Write) -> std::io::Result<()> {
  out.write_all(b"0000")?;
  out.flush()
}

/// Read `key=value` packets up to the next flush.
fn read_kv_until_flush(input: &mut impl Read) -> std::io::Result<Vec<(String, String)>> {
  let mut out = Vec::new();
  while let Some(pkt) = read_pkt(input)? {
    let text = String::from_utf8_lossy(&pkt);
    let text = text.trim_end_matches('\n');
    if let Some((k, v)) = text.split_once('=') {
      out.push((k.to_owned(), v.to_owned()));
    }
  }
  Ok(out)
}

/// Read content packets up to the next flush.
fn read_content_until_flush(input: &mut impl Read) -> std::io::Result<Vec<u8>> {
  let mut out = Vec::new();
  while let Some(pkt) = read_pkt(input)? {
    out.extend_from_slice(&pkt);
  }
  Ok(out)
}

/// Content packets hold at most 65516 payload bytes.
fn write_content(out: &mut impl Write, mut content: &[u8]) -> std::io::Result<()> {
  const MAX: usize = 65516;
  while !content.is_empty() {
    let take = content.len().min(MAX);
    write_pkt(out, &content[..take])?;
    content = &content[take..];
  }
  Ok(())
}

fn process_loop() -> i32 {
  let stdin = std::io::stdin();
  let stdout = std::io::stdout();
  let mut input = stdin.lock();
  let mut output = stdout.lock();
  match serve_process(&mut input, &mut output) {
    Ok(()) => 0,
    // Git closing the pipe at the end of the session arrives as an IO error
    // on the next read; that is the normal shutdown, not a failure.
    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
    Err(_) => 1,
  }
}

fn serve_process(input: &mut impl Read, output: &mut impl Write) -> std::io::Result<()> {
  // Handshake: client announces itself and its versions; we pick version 2.
  let hello = read_kv_until_flush(input)?;
  let client_ok = hello
    .first()
    .is_some_and(|(k, _)| k == "git-filter-client" || k.starts_with("git-filter"));
  // The first packet is bare "git-filter-client", which has no `=`; accept a
  // hello whose version list includes 2 regardless of how it parsed.
  let _ = client_ok;
  write_text_pkt(output, "git-filter-server")?;
  write_text_pkt(output, "version=2")?;
  write_flush(output)?;

  let theirs = read_kv_until_flush(input)?;
  let wants = |cap: &str| theirs.iter().any(|(k, v)| k == "capability" && v == cap);
  if wants("clean") {
    write_text_pkt(output, "capability=clean")?;
  }
  if wants("smudge") {
    write_text_pkt(output, "capability=smudge")?;
  }
  write_flush(output)?;

  loop {
    let header = read_kv_until_flush(input)?;
    let value = |key: &str| {
      header
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
    };
    let command = value("command").unwrap_or_default();
    let pathname = value("pathname").unwrap_or_default();
    let content = read_content_until_flush(input)?;

    let result: Result<Vec<u8>, ()> = match command.as_str() {
      "clean" => Ok(clean_bytes(&pathname, &content)),
      "smudge" => Ok(smudge_bytes(&pathname, &content)),
      _ => Err(()),
    };
    match result {
      Ok(bytes) => {
        write_text_pkt(output, "status=success")?;
        write_flush(output)?;
        write_content(output, &bytes)?;
        write_flush(output)?;
        // The empty list that keeps the success status unchanged.
        write_flush(output)?;
      }
      Err(()) => {
        write_text_pkt(output, "status=error")?;
        write_flush(output)?;
      }
    }
  }
}

/// The clean decision on in-memory content: the pointer for base-identical
/// paths (answered by the daemon), pass-through for content that already is a
/// pointer, a fresh pointer otherwise.
fn clean_bytes(path: &str, content: &[u8]) -> Vec<u8> {
  if let Some(pointer) = daemon_clean_answer(path) {
    return pointer.into_bytes();
  }
  if content.len() as u64 <= gfs_types::lfs::MAX_POINTER_SIZE && parse_pointer(content).is_some() {
    return content.to_vec();
  }
  use sha2::Digest as _;
  let digest = sha2::Sha256::digest(content);
  let oid = ObjectId::from_raw(HashAlgorithm::LfsSha256, &digest).expect("a sha256 digest");
  LfsPointer {
    oid,
    size: content.len() as u64,
  }
  .to_pointer_text()
  .into_bytes()
}

/// The smudge decision on in-memory content: hydrate a pointer through the
/// daemon, pass anything else (including an unavailable object's pointer)
/// through unchanged.
fn smudge_bytes(path: &str, content: &[u8]) -> Vec<u8> {
  let Some(pointer) = parse_pointer(content) else {
    return content.to_vec();
  };
  let answer = ask(&gfs_mount::control::Request::LfsSmudge {
    path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
    oid: pointer.oid.to_qualified(),
  });
  match answer {
    Some(gfs_mount::control::Response::LfsSmudge { path: cached }) => {
      std::fs::read(cached).unwrap_or_else(|_| content.to_vec())
    }
    _ => content.to_vec(),
  }
}

fn daemon_clean_answer(path: &str) -> Option<String> {
  ask(&gfs_mount::control::Request::LfsClean {
    path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
  })
  .and_then(|r| match r {
    gfs_mount::control::Response::LfsClean { pointer } => pointer,
    _ => None,
  })
}

fn clean(path: &str) -> i32 {
  // Ask before reading stdin: for a base-identical path the answer makes the
  // content irrelevant, and the drain below never buffers it.
  let daemon_pointer = ask(&gfs_mount::control::Request::LfsClean {
    path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
  })
  .and_then(|r| match r {
    gfs_mount::control::Response::LfsClean { pointer } => pointer,
    _ => None,
  });

  if let Some(pointer) = daemon_pointer {
    if drain_stdin().is_err() {
      return 1;
    }
    return emit(pointer.as_bytes());
  }

  // Hash-and-emit, the git-lfs clean contract. Streaming: only the digest,
  // the length, and a pointer-sized head (for the pass-through check below)
  // are kept, so a multi-gigabyte file costs no memory.
  let mut hasher = Sha256State::new();
  let mut head = Vec::new();
  let stdin = std::io::stdin();
  let mut stdin = stdin.lock();
  let mut buf = [0u8; 64 * 1024];
  loop {
    match stdin.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => {
        if head.len() < gfs_types::lfs::MAX_POINTER_SIZE as usize {
          let take = (gfs_types::lfs::MAX_POINTER_SIZE as usize - head.len()).min(n);
          head.extend_from_slice(&buf[..take]);
        }
        hasher.update(&buf[..n]);
      }
      Err(_) => return 1,
    }
  }
  let (digest, len) = hasher.finish();

  // Already a pointer (a degraded entry being re-added): pass it through
  // unchanged rather than wrapping a pointer in a pointer.
  if len <= gfs_types::lfs::MAX_POINTER_SIZE && parse_pointer(&head).is_some() {
    return emit(&head);
  }

  let oid = ObjectId::from_raw(HashAlgorithm::LfsSha256, &digest).expect("a sha256 digest");
  let pointer = LfsPointer { oid, size: len };
  emit(pointer.to_pointer_text().as_bytes())
}

fn smudge(path: &str) -> i32 {
  let mut input = Vec::new();
  if std::io::stdin().read_to_end(&mut input).is_err() {
    return 1;
  }
  let Some(pointer) = parse_pointer(&input) else {
    // Not a pointer: pass through, matching git-lfs. This also covers content
    // an earlier smudge already expanded.
    return emit(&input);
  };

  let answer = ask(&gfs_mount::control::Request::LfsSmudge {
    path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
    oid: pointer.oid.to_qualified(),
  });
  match answer {
    Some(gfs_mount::control::Response::LfsSmudge { path: cached }) => {
      match std::fs::File::open(&cached) {
        Ok(mut file) => {
          let stdout = std::io::stdout();
          let mut stdout = stdout.lock();
          if std::io::copy(&mut file, &mut stdout).is_err() {
            return 1;
          }
          0
        }
        Err(_) => emit(&input),
      }
    }
    // Degraded or unreachable: the pointer itself is the truthful content.
    _ => emit(&input),
  }
}

fn emit(bytes: &[u8]) -> i32 {
  let stdout = std::io::stdout();
  let mut stdout = stdout.lock();
  if stdout.write_all(bytes).is_err() {
    return 1;
  }
  0
}

fn drain_stdin() -> std::io::Result<()> {
  let stdin = std::io::stdin();
  let mut stdin = stdin.lock();
  let mut buf = [0u8; 64 * 1024];
  loop {
    match stdin.read(&mut buf)? {
      0 => return Ok(()),
      _ => continue,
    }
  }
}

fn ask(request: &gfs_mount::control::Request) -> Option<gfs_mount::control::Response> {
  let socket = find_control_socket()?;
  gfs_mount::control::call(&socket, request).ok()
}

/// Walk from the worktree root to the daemon's control socket, exactly as the
/// fsmonitor hook does. Git runs filters with the worktree root as cwd.
fn find_control_socket() -> Option<PathBuf> {
  let cwd = std::env::current_dir().ok()?;
  let dotgit = cwd.join(".git");
  let git_dir = if dotgit.is_dir() {
    dotgit
  } else {
    let git_file = std::fs::read_to_string(&dotgit).ok()?;
    PathBuf::from(git_file.trim().strip_prefix("gitdir: ")?)
  };
  let facts = std::fs::read_to_string(git_dir.join("gfs.json")).ok()?;
  let facts: serde_json::Value = serde_json::from_str(&facts).ok()?;
  Some(PathBuf::from(facts.get("control_socket")?.as_str()?))
}

/// Streaming SHA-256, wrapping the `sha2` crate the workspace already carries.
struct Sha256State {
  hasher: sha2::Sha256,
  len: u64,
}

impl Sha256State {
  fn new() -> Self {
    use sha2::Digest as _;
    Sha256State {
      hasher: sha2::Sha256::new(),
      len: 0,
    }
  }

  fn update(&mut self, bytes: &[u8]) {
    use sha2::Digest as _;
    self.hasher.update(bytes);
    self.len += bytes.len() as u64;
  }

  fn finish(self) -> ([u8; 32], u64) {
    use sha2::Digest as _;
    (self.hasher.finalize().into(), self.len)
  }
}
