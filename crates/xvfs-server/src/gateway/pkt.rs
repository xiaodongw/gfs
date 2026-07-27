//! pkt-line framing, and the one place the gateway inspects Git's wire bytes.
//!
//! The gateway is deliberately not a protocol implementation -- stock
//! `upload-pack` is, and DESIGN.md section 7.2 explains why reimplementing it
//! would add compatibility risk for no benefit. But two decisions cannot be
//! delegated to the child process:
//!
//! * a request's `filter` line has to be validated **before** a subprocess
//!   exists, because Git's `uploadpackfilter.<family>.allow` granularity is
//!   coarser than XVFS policy (M0.3 measured that allowing the `blob` family
//!   permits `blob:limit=<n>` too);
//! * a ref advertisement has to be checked for the reserved `refs/xvfs/`
//!   namespace on its way out, because `transfer.hideRefs` is a *list* and a
//!   repository's own configuration can append a negating `!` entry.
//!
//! Both need pkt-line parsing, so it lives here rather than being written twice.
//!
//! # Why the scanner runs on the advertisement only
//!
//! [`AdvertisementScanner`] is applied to `GET /info/refs` responses and to
//! nothing else. That is a correctness requirement, not an optimization: a
//! `POST /git-upload-pack` response carries a **packfile**, whose bytes are
//! arbitrary repository content. A blob containing the ASCII `refs/xvfs/` is
//! ordinary -- this file contains it -- and a scanner over pack bytes would
//! abort a legitimate clone. The advertisement, by contrast, is pkt-lines of ref
//! names and capabilities and never contains object content.

use xvfs_types::error::{ErrorCode, XvfsError};

/// Largest pkt-line Git will emit, including the four-byte length prefix.
pub const MAX_PKT_LINE: usize = 65520;

/// Frame a payload as a pkt-line.
pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
  let mut framed = format!("{:04x}", payload.len() + 4).into_bytes();
  framed.extend_from_slice(payload);
  framed
}

/// The flush packet.
pub const FLUSH_PKT: &[u8] = b"0000";

/// One decoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
  /// A data packet's payload, without the length prefix.
  Data(Vec<u8>),
  /// `0000`.
  Flush,
  /// `0001`, the v2 section delimiter.
  Delim,
  /// `0002`, the v2 response end.
  ResponseEnd,
}

/// Decode a complete pkt-line stream.
///
/// Used on request bodies, which are bounded by the body limit before they get
/// here. A malformed length prefix stops the scan rather than being guessed at:
/// the caller treats a short read as "the rest is not parseable", and the child
/// process -- the actual protocol implementation -- is the one that rejects it.
pub fn decode(mut data: &[u8]) -> Result<Vec<Packet>, XvfsError> {
  let mut out = Vec::new();
  while data.len() >= 4 {
    let Some((packet, consumed)) = decode_one(data)? else {
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        "truncated pkt-line",
      ));
    };
    out.push(packet);
    data = &data[consumed..];
  }
  Ok(out)
}

/// Decode the packet at the head of `data`, returning it and the bytes consumed.
///
/// `Ok(None)` means the framing is valid but the buffer ends mid-packet, so more
/// bytes will complete it. An `Err` means the framing itself is wrong and more
/// bytes cannot help. The streaming scanner has to tell those apart, which is
/// why the incomplete case is not an error.
fn decode_one(data: &[u8]) -> Result<Option<(Packet, usize)>, XvfsError> {
  let malformed = || XvfsError::new(ErrorCode::InvalidArgument, "malformed pkt-line framing");
  let header = std::str::from_utf8(&data[..4]).map_err(|_| malformed())?;
  let len = usize::from_str_radix(header, 16).map_err(|_| malformed())?;
  match len {
    0 => Ok(Some((Packet::Flush, 4))),
    1 => Ok(Some((Packet::Delim, 4))),
    2 => Ok(Some((Packet::ResponseEnd, 4))),
    // A length of 3 claims a packet shorter than its own header.
    3 => Err(malformed()),
    n if n > MAX_PKT_LINE => Err(malformed()),
    n if n > data.len() => Ok(None),
    n => Ok(Some((Packet::Data(data[4..n].to_vec()), n))),
  }
}

/// Incrementally scan an outgoing ref advertisement for reserved refs.
///
/// Feeds bytes through unchanged and buffers only a partial trailing packet, so
/// a 100 000-ref advertisement streams with `MAX_PKT_LINE` of overhead rather
/// than being materialized.
///
/// Fail-closed is the whole point. The protected configuration already sets
/// `transfer.hideRefs`, and M5.2 tests that it works; this is the check that
/// survives a configuration mistake, a Git behaviour change, or a repository
/// that appends `!refs/xvfs/` to un-hide the namespace. A leak aborts the
/// response mid-stream, which a client reports as a broken transfer -- strictly
/// better than serving another job's retained commit as a discoverable ref.
#[derive(Debug)]
pub struct AdvertisementScanner {
  hidden: Vec<String>,
  pending: Vec<u8>,
}

impl AdvertisementScanner {
  pub fn new(hidden_prefixes: &[String]) -> Self {
    AdvertisementScanner {
      hidden: hidden_prefixes.to_vec(),
      pending: Vec::new(),
    }
  }

