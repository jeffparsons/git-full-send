//! Splitting a receive-pack stream into protocol overhead and payload
//! (ADR-0017).
//!
//! `bytes_out` as one lump cannot tell a 3.1 MB ref advertisement from 3.1 MB of
//! pack data, which is exactly the confusion that made a repo's ref count look
//! like a transfer problem. Both directions of the exchange are **pkt-line**
//! framed, and in both the interesting boundary is the **first flush-pkt**:
//!
//! ```text
//! server → client   ref advertisement · 0000 · … · report-status
//! client → server   ref-update commands · 0000 · the pack
//! ```
//!
//! So one counter serves both ends and both directions: feed it the bytes as they
//! pass, and it reports how many landed before the flush (the advertisement, or
//! the commands), how many pkt-lines that was (on the advertising side, the
//! **ref count**), and how many landed after (the pack, or the report).
//!
//! It is a state machine over the 4-byte length headers rather than a parser, so
//! it never buffers the stream and does not care where chunk boundaries fall.

/// The split of one direction of a receive-pack exchange.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct WireCounts {
    /// Every byte that passed, whether framing was understood or not.
    pub total: u64,
    /// Bytes up to and including the first flush-pkt: the ref advertisement in
    /// the server's outbound direction, the ref-update commands in the client's.
    pub pre_flush: u64,
    /// pkt-lines seen before that flush. On the advertising side this is the
    /// number of refs advertised — with the caveat that a repository with **no**
    /// refs still gets one placeholder pkt (git's `capabilities^{}` line), so the
    /// floor is 1 rather than 0.
    pub pre_flush_pkts: u64,
    /// Bytes after the first flush-pkt: the pack, or the report-status.
    pub post_flush: u64,
}

/// Where the counter is in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reading a 4-byte pkt-line length header.
    Header,
    /// Skipping the payload of the pkt-line whose header we just read.
    Payload,
    /// Past the first flush-pkt: everything from here is payload, uncounted by
    /// framing.
    AfterFlush,
    /// The framing stopped making sense (a malformed length). We stop pretending
    /// to understand it and attribute the remainder to `post_flush` — an honest
    /// "we don't know" rather than a fabricated split.
    Unframed,
}

/// Counts one direction of a receive-pack stream, splitting it at the first
/// flush-pkt. See the [module docs](self).
#[derive(Debug)]
pub struct FlushSplit {
    state: State,
    counts: WireCounts,
    /// Bytes of the current length header gathered so far (a header can straddle
    /// any number of reads).
    header: [u8; 4],
    header_len: usize,
    /// Payload bytes still to skip in the current pkt-line.
    remaining: usize,
}

impl Default for FlushSplit {
    fn default() -> Self {
        Self::new()
    }
}

impl FlushSplit {
    /// A counter positioned at the start of a stream.
    pub fn new() -> Self {
        Self {
            state: State::Header,
            counts: WireCounts::default(),
            header: [0; 4],
            header_len: 0,
            remaining: 0,
        }
    }

    /// Account for the next `bytes` of the stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.counts.total += bytes.len() as u64;
        let mut rest = bytes;
        while !rest.is_empty() {
            match self.state {
                // Everything from here is payload; no more framing to do.
                State::AfterFlush | State::Unframed => {
                    self.counts.post_flush += rest.len() as u64;
                    return;
                }
                State::Header => {
                    let want = 4 - self.header_len;
                    let take = want.min(rest.len());
                    self.header[self.header_len..self.header_len + take]
                        .copy_from_slice(&rest[..take]);
                    self.header_len += take;
                    self.counts.pre_flush += take as u64;
                    rest = &rest[take..];
                    if self.header_len < 4 {
                        return;
                    }
                    self.header_len = 0;
                    match parse_length(&self.header) {
                        // A flush-pkt ends the section. Its own 4 bytes are
                        // already counted as pre-flush, which is right: they are
                        // part of the advertisement/command block.
                        Some(0) => self.state = State::AfterFlush,
                        // Delim (`0001`) and response-end (`0002`) carry no
                        // payload. Not used by receive-pack today, but cheap to
                        // handle rather than mistake for a malformed header.
                        Some(1 | 2) => {
                            self.counts.pre_flush_pkts += 1;
                            self.state = State::Header;
                        }
                        // A real pkt-line: 4 header bytes plus its payload.
                        Some(len) if len >= 4 => {
                            self.counts.pre_flush_pkts += 1;
                            self.remaining = len - 4;
                            self.state = if self.remaining == 0 {
                                State::Header
                            } else {
                                State::Payload
                            };
                        }
                        // Length 3 is impossible, and a non-hex header means we
                        // were never looking at pkt-lines.
                        _ => self.state = State::Unframed,
                    }
                }
                State::Payload => {
                    let take = self.remaining.min(rest.len());
                    self.counts.pre_flush += take as u64;
                    self.remaining -= take;
                    rest = &rest[take..];
                    if self.remaining == 0 {
                        self.state = State::Header;
                    }
                }
            }
        }
    }

    /// The split so far.
    pub fn counts(&self) -> WireCounts {
        self.counts
    }
}

