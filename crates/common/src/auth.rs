//! The shared secret that authenticates a push (ADR-0019).
//!
//! `listen` hands whatever it receives to `git receive-pack`, and
//! `update-worktree` then checks that stream out authoritatively over a worktree
//! whose files the receiving machine's tooling *executes*. Binding loopback
//! (ADR-0006) keeps that off the network, but not away from another local user,
//! another SSH session, or a port forward someone else set up — so reaching the
//! port must not be the same thing as being allowed to push (issue #81).
//!
//! ## The exchange
//!
//! `receive-pack` is server-speaks-first, so the client authenticates in a
//! **preamble** written the moment the connection is up, before the server has
//! said anything:
//!
//! ```text
//! client → server   0027git-full-send-auth-v1 <token>\n     (one pkt-line)
//! server            verifies, then spawns receive-pack
//! server → client   ref advertisement · …                    (unchanged)
//! ```
//!
//! Everything after the preamble is the identical raw receive-pack stream:
//! ADR-0005 and ADR-0010 are untouched, and the preamble costs no round trip the
//! connect had not already paid. A server that refuses answers with an [`err_pkt`]
//! and closes; `git push` recognises `ERR` during initial contact and reports it
//! as a remote error, so a misconfigured client is told what happened instead of
//! seeing the connection drop.
//!
//! Bearer-token-over-loopback is the whole of the threat model (issue #81): the
//! SSH tunnel already provides confidentiality, and the goal is only to stop an
//! unrelated local process from pushing code the receiving machine will run.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Environment variable holding the shared secret **inline**, consulted by every
/// command when no `--token-file` is given.
///
/// The flag is the better habit (a file keeps the secret out of `ps` and out of
/// inherited environments); this exists so a caller that already holds the token
/// in memory does not have to materialise a temp file for it.
pub const TOKEN_ENV: &str = "GIT_FULL_SEND_TOKEN";

/// The magic that opens the authentication pkt-line, versioned so a later scheme
/// can be told apart from this one rather than merely failing to parse.
pub const AUTH_PKT_PREFIX: &str = "git-full-send-auth-v1 ";

/// Longest secret we will carry. Far below the pkt-line payload limit (65516) —
/// the bound exists to reject a file that is obviously not a token (someone's
/// private key, a whole config) before it reaches the wire.
pub const MAX_TOKEN_LEN: usize = 4096;

/// Below this many characters a secret is warned about on load. Not a hard floor:
/// a weak secret is the operator's call to make, an unusable one is not.
const WEAK_TOKEN_LEN: usize = 16;

/// The reason a shared secret could not be established.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The token file could not be read.
    #[error("could not read the token file `{path}`")]
    ReadTokenFile {
        /// The file we tried to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The token was empty (an empty file, or an empty environment variable).
    #[error("the shared secret from {source_desc} is empty")]
    Empty {
        /// Where the empty value came from, for a message that points at it.
        source_desc: String,
    },
    /// The token held a character that cannot travel in a single pkt-line.
    #[error(
        "the shared secret from {source_desc} contains whitespace or control characters; \
         it must be a single line of printable characters"
    )]
    NotPrintable {
        /// Where the offending value came from.
        source_desc: String,
    },
    /// The token was longer than [`MAX_TOKEN_LEN`].
    #[error(
        "the shared secret from {source_desc} is {len} bytes; the maximum is {MAX_TOKEN_LEN} \
         (is this really a token file?)"
    )]
    TooLong {
        /// Where the offending value came from.
        source_desc: String,
        /// Its length in bytes.
        len: usize,
    },
}

/// A validated shared secret.
///
/// Deliberately awkward to leak: [`Debug`] is redacted, there is no `Display` and
/// no `PartialEq`, and the only way to compare one against a presented value is
/// [`Token::matches`], which does not stop early on the first differing byte.
#[derive(Clone)]
pub struct Token(String);