  /// Accept a chunk, returning the prefix that has been fully scanned.
  ///
  /// The returned bytes are safe to forward. Anything held back is an
  /// incomplete packet and is emitted by a later call or by [`Self::finish`].
  pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, XvfsError> {
    self.pending.extend_from_slice(chunk);
    let mut scanned = 0usize;
    while self.pending.len() - scanned >= 4 {
      // `None` is the mid-packet case: stop, keep the tail, wait for more.
      let Some((packet, consumed)) = decode_one(&self.pending[scanned..])? else {
        break;
      };
      if let Packet::Data(payload) = &packet {
        self.check(payload)?;
      }
      scanned += consumed;
    }
    let ready = self.pending.drain(..scanned).collect();
    if self.pending.len() > MAX_PKT_LINE {
      // Cannot happen with well-formed input: an incomplete packet is shorter
      // than one maximum pkt-line by construction. If it does, the framing is
      // wrong and buffering more would be an unbounded allocation.
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        "advertisement packet exceeds the pkt-line maximum",
      ));
    }
    Ok(ready)
  }

  /// Flush the tail after the child's stdout closes.
  pub fn finish(&mut self) -> Result<Vec<u8>, XvfsError> {
    Ok(std::mem::take(&mut self.pending))
  }

  /// Reject an advertised ref inside a reserved namespace.
  ///
  /// An advertisement line is `<oid> SP <refname>` optionally followed by
  /// `NUL<capabilities>` on the first one. Capabilities are checked too --
  /// `symref=HEAD:refs/xvfs/...` would disclose the namespace just as
  /// effectively as a ref line.
  fn check(&self, payload: &[u8]) -> Result<(), XvfsError> {
    for prefix in &self.hidden {
      if contains(payload, prefix.as_bytes()) {
        return Err(XvfsError::new(
          ErrorCode::PermissionDenied,
          "the ref advertisement contained a reserved namespace; refusing to serve it",
        ));
      }
    }
    Ok(())
  }
}

/// Substring search over bytes.
///
/// Naive on purpose: the needles are ref prefixes of a dozen bytes and the
/// haystack is one pkt-line, so a specialized searcher would add a dependency to
/// the trust boundary for an unmeasurable gain.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
  !needle.is_empty()
    && haystack.len() >= needle.len()
    && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn framing_round_trips_and_names_the_special_packets() {
    let framed = pkt_line(b"want abc\n");
    assert_eq!(&framed[..4], b"000d");
    assert_eq!(
      decode(&framed).unwrap(),
      vec![Packet::Data(b"want abc\n".to_vec())]
    );
    assert_eq!(
      decode(b"000000010002").unwrap(),
      vec![Packet::Flush, Packet::Delim, Packet::ResponseEnd]
    );
  }

  #[test]
  fn malformed_framing_is_an_error_rather_than_a_guess() {
    // A length prefix that is not hex, a length shorter than the header, and a
    // length beyond Git's maximum are all framing errors. Guessing at any of
    // them would hand the child process bytes the gateway did not understand.
    for bad in [
      &b"zzzz"[..],
      &b"0003"[..],
      &format!("{:04x}", MAX_PKT_LINE + 1).into_bytes()[..],
    ] {
      assert!(decode(bad).is_err(), "{bad:?} must be refused");
    }
    // A truncated final packet is also an error rather than a silent drop.
    assert!(decode(b"0010short").is_err());
  }

  #[test]
  fn the_scanner_forwards_bytes_and_holds_only_a_partial_packet() {
    let mut scanner = AdvertisementScanner::new(&["refs/xvfs/".to_owned()]);
    let mut stream = Vec::new();
    stream.extend_from_slice(&pkt_line(b"# service=git-upload-pack\n"));
    stream.extend_from_slice(FLUSH_PKT);
    stream.extend_from_slice(&pkt_line(
      b"1111111111111111111111111111111111111111 refs/heads/main\0agent=git/2.53.0\n",
    ));
    stream.extend_from_slice(FLUSH_PKT);

    // Split at every offset: the scanner must be indifferent to chunking.
    for split in 1..stream.len() {
      let mut scanner = AdvertisementScanner::new(&["refs/xvfs/".to_owned()]);
      let mut out = scanner.push(&stream[..split]).unwrap();
      out.extend(scanner.push(&stream[split..]).unwrap());
      out.extend(scanner.finish().unwrap());
      assert_eq!(out, stream, "chunk boundary at {split} changed the bytes");
    }

    let out = scanner.push(&stream).unwrap();
    assert_eq!(out, stream);
    assert!(scanner.finish().unwrap().is_empty());
  }

  #[test]
  fn a_leaked_lease_anchor_aborts_the_advertisement() {
    // The failure this exists for: `transfer.hideRefs` is a list and a
    // repository can append `!refs/xvfs/` to negate the gateway's entry. If
    // that ever works, the advertisement must break rather than disclose
    // another job's retained commit.
    let mut scanner = AdvertisementScanner::new(&["refs/xvfs/".to_owned()]);
    let leak = pkt_line(b"1111111111111111111111111111111111111111 refs/xvfs/mounts/m-1\n");
    let err = scanner.push(&leak).unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
  }

  #[test]
  fn a_reserved_ref_hidden_in_a_capability_is_also_caught() {
    // `symref=HEAD:refs/xvfs/...` discloses the namespace without ever
    // appearing as a ref line.
    let mut scanner = AdvertisementScanner::new(&["refs/xvfs/".to_owned()]);
    let line = pkt_line(
      b"1111111111111111111111111111111111111111 HEAD\0symref=HEAD:refs/xvfs/mounts/m-1\n",
    );
    assert!(scanner.push(&line).is_err());
  }
}