/// Copy `reader` → `writer` to EOF, returning the [`WireCounts`] of what passed.
///
/// The shared byte pump of both ends of the exchange: the server has always
/// moved the receive-pack stream through counting threads (ADR-0013), and the
/// client does the same across its interposing socketpair since ADR-0017. The
/// bytes are the identical raw stream — no framing is added, ADR-0005 is
/// unchanged — they are merely counted in transit.
///
/// Deliberately **not** `std::io::copy`: between a socket and a pipe on Linux
/// that takes a `splice`/`sendfile` zero-copy fast path, which deadlocked this
/// bidirectional exchange in issue #44 while passing on macOS, where no such
/// path exists. A plain buffered loop behaves identically on both platforms and
/// yields the counts for free.
///
/// Errors (a broken pipe once the peer has gone, say) end the copy and return
/// what passed so far — best-effort, like every other measurement here.
pub fn pump_splitting(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> WireCounts {
    let mut split = FlushSplit::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    break;
                }
                split.feed(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = writer.flush();
    split.counts()
}

/// Parse a 4-byte hex pkt-line length header.
fn parse_length(header: &[u8; 4]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    usize::from_str_radix(text, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pkt-line carrying `payload`, as git frames it.
    fn pkt(payload: &str) -> Vec<u8> {
        let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
        out.extend_from_slice(payload.as_bytes());
        out
    }

    /// A ref advertisement of `n` refs, then a flush, then a pack.
    fn advertisement(n: usize, pack: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        for i in 0..n {
            stream.extend(pkt(&format!(
                "{:040x} refs/heads/branch-{i}\n",
                0xabcdefu64 + i as u64
            )));
        }
        stream.extend_from_slice(b"0000");
        stream.extend_from_slice(pack);
        stream
    }

    #[test]
    fn splits_the_advertisement_from_what_follows_it() {
        let stream = advertisement(3, b"PACK-and-then-some");
        let mut split = FlushSplit::new();
        split.feed(&stream);
        let counts = split.counts();

        assert_eq!(counts.pre_flush_pkts, 3, "one pkt-line per ref");
        assert_eq!(counts.post_flush, "PACK-and-then-some".len() as u64);
        assert_eq!(
            counts.pre_flush,
            stream.len() as u64 - counts.post_flush,
            "the flush-pkt itself belongs to the advertisement",
        );
        assert_eq!(counts.total, stream.len() as u64);
    }

    /// The whole point is that this runs inside a byte pump, so a header — or a
    /// payload — may be split across any number of reads.
    #[test]
    fn chunk_boundaries_do_not_matter() {
        let stream = advertisement(4, b"0123456789");
        let whole = {
            let mut split = FlushSplit::new();
            split.feed(&stream);
            split.counts()
        };

        for chunk in [1, 2, 3, 5, 7, 64] {
            let mut split = FlushSplit::new();
            for part in stream.chunks(chunk) {
                split.feed(part);
            }
            assert_eq!(
                split.counts(),
                whole,
                "same counts when fed in chunks of {chunk}",
            );
        }
    }

    /// A flush-only conversation — what a liveness probe sends (ADR-0018).
    #[test]
    fn a_bare_flush_is_four_pre_flush_bytes_and_no_pkts() {
        let mut split = FlushSplit::new();
        split.feed(b"0000");
        assert_eq!(
            split.counts(),
            WireCounts {
                total: 4,
                pre_flush: 4,
                pre_flush_pkts: 0,
                post_flush: 0,
            },
        );
    }

    /// A prober that hangs up without saying anything at all.
    #[test]
    fn an_empty_stream_counts_nothing() {
        let split = FlushSplit::new();
        assert_eq!(split.counts(), WireCounts::default());
    }

    /// Nothing here is load-bearing, so a stream that isn't pkt-line framed must
    /// degrade to "we don't know" rather than invent a split.
    #[test]
    fn a_malformed_header_stops_the_split_instead_of_guessing() {
        let mut split = FlushSplit::new();
        split.feed(b"not-a-pkt-line-header-at-all");
        let counts = split.counts();
        assert_eq!(counts.total, 28);
        assert_eq!(counts.pre_flush, 4, "only the header we tried to read");
        assert_eq!(counts.post_flush, 24, "the rest is attributed, not framed");
        assert_eq!(counts.pre_flush_pkts, 0);
    }

    /// Payload after the flush is never re-framed, even when it happens to look
    /// like pkt-lines (a pack can contain anything).
    #[test]
    fn post_flush_bytes_are_never_reinterpreted() {
        let framed_looking = pkt("this looks framed but is pack data");
        let mut split = FlushSplit::new();
        split.feed(b"0000");
        split.feed(&framed_looking);
        assert_eq!(split.counts().pre_flush_pkts, 0);
        assert_eq!(split.counts().post_flush, framed_looking.len() as u64);
    }
}
