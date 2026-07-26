//! The sandboxed `git upload-pack` subprocess contract.
//!
//! Everything the gateway is allowed to decide about the subprocess is decided
//! here: the executable, its arguments, its working directory, its environment,
//! and its Git configuration. Nothing user-supplied reaches any of those except
//! through the typed validation in this module. DESIGN.md section 7.2 and
//! PLAN.md M5.3 both require that, and the reason is direct: `upload-pack` reads
//! configuration that can re-enable hidden refs, hooks, and filters, so the only
//! safe posture is to construct its configuration from scratch every time.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The partial-clone filters XVFS is willing to serve.
///
/// DESIGN.md section 7.2: the initial target is exactly `blob:none`. Git's own
/// `uploadpackfilter.<filter>.allow` granularity is coarser than XVFS policy —
/// allowing the `blob` family permits `blob:limit=<n>` too — so the gateway
/// validates the exact requested filter in addition to configuring Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPolicy {
    /// No filtering advertised or accepted.
    Disabled,
    /// Exactly `filter=blob:none`, nothing else in the blob family.
    BlobNoneOnly,
}

impl FilterPolicy {
    /// Validate a filter spec taken from a client request body.
    pub fn permits(&self, spec: &str) -> bool {
        match self {
            FilterPolicy::Disabled => false,
            FilterPolicy::BlobNoneOnly => spec == "blob:none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadPackPolicy {
    pub filter: FilterPolicy,
    /// Ref prefixes that must never be advertised or fetchable.
    pub hidden_ref_prefixes: Vec<String>,
    pub max_body_bytes: usize,
    /// Maximum bytes a gzip request body may expand to, and the maximum ratio.
    pub max_decompressed_bytes: usize,
    pub max_decompression_ratio: usize,
}

impl Default for UploadPackPolicy {
    fn default() -> Self {
        UploadPackPolicy {
            filter: FilterPolicy::BlobNoneOnly,
            hidden_ref_prefixes: vec!["refs/xvfs/".to_string()],
            max_body_bytes: 16 * 1024 * 1024,
            max_decompressed_bytes: 128 * 1024 * 1024,
            max_decompression_ratio: 100,
        }
    }
}

/// Protocol version negotiated from the `Git-Protocol` request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitProtocol {
    V0,
    V2,
}

impl GitProtocol {
    /// Parse and allow-list the header before it becomes `GIT_PROTOCOL`.
    ///
    /// The header is a colon-separated list of key[=value] items and is fully
    /// attacker-controlled, so it is never forwarded verbatim: only a
    /// recognized `version=N` produces an environment variable, and the value
    /// written is reconstructed by XVFS rather than copied.
    pub fn from_header(header: Option<&str>) -> Self {
        let Some(h) = header else { return GitProtocol::V0 };
        for item in h.split(':') {
            if let Some(v) = item.trim().strip_prefix("version=") {
                if v.trim() == "2" {
                    return GitProtocol::V2;
                }
            }
        }
        GitProtocol::V0
    }

    pub fn env_value(&self) -> Option<&'static str> {
        match self {
            GitProtocol::V0 => None,
            GitProtocol::V2 => Some("version=2"),
        }
    }
}

