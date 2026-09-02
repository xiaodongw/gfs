//! The upstream `/info/lfs/objects/batch` client (download direction).
//!
//! Network egress runs **`curl` as a subprocess**, for the same reason
//! [`crate::mirror`] runs stock Git instead of libgit2's network stack: the
//! server process links no TLS library, and replacing a battle-tested client
//! with an in-process one would be all risk and no capability. The discipline
//! matches the mirror's: cleared environment, and nothing sensitive on the
//! command line — every request is described entirely by curl's
//! config-on-stdin (`-K -`), so the credential and the per-object hrefs
//! (which carry signed query tokens on the big hosts) never appear in argv,
//! `ps`, or an error message. The batch request body, which holds only oids
//! and sizes, rides a temp file the config points at.
//!
//! Only the `basic` transfer adapter is spoken, which is the one every LFS
//! server must support per the API spec.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{ObjectId, RepositoryId};

use super::LfsStore;

/// An object the caller wants in the store: the oid/size pair a pointer names.
#[derive(Clone, Debug)]
pub struct WantedObject {
  pub oid: ObjectId,
  pub size: u64,
}

/// What one download pass achieved. Objects that could not be fetched are
/// reported, not failed: an unfetchable object degrades its entries to
/// pointers (ADR 0012), and the caller decides how loudly to say so.
#[derive(Debug, Default)]
pub struct DownloadReport {
  pub fetched: usize,
  /// `(oid hex, reason)` per object left unfetched.
  pub degraded: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct BatchClient {
  curl_binary: PathBuf,
}

impl BatchClient {
  pub fn new(curl_binary: impl Into<PathBuf>) -> Self {
    BatchClient {
      curl_binary: curl_binary.into(),
    }
  }

  /// The batch endpoint git-lfs would derive for an upstream remote URL, or
  /// `None` for a transport (`file://`, ssh, a local path) that has none.
  pub fn endpoint_for(upstream_url: &str) -> Option<String> {
    let url = upstream_url.trim().trim_end_matches('/');
    if !url.starts_with("https://") && !url.starts_with("http://") {
      return None;
    }
    // git-lfs: `<remote>/info/lfs` when the remote ends in `.git`, otherwise
    // `<remote>.git/info/lfs`.
    Some(if url.ends_with(".git") {
      format!("{url}/info/lfs")
    } else {
      format!("{url}.git/info/lfs")
    })
  }

