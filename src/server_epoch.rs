//! The server-incarnation token.
//!
//! A consumer that wants to know whether an agent session is genuinely LIVE
//! cannot learn it from session metadata alone: every field describing a
//! session — its id, kind, source, terminal — is exactly what a stale saved
//! snapshot also contains. Replaying such a snapshot after a restart makes a
//! dead session indistinguishable from a live one.
//!
//! `server_epoch` closes that gap with one property: it is minted in memory,
//! once per server process, and never written anywhere. A snapshot therefore
//! cannot carry a value matching the CURRENT process, so a consumer that
//! observes a token differing from the one it recorded knows the server has
//! restarted or handed off, and a consumer holding only a snapshot cannot
//! manufacture one at all.
//!
//! It is deliberately NOT derived from anything stable (hostname, socket
//! path, pid, wall clock): a token recomputable from durable or guessable
//! inputs can be forged from them, which defeats the point. There is no
//! fallback — if the OS cannot supply entropy the server refuses to start,
//! because a predictable epoch is worse than an absent one.

use std::sync::OnceLock;

static EPOCH: OnceLock<String> = OnceLock::new();

/// 128 bits. Wide enough that two incarnations colliding is not a case anyone
/// has to reason about, small enough to be a cheap equality check.
const EPOCH_BYTES: usize = 16;

#[derive(Debug)]
pub struct EntropyUnavailable(getrandom::Error);

impl std::fmt::Display for EntropyUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot mint the server incarnation token: OS entropy unavailable ({})",
            self.0
        )
    }
}

impl std::error::Error for EntropyUnavailable {}

/// The entropy source, as an injection point.
///
/// Production always passes `getrandom::fill`. The refusal this module is
/// built around - no entropy, no server - is otherwise unreachable in a test:
/// a working OS entropy source cannot be made to fail without intercepting
/// the syscall, so without this seam the branch that decides whether a
/// serverless refusal or a fabricated token wins would ship unexercised.
type Fill = fn(&mut [u8]) -> Result<(), getrandom::Error>;

/// Draw one token. Separated from `init` so the unguessability property has
/// something to test: with the value hidden behind a `OnceLock`, a test calling
/// `init` twice can only observe that the cell did not change - which is a
/// property of `OnceLock`, not of this module, and would still hold if the
/// token were derived from the pid and the wall clock.
fn mint_with(fill: Fill) -> Result<String, EntropyUnavailable> {
    let mut bytes = [0u8; EPOCH_BYTES];
    fill(&mut bytes).map_err(EntropyUnavailable)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(EPOCH_BYTES * 2), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        }))
}

/// Mint the token for this process. Call ONCE at server startup, before any
/// API response can be served.
///
/// Deliberately eager rather than lazy. Minting on first `agent get` would
/// move a fallible OS call into response construction, where there is no
/// honest way to fail — the caller would get either a made-up token or a
/// panic, and a made-up token is the exact thing this exists to prevent.
/// Failing here means the server does not start, which is the correct
/// outcome: an incarnation token that cannot be trusted is worse than a
/// server that says why it stopped.
pub fn init() -> Result<(), EntropyUnavailable> {
    init_with(getrandom::fill)
}

fn init_with(fill: Fill) -> Result<(), EntropyUnavailable> {
    init_into(&EPOCH, fill)
}

/// The publish rule, with its destination cell as an injection point.
///
/// Production always passes `EPOCH`. The cell is a parameter for the same
/// reason `Fill` is one: whether a refusal leaves the destination EMPTY is not
/// observable on a process-wide cell unless the observer owns the whole
/// process, so stated against `EPOCH` that property answers to test ordering
/// instead of to this function.
fn init_into(cell: &OnceLock<String>, fill: Fill) -> Result<(), EntropyUnavailable> {
    // Already minted: return success without touching the RNG. Drawing again
    // would let a second call FAIL over a token that is present and perfectly
    // valid - reporting an error about a state that is fine.
    if cell.get().is_some() {
        return Ok(());
    }
    let hex = mint_with(fill)?;
    // A second init is a programming error, not a reason to re-mint: the
    // token must be stable for the life of the process.
    let _ = cell.set(hex);
    Ok(())
}

/// An entropy source that is unavailable, so the refusal path has a way in.
#[cfg(test)]
fn entropy_unavailable(_dest: &mut [u8]) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