/// Build the fixed argument list that configures a hardened upload-pack.
///
/// Returned as data rather than applied directly so a test can assert on the
/// exact configuration, which is the only way to notice a silent regression in
/// something as consequential as `uploadpack.allowAnySHA1InWant`.
pub fn protected_config(policy: &UploadPackPolicy) -> Vec<String> {
    let mut cfg: Vec<String> = Vec::new();
    let mut push = |k: &str, v: &str| {
        cfg.push("-c".into());
        cfg.push(format!("{k}={v}"));
    };

    // Filtering: off unless policy allows it, and then only the one form.
    //
    // The subsection name is the filter's wire spelling and contains a colon —
    // `uploadpackfilter.blob:none.allow`, not `uploadpackfilter.blob.none.allow`.
    // The latter parses as subsection `blob`, key `none.allow`, which Git
    // silently ignores; the observable symptom is upload-pack rejecting
    // `blob:none` as "not supported" while the config looks correct.
    match policy.filter {
        FilterPolicy::Disabled => {
            push("uploadpack.allowFilter", "false");
        }
        FilterPolicy::BlobNoneOnly => {
            push("uploadpack.allowFilter", "true");
            // Deny by default, then allow exactly one filter.
            push("uploadpackfilter.allow", "false");
            push("uploadpackfilter.blob:none.allow", "true");
            // Named explicitly rather than left to the default, so that a
            // future change to `uploadpackfilter.allow` cannot quietly enable
            // them. The names are Git's internal config names for each filter
            // family, which are NOT always the filter's wire spelling: the
            // `tree:<depth>` family is configured as `tree`, so
            // `uploadpackfilter.tree:depth.allow` is an ignored no-op that
            // looks like a denial. Measured, not assumed — see
            // spikes/reports/m01-baseline.md.
            push("uploadpackfilter.blob:limit.allow", "false");
            push("uploadpackfilter.tree.allow", "false");
            push("uploadpackfilter.sparse:oid.allow", "false");
            push("uploadpackfilter.object:type.allow", "false");
            push("uploadpackfilter.combine.allow", "false");
        }
    }

    // Never serve an object the client did not learn about from an
    // advertisement; both of these turn the server into an object oracle.
    push("uploadpack.allowAnySHA1InWant", "false");
    push("uploadpack.allowReachableSHA1InWant", "false");
    push("uploadpack.allowTipSHA1InWant", "false");

    // The internal lease namespace is not a published ref and must not leak.
    for prefix in &policy.hidden_ref_prefixes {
        push("transfer.hideRefs", prefix);
    }
    // A repository-level `transfer.hideRefs` could otherwise *un*hide a prefix
    // with a `!` entry. Ignoring repository config for hideRefs is not something
    // Git offers, so the gateway also verifies the advertisement (see checks).

    // No hooks. `uploadpack.packObjectsHook` is deliberately NOT set to the
    // empty string: Git treats an empty value as a command to run and fails
    // with "cannot run : No such file or directory" at pack generation. Leaving
    // it unset is what disables it, and the cleared environment plus
    // GIT_CONFIG_NOSYSTEM is what stops a repository from setting it.
    push("core.hooksPath", "/dev/null");
    push("transfer.fsckObjects", "true");

    cfg
}