/// Redacted, so a `?config`/`{err:?}` anywhere — ours or a dependency's — cannot
/// print the secret.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Validate `value` as a shared secret. `source_desc` names where it came
    /// from (`the token file \`…\``, `$GIT_FULL_SEND_TOKEN`) so a rejection points
    /// at the thing the operator has to fix.
    ///
    /// Surrounding whitespace is trimmed first: a token file written by `echo`
    /// or an editor ends with a newline, and failing on that would be a rite of
    /// passage rather than a security property.
    pub fn new(value: &str, source_desc: impl Into<String>) -> Result<Self, AuthError> {
        let source_desc = source_desc.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(AuthError::Empty { source_desc });
        }
        if value.len() > MAX_TOKEN_LEN {
            return Err(AuthError::TooLong {
                source_desc,
                len: value.len(),
            });
        }
        // The secret travels as one pkt-line, so it must not contain the framing's
        // own terminator — or anything else that would make the line ambiguous.
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(AuthError::NotPrintable { source_desc });
        }
        if value.chars().count() < WEAK_TOKEN_LEN {
            tracing::warn!(
                source = %source_desc,
                "the shared secret is short; prefer at least {WEAK_TOKEN_LEN} random \
                 characters (e.g. `head -c 32 /dev/urandom | base64`)",
            );
        }
        Ok(Self(value.to_string()))
    }

    /// Read a shared secret from `path`.
    ///
    /// The file's permissions are checked as a courtesy: a token any local user
    /// can read defeats the point of having one, but it is a warning rather than
    /// a refusal — the operator may have a reason, and refusing to start over a
    /// mode bit is its own kind of hazard.
    pub fn from_file(path: &Path) -> Result<Self, AuthError> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| AuthError::ReadTokenFile {
                path: path.to_path_buf(),
                source,
            })?;
        warn_if_world_readable(path);
        Self::new(&contents, format!("the token file `{}`", path.display()))
    }

    /// Resolve the shared secret for a command: `--token-file` if given, else
    /// [`TOKEN_ENV`] if set and non-empty, else `None`.
    ///
    /// The one place the lookup order lives, so client and server cannot drift
    /// apart on it.
    pub fn resolve(token_file: Option<&Path>) -> Result<Option<Self>, AuthError> {
        if let Some(path) = token_file {
            return Self::from_file(path).map(Some);
        }
        match std::env::var(TOKEN_ENV) {
            Ok(value) if !value.trim().is_empty() => {
                Self::new(&value, format!("${TOKEN_ENV}")).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Whether `presented` is this secret.
    ///
    /// Compares in time independent of *where* the two differ, so a caller cannot
    /// learn the secret a byte at a time by measuring rejections. Length is
    /// compared separately and is not treated as secret: these are high-entropy
    /// tokens, and the length of the presented value is the attacker's own choice
    /// anyway.
    pub fn matches(&self, presented: &[u8]) -> bool {
        let expected = self.0.as_bytes();
        let mut diff = u8::from(expected.len() != presented.len());
        for (a, b) in expected.iter().zip(presented.iter()) {
            diff |= a ^ b;
        }
        std::hint::black_box(diff) == 0
    }

    /// The secret as it travels on the wire. Private on purpose: the only public
    /// ways out of a [`Token`] are [`matches`](Self::matches) and [`auth_pkt`].
    fn expose(&self) -> &str {
        &self.0
    }
}

/// Warn when a token file is readable by anyone but its owner. A no-op off Unix,
/// where the mode bits mean nothing.
#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

/// Warn when a token file is readable by anyone but its owner.
#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:04o}"),
            "the token file is readable beyond its owner; `chmod 600` it — a shared \
             secret every local user can read authenticates nothing",
        );
    }
}

/// The authentication preamble for `token`, framed as one pkt-line.
pub fn auth_pkt(token: &Token) -> Vec<u8> {
    pkt_line(&format!("{AUTH_PKT_PREFIX}{}\n", token.expose()))
}

