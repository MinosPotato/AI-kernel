//! The token a client presents, and the account check that happens before it is read.
//!
//! # Two checks, in this order
//!
//! 1. **Peer credentials.** The operating system tells the host which account is on the other
//!    end of the socket, and it cannot be forged by anything the peer sends. A peer that is
//!    not this account is refused before a single byte of its handshake is parsed.
//! 2. **The token.** A random secret this host generated at startup, written mode `0600`
//!    beside the socket. Presenting it proves the peer read a file only this account can read,
//!    and — because it is regenerated every start — that it read *this* host's file rather
//!    than a stale one from a process that has gone.
//!
//! Neither check is redundant. The first is what stops another account entirely, and is the
//! one that matters; it is also the one that quietly stops holding if a socket ever ends up
//! somewhere with looser modes than intended. The second is what stops a *stale* or *confused*
//! client: a process holding an old connection path, a test pointed at the wrong endpoint, a
//! client that found a socket in a shared directory. Against an attacker who already runs as
//! this account the token proves nothing, and it is not claimed to — such an attacker can read
//! the token file, the database and the transcripts directly.
//!
//! # Why the comparison is constant-time
//!
//! Not because a timing attack across a Unix socket by the same account is a realistic threat
//! — it is not — but because a naïve `==` on a secret is the kind of thing that gets copied
//! into a context where it does matter. The cost here is a few dozen byte operations.
//!
//! # Where the randomness comes from
//!
//! `/dev/urandom`, read directly. A short read is a failure rather than a shorter token: a
//! credential that is weaker than advertised is worse than no credential, because everything
//! above it carries on believing it is the advertised strength.

use std::io::Read as _;
use std::path::Path;

use aik_core::{Error, Result};

/// How many random bytes a token carries.
///
/// 256 bits, hex-encoded to 64 characters. Far past anything guessable, and small enough that
/// the file is a single line somebody can read out.
pub const TOKEN_BYTES: usize = 32;

/// The mode the token file is written with.
pub const TOKEN_FILE_MODE: u32 = 0o600;

/// The source of randomness, read directly rather than through a dependency.
const RANDOM_SOURCE: &str = "/dev/urandom";

/// This process's real user id.
pub fn current_uid() -> u32 {
    // Cannot fail and has no side effects; `getuid` is one of the few calls POSIX guarantees
    // always succeeds.
    unsafe { libc::getuid() }
}