/// Run `init` against an unavailable entropy source. Lets the process-level
/// half of the refusal - what `main` does with the error - be exercised from
/// the crate's own tests instead of being taken on trust.
#[cfg(test)]
pub(crate) fn init_without_entropy_for_test() -> Result<(), EntropyUnavailable> {
    init_with(entropy_unavailable)
}

/// The token for this process, or `None` before `init` has run.
///
/// `None` means UNAVAILABLE and must never be rendered as a token. Callers
/// serialise it as an absent field rather than an empty string, so a consumer
/// cannot mistake "no epoch" for "the empty epoch".
pub fn server_epoch() -> Option<&'static str> {
    EPOCH.get().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_read_is_stable_within_one_process() {
        init().expect("entropy must be available in test");
        let first = server_epoch().expect("initialised");
        let second = server_epoch().expect("initialised");
        assert_eq!(first, second, "the token must not change within a process");
    }

    #[test]
    fn the_token_is_32_lowercase_hex_characters() {
        init().expect("entropy must be available in test");
        let e = server_epoch().expect("initialised");
        assert_eq!(e.len(), EPOCH_BYTES * 2, "128 bits, hex-encoded");
        assert!(
            e.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "lowercase hex only: {e}"
        );
    }

    #[test]
    fn a_second_init_does_not_re_mint() {
        init().expect("entropy");
        let first = server_epoch().expect("initialised").to_string();
        init().expect("entropy");
        assert_eq!(
            server_epoch().expect("initialised"),
            first,
            "re-initialising must not change the token"
        );
    }

    /// The token must be UNGUESSABLE, so two mints must differ - the property
    /// the previous pid+wall-clock derivation did not have.
    ///
    /// This calls THIS MODULE's mint, not `getrandom` directly. Calling the
    /// dependency would pass verbatim even if minting were reverted to deriving
    /// the token from the pid and the clock, which is exactly the regression
    /// the test claims to pin.
    #[test]
    fn independent_mints_differ() {
        let a = mint_with(getrandom::fill).expect("entropy");
        let b = mint_with(getrandom::fill).expect("entropy");
        assert_ne!(a, b, "two mints must differ");
        assert_eq!(a.len(), EPOCH_BYTES * 2);
        assert_eq!(b.len(), EPOCH_BYTES * 2);
    }

    /// Entropy is gone: minting must REFUSE, not return something.
    ///
    /// The whole design rests on this branch - a token that can be produced
    /// without entropy is guessable, and a guessable epoch is worse than an
    /// absent one - so it is asserted rather than assumed.
    #[test]
    fn without_entropy_minting_refuses_instead_of_producing_a_token() {
        let err = mint_with(entropy_unavailable).expect_err("no entropy, no token");
        let message = err.to_string();
        assert!(
            message.contains("cannot mint the server incarnation token"),
            "the refusal must name what could not be done: {message}"
        );
        assert!(
            message.contains("OS entropy unavailable"),
            "the refusal must name the cause: {message}"
        );
    }

    /// A refused mint must leave the cell EMPTY, so the reader keeps reporting
    /// unavailable and no response can carry a fabricated token.
    ///
    /// Asserted on a private cell rather than `EPOCH`: the precondition -
    /// nothing has minted yet - is constructed here instead of assumed of the
    /// process, so the verdict is a fact about `init_into` rather than about
    /// which sibling test happened to run first.
    #[test]
    fn a_refused_mint_publishes_no_token() {
        let cell = OnceLock::new();
        init_into(&cell, entropy_unavailable).expect_err("init must propagate the refusal");
        assert!(
            cell.get().is_none(),
            "a refused mint must publish nothing, not an empty or partial token"
        );
    }

    /// A cell that already holds a token must survive a later refusal: the
    /// second call must not fail over a state that is fine, and must not
    /// replace the value that is already published.
    #[test]
    fn an_already_minted_cell_is_untouched_by_a_later_refusal() {
        let cell = OnceLock::new();
        init_into(&cell, getrandom::fill).expect("entropy");
        let minted = cell.get().expect("initialised").to_string();
        init_into(&cell, entropy_unavailable).expect("an already-minted cell must not fail");
        assert_eq!(
            cell.get().map(String::as_str),
            Some(minted.as_str()),
            "a refusal after a successful mint must not clear or replace the token"
        );
    }
}