  /// Ask the batch API for `wanted` and put every object it serves into the
  /// store. The store verifies each object against its address before
  /// publication, so a lying or truncating upstream degrades entries rather
  /// than corrupting the store.
  pub fn download(
    &self,
    endpoint: &str,
    credential: Option<&str>,
    wanted: &[WantedObject],
    store: &LfsStore,
    repository: &RepositoryId,
  ) -> Result<DownloadReport, GfsError> {
    if wanted.is_empty() {
      return Ok(DownloadReport::default());
    }

    let request = serde_json::json!({
      "operation": "download",
      "transfers": ["basic"],
      "objects": wanted
        .iter()
        .map(|w| serde_json::json!({ "oid": w.oid.to_hex(), "size": w.size }))
        .collect::<Vec<_>>(),
    });
    let body = TempFile::holding(store, request.to_string().as_bytes())?;

    let mut config = String::new();
    config.push_str("request = \"POST\"\n");
    config.push_str("header = \"Accept: application/vnd.git-lfs+json\"\n");
    config.push_str("header = \"Content-Type: application/vnd.git-lfs+json\"\n");
    if let Some(secret) = credential {
      config.push_str(&format!("header = {}\n", curl_quote(&basic_auth(secret))));
    }
    config.push_str(&format!(
      "data-binary = {}\n",
      curl_quote(&format!("@{}", body.path.display()))
    ));
    let batch_url = format!("{}/objects/batch", endpoint.trim_end_matches('/'));
    config.push_str(&format!("url = {}\n", curl_quote(&batch_url)));

    let response = self.run_curl(&config)?;
    let response: BatchResponse = serde_json::from_slice(&response).map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("upstream batch response is not LFS JSON: {e}"),
      )
    })?;

    let by_oid: HashMap<&str, &BatchObject> = response
      .objects
      .iter()
      .map(|o| (o.oid.as_str(), o))
      .collect();

    let mut report = DownloadReport::default();
    for want in wanted {
      let hex = want.oid.to_hex();
      let outcome = match by_oid.get(hex.as_str()) {
        None => Err("upstream batch response omitted the object".to_owned()),
        Some(BatchObject {
          error: Some(err), ..
        }) => Err(format!("upstream refused: {} ({})", err.message, err.code)),
        Some(BatchObject { actions, .. }) => {
          match actions.as_ref().and_then(|a| a.download.as_ref()) {
            // No download action and no error is the spec's way of saying the
            // object exists but is not currently retrievable.
            None => Err("upstream offered no download action".to_owned()),
            Some(action) => self
              .fetch_object(action, want, store, repository)
              .map_err(|e| e.message),
          }
        }
      };
      match outcome {
        Ok(()) => report.fetched += 1,
        Err(reason) => report.degraded.push((hex, reason)),
      }
    }
    Ok(report)
  }

  /// Upload every one of `objects` the upstream says it lacks, from the
  /// store, with the caller's credential. Returns how many were transferred.
  ///
  /// Unlike downloads, a failure here is an error, not a degradation: the ref
  /// about to be pushed references these objects, and an upstream branch
  /// whose LFS objects are missing is broken for every other clone.
  pub fn upload(
    &self,
    endpoint: &str,
    credential: Option<&str>,
    objects: &[WantedObject],
    store: &LfsStore,
    repository: &RepositoryId,
  ) -> Result<usize, GfsError> {
    if objects.is_empty() {
      return Ok(0);
    }

    let request = serde_json::json!({
      "operation": "upload",
      "transfers": ["basic"],
      "objects": objects
        .iter()
        .map(|w| serde_json::json!({ "oid": w.oid.to_hex(), "size": w.size }))
        .collect::<Vec<_>>(),
    });
    let body = TempFile::holding(store, request.to_string().as_bytes())?;

    let auth = credential.map(basic_auth);
    let mut config = String::new();
    config.push_str("request = \"POST\"\n");
    config.push_str("header = \"Accept: application/vnd.git-lfs+json\"\n");
    config.push_str("header = \"Content-Type: application/vnd.git-lfs+json\"\n");
    if let Some(auth) = &auth {
      config.push_str(&format!("header = {}\n", curl_quote(auth)));
    }
    config.push_str(&format!(
      "data-binary = {}\n",
      curl_quote(&format!("@{}", body.path.display()))
    ));
    config.push_str(&format!(
      "url = {}\n",
      curl_quote(&format!("{}/objects/batch", endpoint.trim_end_matches('/')))
    ));

    let response = self.run_curl(&config)?;
    let response: BatchResponse = serde_json::from_slice(&response).map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("upstream batch response is not LFS JSON: {e}"),
      )
    })?;
    let by_oid: HashMap<&str, &BatchObject> = response
      .objects
      .iter()
      .map(|o| (o.oid.as_str(), o))
      .collect();

    let mut transferred = 0;
    for want in objects {
      let hex = want.oid.to_hex();
      let answer = by_oid.get(hex.as_str()).ok_or_else(|| {
        GfsError::new(
          ErrorCode::Unavailable,
          format!("upstream batch response omitted {hex}"),
        )
      })?;
      if let Some(err) = &answer.error {
        return Err(GfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "upstream refused LFS object {hex}: {} ({})",
            err.message, err.code
          ),
        ));
      }
      let Some(actions) = &answer.actions else {
        // No actions on an upload answer means the upstream already has it.
        continue;
      };
      if let Some(action) = &actions.upload {
        let object_path = store
          .object_path_for(repository, &want.oid)
          .ok_or_else(|| {
            GfsError::new(
              ErrorCode::FailedPrecondition,
              format!(
                "LFS object {hex} is referenced by the branch but not in the \
               store; it cannot be uploaded"
              ),
            )
          })?;
        let mut config = String::new();
        config.push_str(&format!(
          "upload-file = {}\n",
          curl_quote(&object_path.display().to_string())
        ));
        for (name, value) in &action.header {
          if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(GfsError::invalid(
              "upstream action header contains a line break",
            ));
          }
          config.push_str(&format!(
            "header = {}\n",
            curl_quote(&format!("{name}: {value}"))
          ));
        }
        config.push_str(&format!("url = {}\n", curl_quote(&action.href)));
        self.run_curl(&config)?;
        transferred += 1;
      }
      if let Some(verify) = &actions.verify {
        let mut config = String::new();
        config.push_str("request = \"POST\"\n");
        config.push_str("header = \"Accept: application/vnd.git-lfs+json\"\n");
        config.push_str("header = \"Content-Type: application/vnd.git-lfs+json\"\n");
        for (name, value) in &verify.header {
          if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(GfsError::invalid(
              "upstream action header contains a line break",
            ));
          }
          config.push_str(&format!(
            "header = {}\n",
            curl_quote(&format!("{name}: {value}"))
          ));
        }
        config.push_str(&format!(
          "data = {}\n",
          curl_quote(&serde_json::json!({ "oid": hex, "size": want.size }).to_string())
        ));
        config.push_str(&format!("url = {}\n", curl_quote(&verify.href)));
        self.run_curl(&config)?;
      }
    }
    Ok(transferred)
  }

  /// Download one object through its `basic` action and publish it.
  fn fetch_object(
    &self,
    action: &BatchAction,
    want: &WantedObject,
    store: &LfsStore,
    repository: &RepositoryId,
  ) -> Result<(), GfsError> {
    let mut config = String::new();
    // Redirects are part of the basic adapter's contract on the big hosts
    // (API host to CDN); auth headers are not forwarded across hosts by curl
    // when the location changes origin, matching git-lfs.
    config.push_str("location\n");
    for (name, value) in &action.header {
      if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
        return Err(GfsError::invalid(
          "upstream action header contains a line break",
        ));
      }
      config.push_str(&format!(
        "header = {}\n",
        curl_quote(&format!("{name}: {value}"))
      ));
    }
    config.push_str(&format!("url = {}\n", curl_quote(&action.href)));

    let bytes = self.run_curl(&config)?;
    store.put(repository, &want.oid, &bytes)
  }

  /// Run curl described entirely by `config`, returning the response body.
  /// A transport failure or a non-2xx status is an error (`--fail`).
  fn run_curl(&self, config: &str) -> Result<Vec<u8>, GfsError> {
    let mut child = Command::new(&self.curl_binary)
      .env_clear()
      .env("PATH", "/usr/bin:/bin")
      .arg("--silent")
      .arg("--show-error")
      .arg("--fail")
      .arg("--max-time")
      .arg("600")
      .arg("--config")
      .arg("-")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| {
        GfsError::new(
          ErrorCode::Unavailable,
          format!("cannot run {}: {e}", self.curl_binary.display()),
        )
      })?;
    child
      .stdin
      .take()
      .expect("stdin was piped")
      .write_all(config.as_bytes())
      .map_err(|e| GfsError::new(ErrorCode::Unavailable, format!("writing curl config: {e}")))?;
    let out = child
      .wait_with_output()
      .map_err(|e| GfsError::new(ErrorCode::Unavailable, format!("waiting for curl: {e}")))?;
    if !out.status.success() {
      // curl's stderr does not echo config values, so this is safe to bound
      // and return; server-side paths and credentials are not in it.
      let stderr = String::from_utf8_lossy(&out.stderr);
      let bounded: String = stderr.chars().take(1000).collect();
      return Err(GfsError::new(
        ErrorCode::Unavailable,
        format!("LFS transfer failed: {}", bounded.trim()),
      ));
    }
    Ok(out.stdout)
  }
}