/// An `ERR` pkt-line carrying `message`.
///
/// `git` recognises this during initial contact and dies with
/// `remote error: <message>`, which is how a rejected client learns *why* the
/// connection ended instead of seeing it drop.
pub fn err_pkt(message: &str) -> Vec<u8> {
    pkt_line(&format!("ERR {message}\n"))
}

/// Frame `payload` as a pkt-line (4 hex length bytes, length-inclusive).
fn pkt_line(payload: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    out
}

/// What the server made of a client's preamble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthOutcome {
    /// The presented secret matched.
    Ok,
    /// A well-formed preamble carrying the wrong secret.
    Mismatch,
    /// Something arrived, but it was not an authentication preamble — a client
    /// built against a different scheme, or something that is not us at all.
    Malformed,
    /// Nothing arrived before the deadline: almost always a client that has no
    /// token configured, waiting for the ref advertisement that will never come.
    Absent,
}

impl AuthOutcome {
    /// The message sent back to a refused client (and logged), or `None` when
    /// there is nothing to refuse.
    ///
    /// Deliberately uniform across the three failure modes: a peer that cannot
    /// authenticate learns that it cannot, not which part it got wrong.
    pub fn refusal(self) -> Option<&'static str> {
        match self {
            AuthOutcome::Ok => None,
            _ => Some("git-full-send: authentication required (see `listen --token-file`)"),
        }
    }

    /// The stable string for the log line and the metrics record.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthOutcome::Ok => "ok",
            AuthOutcome::Mismatch => "mismatch",
            AuthOutcome::Malformed => "malformed",
            AuthOutcome::Absent => "absent",
        }
    }
}