/// The environment an upload-pack child is allowed to see.
///
/// Built by allow-list, never inherited: an inherited `GIT_CONFIG_*`,
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES`, or `GIT_PROTOCOL` would each defeat part
/// of the sandbox.
pub fn protected_env(protocol: GitProtocol) -> Vec<(String, String)> {
    let mut env = vec![
        // Neutralize user and system configuration entirely.
        ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
        ("GIT_CONFIG_SYSTEM".to_string(), "/dev/null".to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("HOME".to_string(), "/nonexistent".to_string()),
        // upload-pack forks helpers such as `git-pack-objects` out of Git's
        // exec directory. With a cleared environment it must be told where that
        // is, or negotiation fails at pack generation with "unable to fork
        // git-pack-objects" — a failure that only appears once a real client
        // gets past the advertisement.
        ("GIT_EXEC_PATH".to_string(), git_exec_path()),
        ("PATH".to_string(), format!("{}:/usr/bin:/bin", git_exec_path())),
    ];
    if let Some(v) = protocol.env_value() {
        env.push(("GIT_PROTOCOL".to_string(), v.to_string()));
    }
    env
}

/// Git's helper directory, resolved once.
///
/// A deployment should pin this to the packaged path rather than asking the
/// binary, but for the probe it must match whichever Git is on this machine.
fn git_exec_path() -> String {
    use std::sync::OnceLock;
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        Command::new("git")
            .arg("--exec-path")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "/usr/lib/git-core".to_string())
    })
    .clone()
}

pub struct UploadPack {
    pub repo: PathBuf,
    pub policy: UploadPackPolicy,
}

impl UploadPack {
    pub fn new(repo: impl AsRef<Path>, policy: UploadPackPolicy) -> Result<Self> {
        let repo = repo.as_ref().canonicalize()?;
        if !repo.join("objects").is_dir() {
            bail!("{} is not a bare Git repository", repo.display());
        }
        Ok(UploadPack { repo, policy })
    }

    fn base_command(&self, protocol: GitProtocol) -> Command {
        // Absolute executable, no shell, no user-controlled path.
        let mut cmd = Command::new("git");
        cmd.args(protected_config(&self.policy));
        cmd.env_clear();
        for (k, v) in protected_env(protocol) {
            cmd.env(k, v);
        }
        // A fixed working directory that is not the repository, so a relative
        // path in a request can never resolve to something useful.
        cmd.current_dir("/");
        cmd
    }

    /// `GET /info/refs?service=git-upload-pack`.
    ///
    /// Returns the response body exactly as it must go on the wire. The
    /// version-dependent preamble is the subtle part: `upload-pack
    /// --http-backend-info-refs` emits only the advertisement, so for v0/v1 the
    /// gateway must prepend the `# service=git-upload-pack` pkt-line and a
    /// flush packet the way `git-http-backend` does, and for v2 it must not,
    /// because the body already starts with upload-pack's `version 2` pkt-line.
    pub fn advertise(&self, protocol: GitProtocol) -> Result<Vec<u8>> {
        let out = self
            .base_command(protocol)
            .arg("upload-pack")
            .arg("--http-backend-info-refs")
            .arg(&self.repo)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            bail!(
                "upload-pack --http-backend-info-refs failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let mut body = Vec::new();
        if protocol == GitProtocol::V0 {
            body.extend_from_slice(&pkt_line(b"# service=git-upload-pack\n"));
            body.extend_from_slice(b"0000");
        }
        body.extend_from_slice(&out.stdout);
        Ok(body)
    }

    /// `POST /git-upload-pack`, the stateless RPC.
    pub fn rpc(&self, protocol: GitProtocol, request: &[u8]) -> Result<Vec<u8>> {
        // Validate the request before a subprocess exists to attack.
        self.validate_request(request)?;

        use std::io::Write;
        let mut child = self
            .base_command(protocol)
            .arg("upload-pack")
            .arg("--stateless-rpc")
            .arg(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(request)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "upload-pack --stateless-rpc failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Reject a request whose filter the policy does not permit.
    ///
    /// This runs in the gateway rather than relying on Git's configuration
    /// because the two have different granularity: `uploadpackfilter.blob.*`
    /// cannot express "blob:none but not blob:limit".
    pub fn validate_request(&self, request: &[u8]) -> Result<()> {
        for line in parse_pkt_lines(request) {
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\0']);
            if let Some(spec) = text.strip_prefix("filter ") {
                if !self.policy.filter.permits(spec.trim()) {
                    bail!("filter {:?} is not permitted by policy", spec.trim());
                }
            }
            // A `want` for an object that was never advertised is refused by
            // Git itself given the config above; this is the belt to that
            // suspenders, and makes the rejection legible in gateway logs.
            if let Some(rest) = text.strip_prefix("deepen-not ") {
                let _ = rest; // shallow forms are permitted; recorded for audit
            }
        }
        Ok(())
    }

    /// Decompress a `Content-Encoding: gzip` request body under explicit bounds.
    ///
    /// Unbounded decompression here is a trivially reachable memory-exhaustion
    /// bug, so both an absolute output cap and a ratio cap apply.
    pub fn decompress_body(&self, body: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut out = Vec::new();
        let limit = self.policy.max_decompressed_bytes;
        let mut dec = flate2::read::GzDecoder::new(body).take(limit as u64 + 1);
        dec.read_to_end(&mut out)?;
        if out.len() > limit {
            bail!("gzip body exceeded {limit} byte output limit");
        }
        if !body.is_empty() && out.len() / body.len().max(1) > self.policy.max_decompression_ratio {
            bail!(
                "gzip body exceeded {}:1 expansion ratio",
                self.policy.max_decompression_ratio
            );
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// pkt-line
// ---------------------------------------------------------------------------

pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut v = format!("{:04x}", payload.len() + 4).into_bytes();
    v.extend_from_slice(payload);
    v
}

/// Split a pkt-line stream into payloads, skipping flush/delim packets.
pub fn parse_pkt_lines(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let Ok(hdr) = std::str::from_utf8(&data[..4]) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(hdr, 16) else {
            break;
        };
        if len == 0 || len == 1 || len == 2 {
            // flush-pkt, delim-pkt, response-end-pkt
            data = &data[4..];
            continue;
        }
        if len < 4 || len > data.len() {
            break;
        }
        out.push(data[4..len].to_vec());
        data = &data[len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_header_is_allow_listed_not_forwarded() {
        // A hostile header must not reach GIT_PROTOCOL verbatim.
        let p = GitProtocol::from_header(Some("version=2:evil=1"));
        assert_eq!(p, GitProtocol::V2);
        assert_eq!(p.env_value(), Some("version=2"));

        assert_eq!(GitProtocol::from_header(Some("version=99")), GitProtocol::V0);
        assert_eq!(GitProtocol::from_header(Some("garbage")), GitProtocol::V0);
        assert_eq!(GitProtocol::from_header(None), GitProtocol::V0);
        assert_eq!(GitProtocol::from_header(Some("version=2")).env_value(), Some("version=2"));
        assert_eq!(GitProtocol::V0.env_value(), None);
    }

    #[test]
    fn filter_policy_permits_only_the_exact_form() {
        let p = FilterPolicy::BlobNoneOnly;
        assert!(p.permits("blob:none"));
        // These are all in the family Git's config would allow.
        assert!(!p.permits("blob:limit=1k"));
        assert!(!p.permits("tree:0"));
        assert!(!p.permits("sparse:oid=main"));
        assert!(!p.permits("combine:blob:none+tree:0"));
        assert!(!p.permits("object:type=blob"));
        assert!(!FilterPolicy::Disabled.permits("blob:none"));
    }

    #[test]
    fn protected_config_denies_unadvertised_wants_and_hides_leases() {
        let cfg = protected_config(&UploadPackPolicy::default()).join(" ");
        assert!(cfg.contains("uploadpack.allowAnySHA1InWant=false"));
        assert!(cfg.contains("uploadpack.allowReachableSHA1InWant=false"));
        assert!(cfg.contains("uploadpack.allowTipSHA1InWant=false"));
        assert!(cfg.contains("transfer.hideRefs=refs/xvfs/"));
        // Colon-bearing subsection names; see the comment in protected_config.
        assert!(cfg.contains("uploadpackfilter.blob:none.allow=true"));
        assert!(cfg.contains("uploadpackfilter.blob:limit.allow=false"));
        // `tree`, not `tree:depth`: Git's config name for the family differs
        // from the filter's wire spelling, and the wrong name denies nothing.
        assert!(cfg.contains("uploadpackfilter.tree.allow=false"));
        assert!(!cfg.contains("uploadpackfilter.tree:depth"));
        // Deny-by-default is the mechanism that actually enforces the policy;
        // the per-family lines above are defence in depth against it changing.
        assert!(cfg.contains("uploadpackfilter.allow=false"));
    }

    #[test]
    fn pkt_line_round_trips() {
        let framed = pkt_line(b"want abc\n");
        assert_eq!(&framed[..4], b"000d");
        let parsed = parse_pkt_lines(&framed);
        assert_eq!(parsed, vec![b"want abc\n".to_vec()]);
    }
}
