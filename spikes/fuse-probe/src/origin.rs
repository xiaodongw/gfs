//! A stand-in for the XVFS snapshot/blob API.
//!
//! The M0.2 question is about the *filesystem*, not the server, so this is
//! deliberately the smallest thing that makes a read genuinely remote: a local
//! TCP server with injectable latency that counts what it was asked for. What
//! matters is that a `read(2)` inside the mount blocks on a socket, because
//! that is what makes FUSE callback threading, cancellation, and concurrency
//! observable at all.

use anyhow::Result;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
pub struct OriginStats {
  pub blob_requests: AtomicU64,
  pub bytes_served: AtomicU64,
}

pub struct Origin {
  pub port: u16,
  pub stats: Arc<OriginStats>,
}

/// Serve `name -> content` over a trivial line protocol: `GET <name>\n` returns
/// `<len>\n<bytes>`.
pub fn start(blobs: HashMap<String, Vec<u8>>, latency: Duration) -> Result<Origin> {
  let listener = TcpListener::bind("127.0.0.1:0")?;
  let port = listener.local_addr()?.port();
  let stats = Arc::new(OriginStats::default());
  let blobs = Arc::new(blobs);

  let stats_thread = Arc::clone(&stats);
  std::thread::spawn(move || {
    for stream in listener.incoming().flatten() {
      let blobs = Arc::clone(&blobs);
      let stats = Arc::clone(&stats_thread);
      std::thread::spawn(move || {
        let _ = serve_one(stream, &blobs, &stats, latency);
      });
    }
  });
  Ok(Origin { port, stats })
}

fn serve_one(
  stream: TcpStream,
  blobs: &HashMap<String, Vec<u8>>,
  stats: &OriginStats,
  latency: Duration,
) -> Result<()> {
  let mut reader = BufReader::new(stream.try_clone()?);
  let mut stream = stream;
  loop {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
      return Ok(());
    }
    let Some(name) = line.trim().strip_prefix("GET ") else {
      return Ok(());
    };
    if !latency.is_zero() {
      std::thread::sleep(latency);
    }
    match blobs.get(name) {
      Some(data) => {
        stats.blob_requests.fetch_add(1, Ordering::Relaxed);
        stats
          .bytes_served
          .fetch_add(data.len() as u64, Ordering::Relaxed);
        writeln!(stream, "{}", data.len())?;
        stream.write_all(data)?;
      }
      None => writeln!(stream, "-1")?,
    }
    stream.flush()?;
  }
}

/// Client side, used by the filesystem's read path.
///
/// One connection per fetch, deliberately. An earlier version held a single
/// pooled connection behind a mutex, and every concurrency measurement it
/// produced was wrong in the flattering direction: threads blocked on the mutex
/// were counted as concurrent fetches, so peak concurrency read as 16 while the
/// wall clock stayed exactly serial. A real client would pool connections; a
/// probe that is measuring concurrency must not put a lock on the path it is
/// measuring.
pub struct OriginClient {
  port: u16,
}

impl OriginClient {
  pub fn new(port: u16) -> Self {
    OriginClient { port }
  }

  pub fn fetch(&self, name: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let stream = TcpStream::connect(("127.0.0.1", self.port))?;
    let mut reader = BufReader::new(stream);
    reader
      .get_mut()
      .write_all(format!("GET {name}\n").as_bytes())?;
    reader.get_mut().flush()?;

    let mut len_line = String::new();
    reader.read_line(&mut len_line)?;
    let len: i64 = len_line.trim().parse()?;
    anyhow::ensure!(len >= 0, "origin has no blob named {name}");
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf)?;
    Ok(buf)
  }
}