/// `Authorization: Basic <base64(credential)>` for a `user:token` credential.
fn basic_auth(credential: &str) -> String {
  format!(
    "Authorization: Basic {}",
    base64_standard(credential.as_bytes())
  )
}

/// Quote a value for a curl config file: double quotes, with `\`, `"`, and
/// control characters escaped, per `curl --config`'s documented grammar.
fn curl_quote(value: &str) -> String {
  let mut quoted = String::with_capacity(value.len() + 2);
  quoted.push('"');
  for c in value.chars() {
    match c {
      '"' => quoted.push_str("\\\""),
      '\\' => quoted.push_str("\\\\"),
      '\n' => quoted.push_str("\\n"),
      '\r' => quoted.push_str("\\r"),
      '\t' => quoted.push_str("\\t"),
      c => quoted.push(c),
    }
  }
  quoted.push('"');
  quoted
}

/// Standard (RFC 4648, padded) base64 — distinct from the URL-safe unpadded
/// alphabet `gfs_types::path` uses for path encoding, because HTTP Basic auth
/// is specified against this one.
fn base64_standard(input: &[u8]) -> String {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
  for chunk in input.chunks(3) {
    let b = [
      chunk[0],
      *chunk.get(1).unwrap_or(&0),
      *chunk.get(2).unwrap_or(&0),
    ];
    let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
    out.push(ALPHABET[(n >> 18) as usize & 63] as char);
    out.push(ALPHABET[(n >> 12) as usize & 63] as char);
    out.push(if chunk.len() > 1 {
      ALPHABET[(n >> 6) as usize & 63] as char
    } else {
      '='
    });
    out.push(if chunk.len() > 2 {
      ALPHABET[n as usize & 63] as char
    } else {
      '='
    });
  }
  out
}