/// A host's per-instance shared secret.
///
/// Deliberately not `Clone`-shy or zeroing: it is written to a file the same account can read
/// and lives for as long as the host does, so scrubbing it from memory would be a gesture
/// rather than a defence. What it does refuse to do is appear in a `Debug` rendering, because
/// that is how a secret reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Draws a fresh token from the system's random source.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        let mut source = std::fs::File::open(RANDOM_SOURCE)
            .map_err(|error| Error::wrap(format!("opening {RANDOM_SOURCE}"), error))?;
        source.read_exact(&mut bytes).map_err(|error| {
            Error::wrap(format!("reading {TOKEN_BYTES} bytes of randomness"), error)
        })?;

        let mut hex = String::with_capacity(TOKEN_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self(hex))
    }

    /// Reads a token that was written by [`Token::write_to`].
    ///
    /// The file's ownership and mode are verified first: a token file another account could
    /// have written is a token another account chose, and using it would mean connecting to
    /// whatever they are listening on.
    pub fn read_from(path: &Path) -> Result<Self> {
        crate::endpoint::verify_private(path, "the token file")?;
        let raw = std::fs::read_to_string(path)
            .map_err(|error| Error::wrap(format!("reading {}", path.display()), error))?;
        Self::parse(raw.trim())
    }

    /// Accepts a token of exactly the right shape, and nothing else.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() != TOKEN_BYTES * 2 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::InvalidArgument(format!(
                "a token is {} hexadecimal characters",
                TOKEN_BYTES * 2
            )));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The token, as it goes on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is this token, compared in constant time.
    ///
    /// A wrong length is refused immediately, which leaks the length of the expected token —
    /// a constant of this build, printed above. What must not leak is *where* two tokens of
    /// the right length first differ, and that is what the loop is for.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        let mut difference = 0u8;
        for (left, right) in expected.iter().zip(presented) {
            difference |= left ^ right;
        }
        difference == 0
    }

    /// Writes the token beside the socket, replacing any previous one atomically.
    ///
    /// Created with `O_EXCL` under a temporary name and renamed into place, so there is no
    /// moment at which the file exists with the wrong mode and no moment at which a reader
    /// sees a half-written token. The temporary name is this process's pid, which is unique
    /// among the processes that could be writing here — the directory is mode `0700` and
    /// belongs to this account.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let directory = path.parent().unwrap_or(Path::new("."));
        let temporary = directory.join(format!(
            ".{}.{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("token"),
            std::process::id(),
        ));

        // A leftover from a previous crash would make `create_new` fail; removing it first is
        // safe because the directory is private to this account and the name carries this
        // process's own pid.
        let _ = std::fs::remove_file(&temporary);

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(TOKEN_FILE_MODE)
            .open(&temporary)
            .map_err(|error| Error::wrap(format!("creating {}", temporary.display()), error))?;
        file.write_all(self.0.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| Error::wrap(format!("writing {}", temporary.display()), error))?;
        drop(file);

        std::fs::rename(&temporary, path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            Error::wrap(format!("installing {}", path.display()), error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_generated_token_is_the_advertised_length_and_not_the_same_twice() {
        let one = Token::generate().expect("generated");
        let two = Token::generate().expect("generated");

        assert_eq!(one.as_str().len(), TOKEN_BYTES * 2);
        assert!(one.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(one, two, "a per-instance credential must not repeat");
    }

    #[test]
    fn a_token_never_prints_itself_but_still_says_what_it_is() {
        let token = Token::generate().expect("generated");
        let rendered = format!("{token:?}");

        assert!(!rendered.contains(token.as_str()), "{rendered}");
        // A rendering that dropped the secret by rendering nothing at all would satisfy the
        // line above and make every log line holding a token unreadable, so the shape is
        // asserted too.
        assert!(rendered.contains("Token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn only_the_exact_token_matches() {
        let token = Token::generate().expect("generated");
        assert!(token.matches(token.as_str()));

        assert!(!token.matches(""));
        assert!(!token.matches(&token.as_str()[..TOKEN_BYTES]));
        assert!(!token.matches(&format!("{}0", token.as_str())));

        let mut wrong = token.as_str().to_owned();
        // Flip the last character: everything before it matches, which is the case a
        // short-circuiting comparison would answer fastest.
        let last = wrong.pop().expect("a character");
        wrong.push(if last == 'a' { 'b' } else { 'a' });
        assert!(!token.matches(&wrong));
    }

    #[test]
    fn anything_that_is_not_a_token_is_refused() {
        for raw in [
            "",
            "not-hex",
            &"g".repeat(TOKEN_BYTES * 2),
            &"a".repeat(TOKEN_BYTES * 2 - 1),
            &"a".repeat(TOKEN_BYTES * 2 + 1),
        ] {
            assert!(Token::parse(raw).is_err(), "{raw:?} is not a token");
        }
        assert!(Token::parse(&"a".repeat(TOKEN_BYTES * 2)).is_ok());
    }

    #[test]
    fn a_written_token_is_private_and_reads_back() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("tightened");
        let path = directory.path().join("aikd.token");

        let token = Token::generate().expect("generated");
        token.write_to(&path).expect("written");

        let mode = std::fs::metadata(&path)
            .expect("the file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, TOKEN_FILE_MODE);
        assert_eq!(Token::read_from(&path).expect("read"), token);
    }

    #[test]
    fn writing_replaces_a_previous_token_without_loosening_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("tightened");
        let path = directory.path().join("aikd.token");

        Token::generate()
            .expect("generated")
            .write_to(&path)
            .expect("written");
        let replacement = Token::generate().expect("generated");
        replacement.write_to(&path).expect("replaced");

        assert_eq!(Token::read_from(&path).expect("read"), replacement);
        let mode = std::fs::metadata(&path)
            .expect("the file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, TOKEN_FILE_MODE);
        assert!(
            std::fs::read_dir(directory.path())
                .expect("listed")
                .filter_map(std::result::Result::ok)
                .all(|entry| entry.file_name() == "aikd.token"),
            "the temporary file must not be left behind",
        );
    }

    #[test]
    fn a_token_file_anyone_can_read_is_refused() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("tightened");
        let path = directory.path().join("aikd.token");

        Token::generate()
            .expect("generated")
            .write_to(&path)
            .expect("written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosened");

        let error = Token::read_from(&path).expect_err("a readable credential is not a credential");
        assert_eq!(error.kind(), aik_core::ErrorKind::Permission);
    }
}
