//! A minimal smart-HTTP server for the two read-side routes.
//!
//! Hand-written HTTP/1.1 on purpose (see Cargo.toml). It buffers rather than
//! streams, which a real gateway must not do — M5.1 owns streaming and
//! backpressure. What it does reproduce faithfully is the part M0.3 needs to
//! validate: route parsing, header handling, gzip request bodies, the
//! version-dependent framing, and the content types.

use crate::upload_pack::{GitProtocol, UploadPack, UploadPackPolicy};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Gateway {
    /// Directory holding bare repositories, addressed as `/<name>.git/...`.
    pub root: PathBuf,
    pub policy: UploadPackPolicy,
}

impl Gateway {
    /// Map a request path to a repository directory.
    ///
    /// Repository selection is the classic path-traversal sink, so the name is
    /// validated as a single component against an allow-listed character set
    /// rather than being canonicalized after the fact.
    pub fn resolve_repo(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() || name.len() > 128 {
            bail!("invalid repository name");
        }
        let stem = name.strip_suffix(".git").unwrap_or(name);
        let valid = !stem.is_empty()
            && stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            && !stem.contains("..")
            && !stem.starts_with('.');
        if !valid {
            bail!("invalid repository name {name:?}");
        }
        let path = self.root.join(format!("{stem}.git"));
        if !path.join("objects").is_dir() {
            bail!("no such repository");
        }
        Ok(path)
    }

    fn handle(&self, stream: &mut TcpStream) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };

        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let protocol = GitProtocol::from_header(header("git-protocol"));

        let result = (|| -> Result<(u16, &'static str, Vec<u8>)> {
            match (method.as_str(), segments.as_slice()) {
                ("GET", [repo, "info", "refs"]) => {
                    if !query.contains("service=git-upload-pack") {
                        // Dumb HTTP is not served at all; an unadvertised
                        // fallback would expose the object directory.
                        bail!("only service=git-upload-pack is supported");
                    }
                    let up = UploadPack::new(self.resolve_repo(repo)?, self.policy.clone())?;
                    Ok((
                        200,
                        "application/x-git-upload-pack-advertisement",
                        up.advertise(protocol)?,
                    ))
                }
                ("POST", [repo, "git-upload-pack"]) => {
                    let up = UploadPack::new(self.resolve_repo(repo)?, self.policy.clone())?;
                    let raw = read_body(&mut reader, header("content-length"), header("transfer-encoding"), self.policy.max_body_bytes)?;
                    let body = if header("content-encoding") == Some("gzip") {
                        up.decompress_body(&raw)?
                    } else {
                        raw
                    };
                    Ok((
                        200,
                        "application/x-git-upload-pack-result",
                        up.rpc(protocol, &body)?,
                    ))
                }
                _ => bail!("not found"),
            }
        })();

        match result {
            Ok((status, content_type, body)) => {
                let mut head = format!(
                    "HTTP/1.1 {status} OK\r\n\
                     Content-Type: {content_type}\r\n\
                     Content-Length: {}\r\n\
                     Cache-Control: no-cache, max-age=0, must-revalidate\r\n\
                     Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\n\
                     Pragma: no-cache\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                head.extend_from_slice(&body);
                stream.write_all(&head)?;
            }
            Err(e) => {
                // Error text is deliberately generic on the wire; the detail
                // goes to the server's own log. A repository path or an object
                // ID in an error body is an information leak.
                eprintln!("gateway: {method} {target}: {e:#}");
                let body = b"error\n";
                let head = format!(
                    "HTTP/1.1 400 Bad Request\r\n\
                     Content-Type: text/plain\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes())?;
                stream.write_all(body)?;
            }
        }
        stream.flush()?;
        Ok(())
    }
}

fn read_body(
    reader: &mut BufReader<TcpStream>,
    content_length: Option<&str>,
    transfer_encoding: Option<&str>,
    max: usize,
) -> Result<Vec<u8>> {
    if transfer_encoding.is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut out = Vec::new();
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line)?;
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .context("malformed chunk size")?;
            if size == 0 {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                break;
            }
            if out.len() + size > max {
                bail!("request body exceeded {max} bytes");
            }
            let mut buf = vec![0u8; size];
            reader.read_exact(&mut buf)?;
            out.extend_from_slice(&buf);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
        }
        return Ok(out);
    }

    let len: usize = content_length.unwrap_or("0").trim().parse().unwrap_or(0);
    if len > max {
        bail!("request body exceeded {max} bytes");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// A running gateway bound to an ephemeral port.
pub struct RunningGateway {
    pub port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningGateway {
    pub fn start(gateway: Gateway) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match incoming {
                    Ok(mut stream) => {
                        let g = Gateway {
                            root: gateway.root.clone(),
                            policy: gateway.policy.clone(),
                        };
                        // One thread per connection: adequate for a probe, and
                        // not a model for M5.1.
                        std::thread::spawn(move || {
                            if let Err(e) = g.handle(&mut stream) {
                                eprintln!("gateway connection error: {e:#}");
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(RunningGateway {
            port,
            stop,
            handle: Some(handle),
        })
    }

    pub fn url(&self, repo: &str) -> String {
        format!("http://127.0.0.1:{}/{repo}", self.port)
    }
}

impl Drop for RunningGateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