/// A temp file under the store root (same filesystem, swept with the store),
/// removed on drop.
struct TempFile {
  path: PathBuf,
}

impl TempFile {
  fn holding(store: &LfsStore, content: &[u8]) -> Result<TempFile, GfsError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = store
      .root()
      .join(format!(".batch-{}-{n}.json", std::process::id()));
    std::fs::write(&path, content)
      .map_err(|e| GfsError::internal(format!("writing batch request body: {e}")))?;
    Ok(TempFile { path })
  }
}

impl Drop for TempFile {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.path);
  }
}

// ---------------------------------------------------------------------------
// The batch response, per the git-lfs batch API spec.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct BatchResponse {
  #[serde(default)]
  objects: Vec<BatchObject>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchObject {
  oid: String,
  #[serde(default)]
  actions: Option<BatchActions>,
  #[serde(default)]
  error: Option<BatchObjectError>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchActions {
  #[serde(default)]
  download: Option<BatchAction>,
  #[serde(default)]
  upload: Option<BatchAction>,
  #[serde(default)]
  verify: Option<BatchAction>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchAction {
  href: String,
  #[serde(default)]
  header: HashMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchObjectError {
  code: i64,
  message: String,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_endpoint_is_derived_the_way_git_lfs_derives_it() {
    assert_eq!(
      BatchClient::endpoint_for("https://github.com/org/repo.git").as_deref(),
      Some("https://github.com/org/repo.git/info/lfs")
    );
    assert_eq!(
      BatchClient::endpoint_for("https://github.com/org/repo/").as_deref(),
      Some("https://github.com/org/repo.git/info/lfs")
    );
    // No batch API to derive for non-HTTP transports.
    assert_eq!(
      BatchClient::endpoint_for("file:///srv/fixtures/basic.git"),
      None
    );
    assert_eq!(
      BatchClient::endpoint_for("git@github.com:org/repo.git"),
      None
    );
    assert_eq!(BatchClient::endpoint_for("/srv/local/repo.git"), None);
  }

  #[test]
  fn config_values_are_quoted_against_injection() {
    // A crafted href must not be able to smuggle a second config directive.
    assert_eq!(
      curl_quote("https://x/\"\nupload-file = \"/etc/shadow"),
      "\"https://x/\\\"\\nupload-file = \\\"/etc/shadow\""
    );
  }

  #[test]
  fn basic_auth_encodes_the_rfc_4648_test_vectors() {
    assert_eq!(base64_standard(b""), "");
    assert_eq!(base64_standard(b"f"), "Zg==");
    assert_eq!(base64_standard(b"fo"), "Zm8=");
    assert_eq!(base64_standard(b"foo"), "Zm9v");
    assert_eq!(base64_standard(b"foobar"), "Zm9vYmFy");
    assert_eq!(
      basic_auth("user:token"),
      "Authorization: Basic dXNlcjp0b2tlbg=="
    );
  }

  #[test]
  fn a_batch_response_parses_including_errors_and_missing_actions() {
    let json = r#"{
      "transfer": "basic",
      "objects": [
        {"oid": "aa", "size": 1, "actions": {"download": {"href": "https://cdn/x",
          "header": {"Authorization": "RemoteAuth sig"}}}},
        {"oid": "bb", "size": 2, "error": {"code": 404, "message": "not found"}},
        {"oid": "cc", "size": 3}
      ]
    }"#;
    let parsed: BatchResponse = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.objects.len(), 3);
    assert_eq!(
      parsed.objects[0]
        .actions
        .as_ref()
        .unwrap()
        .download
        .as_ref()
        .unwrap()
        .href,
      "https://cdn/x"
    );
    assert_eq!(parsed.objects[1].error.as_ref().unwrap().code, 404);
    assert!(parsed.objects[2].actions.is_none() && parsed.objects[2].error.is_none());
  }
}