/// Read one authentication preamble from `reader` and check it against
/// `expected`.
///
/// Reads *exactly* the pkt-line and no further, so the bytes that follow are
/// untouched and can be handed to `git receive-pack` as the raw stream it
/// expects. The caller is responsible for bounding the wait (a read timeout on
/// the socket): a client with no token configured sends nothing at all, and
/// [`AuthOutcome::Absent`] is how that arrives here.
pub fn read_auth_pkt(reader: &mut impl std::io::Read, expected: &Token) -> AuthOutcome {
    let mut header = [0u8; 4];
    if reader.read_exact(&mut header).is_err() {
        return AuthOutcome::Absent;
    }
    let Some(len) = std::str::from_utf8(&header)
        .ok()
        .and_then(|text| usize::from_str_radix(text, 16).ok())
    else {
        return AuthOutcome::Malformed;
    };
    // A flush-pkt or a line too long to be ours: not a preamble, and not worth
    // allocating for.
    if !(5..=AUTH_PKT_PREFIX.len() + MAX_TOKEN_LEN + 6).contains(&len) {
        return AuthOutcome::Malformed;
    }
    let mut payload = vec![0u8; len - 4];
    if reader.read_exact(&mut payload).is_err() {
        return AuthOutcome::Malformed;
    }
    let Some(presented) = payload.strip_prefix(AUTH_PKT_PREFIX.as_bytes()) else {
        return AuthOutcome::Malformed;
    };
    let presented = presented.strip_suffix(b"\n").unwrap_or(presented);
    if expected.matches(presented) {
        AuthOutcome::Ok
    } else {
        AuthOutcome::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: &str) -> Token {
        Token::new(value, "the test").expect("valid token")
    }

    #[test]
    fn a_token_is_trimmed_and_validated() {
        // A file written by `echo` ends in a newline; that must not be a failure.
        assert!(Token::new("  s3cret-token-value  \n", "the test").is_ok());
        assert!(matches!(
            Token::new("   \n", "the test"),
            Err(AuthError::Empty { .. }),
        ));
        assert!(matches!(
            Token::new("two words", "the test"),
            Err(AuthError::NotPrintable { .. }),
        ));
        assert!(matches!(
            Token::new("has\ttab", "the test"),
            Err(AuthError::NotPrintable { .. }),
        ));
        assert!(matches!(
            Token::new(&"x".repeat(MAX_TOKEN_LEN + 1), "the test"),
            Err(AuthError::TooLong { .. }),
        ));
    }

    /// The secret must not be printable by accident — not by us, and not by a
    /// dependency that formats a struct we put it in.
    #[test]
    fn debug_never_prints_the_secret() {
        let token = token("hunter2-hunter2-hunter2");
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
        assert!(!format!("{:?}", Some(token)).contains("hunter2"));
    }

    #[test]
    fn matches_accepts_only_the_exact_secret() {
        let token = token("correct-horse-battery-staple");
        assert!(token.matches(b"correct-horse-battery-staple"));
        assert!(!token.matches(b"correct-horse-battery-stapl")); // shorter
        assert!(!token.matches(b"correct-horse-battery-staple!")); // longer
        assert!(!token.matches(b"correct-horse-battery-stapld")); // last byte
        assert!(!token.matches(b"xorrect-horse-battery-staple")); // first byte
        assert!(!token.matches(b""));
    }

    #[test]
    fn a_preamble_round_trips() {
        let token = token("s3cret-token-value-here");
        let mut wire = auth_pkt(&token);
        // Whatever follows the preamble must be left for `receive-pack`.
        wire.extend_from_slice(b"0000the-rest-of-the-stream");

        let mut reader = std::io::Cursor::new(wire);
        assert_eq!(read_auth_pkt(&mut reader, &token), AuthOutcome::Ok);

        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut rest).unwrap();
        assert_eq!(rest, b"0000the-rest-of-the-stream");
    }

    #[test]
    fn a_preamble_is_classified_by_how_it_failed() {
        let expected = token("the-real-token-value");

        let wrong = auth_pkt(&token("not-the-real-token"));
        assert_eq!(
            read_auth_pkt(&mut std::io::Cursor::new(wrong), &expected),
            AuthOutcome::Mismatch,
        );
        // A client that says nothing: read_exact hits EOF (or the caller's
        // timeout, which surfaces the same way).
        assert_eq!(
            read_auth_pkt(&mut std::io::Cursor::new(Vec::new()), &expected),
            AuthOutcome::Absent,
        );
        // A flush-pkt — what `probe` sends — is not a preamble.
        assert_eq!(
            read_auth_pkt(&mut std::io::Cursor::new(b"0000".to_vec()), &expected),
            AuthOutcome::Malformed,
        );
        assert_eq!(
            read_auth_pkt(&mut std::io::Cursor::new(b"zzzz".to_vec()), &expected),
            AuthOutcome::Malformed,
        );
        // Well-framed, but some other scheme's line.
        assert_eq!(
            read_auth_pkt(&mut std::io::Cursor::new(pkt_line("hello\n")), &expected),
            AuthOutcome::Malformed,
        );
    }

    /// Every refusal says the same thing: a peer that cannot authenticate does
    /// not get to learn which half it got wrong.
    #[test]
    fn refusals_do_not_distinguish_themselves() {
        assert_eq!(AuthOutcome::Ok.refusal(), None);
        let refusals = [
            AuthOutcome::Mismatch.refusal(),
            AuthOutcome::Malformed.refusal(),
            AuthOutcome::Absent.refusal(),
        ];
        assert!(refusals.iter().all(|r| *r == refusals[0] && r.is_some()));
    }

    #[test]
    fn an_err_pkt_is_framed_as_git_expects() {
        assert_eq!(err_pkt("nope"), b"000dERR nope\n".to_vec());
    }

    /// The environment half of the lookup is left to the CLI's own tests: it is
    /// process-global state, and the workspace's tests share a process.
    #[test]
    fn resolve_reads_the_named_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "from-the-file-token\n").unwrap();

        let resolved = Token::resolve(Some(&path)).unwrap().expect("a token");
        assert!(resolved.matches(b"from-the-file-token"));

        // A missing file is an error, not a silent fall-through to the
        // environment: the operator asked for that file.
        let missing = dir.path().join("nope");
        assert!(matches!(
            Token::resolve(Some(&missing)),
            Err(AuthError::ReadTokenFile { .. }),
        ));
    }
}
