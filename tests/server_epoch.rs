//! Behavioural coverage for the server incarnation token.
//!
//! These live in the repository on purpose. An external harness can show the
//! token behaving correctly on one machine on one day; only a test in the tree
//! keeps it behaving that way, and only a test in the tree fails when someone
//! later persists the token or drops the rotation.
//!
//! Every case runs against a real server in an isolated XDG/socket tree and is
//! torn down afterwards.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Isolated {
    base: PathBuf,
    socket: PathBuf,
    server: Option<Child>,
}

impl Isolated {
    fn start(tag: &str) -> Self {
        let mut me = Self::prepare(tag);
        me.spawn_server();
        me
    }

    /// The isolated tree WITHOUT a running server. The entropy-failure test
    /// needs this: it must control the server's environment (`LD_PRELOAD`)
    /// and observe its exit, neither of which a constructor that already
    /// started the process would allow.
    fn prepare(tag: &str) -> Self {
        // A UNIX socket path must fit in sockaddr_un.sun_path: 104 bytes on
        // macOS, 108 on Linux. `std::env::temp_dir()` is `/var/folders/<2>/
        // <28+>/T/` on macOS, so a descriptive base under it pushes the socket
        // past the limit and the server dies with "path must be shorter than
        // SUN_LEN" - which is what macOS CI reported. Build the root SHORT and
        // assert it, rather than discovering the ceiling on one platform.
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        // Short but collision-safe: pid plus a monotonic counter, so parallel
        // tests in one process cannot collide and reruns cannot inherit a
        // previous tree.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let short: String = tag
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(4)
            .collect();
        let base = root.join(format!("he{}-{}-{}", short, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&base);
        for sub in ["config", "data", "state", "run"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        // Both spellings: the app directory is `herdr` for a release build and
        // `herdr-dev` for a debug one, so writing only the first left this
        // setting unread on the profile the suite actually runs under.
        for app_dir in ["herdr", "herdr-dev"] {
            std::fs::create_dir_all(base.join("config").join(app_dir)).unwrap();
            std::fs::write(
                base.join("config").join(app_dir).join("config.toml"),
                "onboarding = false\n",
            )
            .unwrap();
        }
        let socket = base.join("run/herdr.sock");
        // The assertion is the guard: without it this ceiling is only ever
        // found by a platform that has it, and only in CI.
        assert!(
            socket.as_os_str().len() < 100,
            "socket path {} is {} bytes; sockaddr_un.sun_path is 104 on macOS",
            socket.display(),
            socket.as_os_str().len()
        );

        Self {
            base,
            socket,
            server: None,
        }
    }

    fn spawn_server(&mut self) {
        assert!(
            self.server.is_none(),
            "a server is already owned; stop it before starting another"
        );
        self.server = Some(self.cmd(&["server"]).spawn().unwrap());
        // Wait until the server actually ANSWERS, not merely until the socket
        // file exists: `bind` creates the path before the listener accepts, and
        // after a restart the successor must rebind the path the old server
        // removed, so a file check can return before anything is listening.
        self.wait_until_serving();
    }

    /// Stop the owned server and wait until it has actually exited.
    ///
    /// The same discipline as the persistence test: `stop` must SUCCEED and
    /// the process must be GONE, proven via `/proc` where it exists and via
    /// the socket disappearing elsewhere. A restart layered on a fixed sleep
    /// could race the old process and read the OLD incarnation as the new
    /// one, making rotation look broken — or worse, not look at all.
    fn stop_and_wait(&mut self) {
        let stop = self.cmd(&["server", "stop"]).output().unwrap();
        assert!(
            stop.status.success(),
            "server stop failed: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.server_pids() {
                Some(p) if p.is_empty() => break,
                // Where /proc is unavailable, fall back to the socket
                // disappearing — the same fallback the persistence test uses.
                None if !self.socket.exists() => break,
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "the owned server did not exit after a successful stop"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(mut c) = self.server.take() {
            let _ = c.wait();
        }
        // Clear the stale socket FILE the exited server may have left, so the
        // next `spawn_server` rebinds a clean path before `wait_until_serving`
        // begins probing for an answer.
        let _ = std::fs::remove_file(&self.socket);
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_herdr"));
        c.args(args)
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("XDG_DATA_HOME", self.base.join("data"))
            .env("XDG_STATE_HOME", self.base.join("state"))
            .env("XDG_RUNTIME_DIR", self.base.join("run"))
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Wait until the server ANSWERS, and return the token it answered with.
    ///
    /// Not "until the socket file exists": `bind` creates that path before the
    /// listener is accepting, and the api client connects once without
    /// retrying, so the next command can still be refused. After a handoff the
    /// existence check is weaker still - the successor has to rebind the path
    /// the old server removed, so a file check can return before any server is
    /// listening at all.
    fn wait_until_serving(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(epoch) = self.snapshot_epoch() {
                return epoch;
            }
            assert!(
                Instant::now() < deadline,
                "no server answered on {}",
                self.socket.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The token as the SNAPSHOT reports it.
    fn snapshot_epoch(&self) -> Option<String> {
        let raw = self.run(&["api", "snapshot"]);
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v["result"]["snapshot"]["server_epoch"]
            .as_str()
            .map(str::to_string)
    }

    /// The token as an AGENT record reports it.
    fn agent_epoch(&self, target: &str) -> Option<String> {
        let raw = self.run(&["agent", "get", target]);
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v["result"]["agent"]["server_epoch"]
            .as_str()
            .map(str::to_string)
    }

    fn first_pane(&self) -> String {
        let raw = self.run(&["api", "snapshot"]);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["result"]["snapshot"]["panes"][0]["pane_id"]
            .as_str()
            .expect("a pane exists")
            .to_string()
    }

    /// The label of the first workspace the snapshot reports.
    ///
    /// Read alongside the pane id so a restart can be shown to have RESTORED
    /// the saved session rather than started an empty one: a fresh server also
    /// serves `w1:p1`, so pane identity alone cannot tell the two apart, while
    /// a label nobody typed twice can.
    fn first_workspace_label(&self) -> String {
        let raw = self.run(&["api", "snapshot"]);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["result"]["snapshot"]["workspaces"][0]["label"]
            .as_str()
            .expect("a workspace exists")
            .to_string()
    }

    /// Stop the owned server and start another against the SAME isolated tree,
    /// so the successor restores the session the first one persisted.
    ///
    /// A plain restart, not `live-handoff`: the two are different paths - one
    /// passes the listening socket to a successor process, the other exits and
    /// lets a later process read the session back off disk - and only this one
    /// is what a user does when they reboot or run `herdr server stop`.
    fn restart(&mut self) {
        let stop = self.cmd(&["server", "stop"]).output().unwrap();
        assert!(
            stop.status.success(),
            "server stop failed: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
        if let Some(mut previous) = self.server.take() {
            let _ = previous.wait();
        }
        self.server = Some(self.cmd(&["server"]).spawn().unwrap());
        self.wait_until_serving();
    }

    /// PIDs of servers belonging to THIS socket.
    ///
    /// `/proc/<pid>/environ` is a Linux interface. On other platforms this
    /// returns `None` - not an empty list, which a caller would read as
    /// "no servers" and treat as evidence. A test that needs process proof
    /// must skip where the proof is unavailable rather than assert over it.
    #[cfg(target_os = "linux")]
    fn server_pids(&self) -> Option<Vec<String>> {
        // Only processes whose environment names OUR socket - never a server
        // belonging to the developer running the suite.
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return None;
        };
        for e in entries.flatten() {
            let pid = e.file_name().to_string_lossy().to_string();
            if !pid.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) {
                let want = format!("HERDR_SOCKET_PATH={}", self.socket.display());
                if env.split(|b| *b == 0).any(|v| v == want.as_bytes()) {
                    out.push(pid);
                }
            }
        }
        Some(out)
    }

    #[cfg(not(target_os = "linux"))]
    fn server_pids(&self) -> Option<Vec<String>> {
        None
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop"]).output();
        if let Some(mut c) = self.server.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        for pid in self.server_pids().unwrap_or_default() {
            let _ = Command::new("kill").args(["-9", &pid]).output();
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn is_lower_hex_32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Within one process the token must be ONE value, wherever it is read from.
/// If the snapshot and an agent record could disagree, a consumer comparing
/// them would see a handoff that never happened.
#[test]
fn the_token_is_identical_on_the_snapshot_and_on_agent_info() {
    let iso = Isolated::start("same-process");
    iso.run(&["workspace", "create", "--label", "epoch", "--focus"]);
    let pane = iso.first_pane();
    iso.run(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "epoch-test",
        "--agent",
        "claude",
        "--state",
        "idle",
    ]);

    let snapshot = iso.snapshot_epoch().expect("snapshot carries the token");
    let agent = iso
        .agent_epoch(&pane)
        .expect("agent info carries the token");
    assert!(
        is_lower_hex_32(&snapshot),
        "not 128 bits of hex: {snapshot}"
    );
    assert_eq!(
        snapshot, agent,
        "one incarnation must present one token on every surface"
    );

    // ...and it must not drift between reads within that process.
    assert_eq!(Some(snapshot), iso.snapshot_epoch());
}

/// The whole point: a live handoff replaces the process, so the token must
/// change. A consumer holding the old value can then tell.
///
/// Unix only. `server live-handoff` passes the listening socket to the
/// successor across a UNIX domain socket; there is no equivalent path on
/// Windows, so the test is not "expected to fail" there - it is not applicable,
/// and `cfg` says so rather than a runtime skip that still has to compile
/// against an API that does not exist.
#[cfg(unix)]
#[test]
fn a_successful_handoff_rotates_the_token() {
    let iso = Isolated::start("rotate");
    let before = iso.snapshot_epoch().expect("token before handoff");
    let pids_before = iso.server_pids();

    let out = iso
        .cmd(&[
            "server",
            "live-handoff",
            "--import-exe",
            env!("CARGO_BIN_EXE_herdr"),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "handoff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The old server removes the public socket before it answers this command,
    // so the successor has to rebind it. Waiting for an ANSWER waits for that;
    // a fixed sleep would only have made the window likely rather than closed.
    let after = iso.wait_until_serving();
    assert_ne!(before, after, "a handoff must rotate the token");
    assert!(is_lower_hex_32(&after));
    // Process proof only where /proc exists. Elsewhere the rotation above is
    // still asserted; what is skipped is the corroboration, and it is skipped
    // explicitly rather than by an empty list quietly comparing equal.
    match (pids_before, iso.server_pids()) {
        (Some(before), Some(after)) => assert_ne!(
            before, after,
            "the token rotated, so the process must actually have been replaced"
        ),
        _ => eprintln!("process-replacement proof skipped: /proc is Linux-only"),
    }
}

/// A handoff that FAILS must leave the incarnation alone. Rotating on a failed
/// handoff would tell every client its cache was stale when nothing moved.
#[cfg(unix)]
#[test]
fn a_failed_handoff_retains_the_token() {
    let iso = Isolated::start("retain");
    let before = iso.snapshot_epoch().expect("token before");

    let missing = iso.base.join("no-such-binary");
    let out = iso
        .cmd(&[
            "server",
            "live-handoff",
            "--import-exe",
            missing.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a handoff importing a nonexistent executable must fail"
    );

    let after = iso
        .snapshot_epoch()
        .expect("token after the failed handoff");
    assert_eq!(
        before, after,
        "a failed handoff must not rotate the token: nothing was replaced"
    );
}

/// The case the field exists for, end to end: a restart REPLAYS the saved
/// session, so everything describing it comes back identical and a client
/// comparing that metadata cannot tell the restored session from the one it
/// recorded. The incarnation token is the one thing that differs.
///
/// Distinct from the handoff case above, which exercises the socket-passing
/// path. This one exercises the ordinary path - the server exits, a later
/// process reads the session back off disk - and would keep passing if
/// rotation were ever made a property of handoff alone.
#[test]
fn a_restart_replays_the_saved_session_but_not_the_incarnation() {
    let mut iso = Isolated::start("replay");
    iso.run(&["workspace", "create", "--label", "replayed", "--focus"]);
    let pane_before = iso.first_pane();
    let label_before = iso.first_workspace_label();
    let before = iso.snapshot_epoch().expect("token before the restart");
    assert_eq!(label_before, "replayed", "the label under test must be set");

    iso.restart();

    // The session really was restored, so the comparison below is between two
    // servers serving the SAME session rather than between a session and an
    // empty one.
    assert_eq!(
        (iso.first_workspace_label(), iso.first_pane()),
        (label_before, pane_before),
        "the restart must replay the saved session, or this proves nothing"
    );

    let after = iso.snapshot_epoch().expect("token after the restart");
    assert!(is_lower_hex_32(&after), "not 128 bits of hex: {after}");
    assert_ne!(
        before, after,
        "a restart is a new incarnation: a client holding the old token must be able to tell, \
         even though every other field it could compare is identical"
    );
}

/// A RESTART must rotate the token across real process boundaries: two
/// sequential server processes over the same state directory mint different
/// tokens. The unit test `independent_mints_differ` proves two draws differ
/// inside ONE process — a mint derived from anything stable across restarts
/// (state-dir contents, hostname, socket path) would pass it while restarts
/// kept presenting the same token, which is exactly the staleness a consumer
/// could then never detect. Only a second process can pin this.
#[test]
fn a_new_server_process_over_the_same_state_dir_mints_a_different_token() {
    let mut iso = Isolated::start("xproc");
    let first = iso.snapshot_epoch().expect("token from the first process");
    assert!(is_lower_hex_32(&first), "not 128 bits of hex: {first}");
    let pids_first = iso.server_pids();

    iso.stop_and_wait();
    iso.spawn_server();

    let second = iso.snapshot_epoch().expect("token from the second process");
    assert!(is_lower_hex_32(&second), "not 128 bits of hex: {second}");
    assert_ne!(
        first, second,
        "a fresh server process over the same state dir must mint a fresh token"
    );
    // Process proof only where /proc exists — the same corroboration split as
    // the handoff test: rotation is asserted everywhere, replacement is
    // proven where it can be and skipped out loud where it cannot.
    match (pids_first, iso.server_pids()) {
        (Some(before), Some(after)) => assert_ne!(
            before, after,
            "the token rotated, so a different process must be serving it"
        ),
        _ => eprintln!("process-replacement proof skipped: /proc is Linux-only"),
    }
}

/// N CONCURRENT `agent get` reads must agree on ONE token. One incarnation is
/// one value; if parallel readers could disagree — a per-response mint, a
/// per-connection value — then two consumers comparing notes would see a
/// handoff that never happened. Twelve parallel readers, matching the
/// r18-server-epoch drill; every reader must produce a value, because a
/// missing read compared against nothing passes vacuously.
#[test]
fn concurrent_agent_get_reads_agree_on_one_token() {
    let iso = Isolated::start("conc");
    iso.run(&["workspace", "create", "--label", "conc", "--focus"]);
    let pane = iso.first_pane();
    iso.run(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "epoch-test",
        "--agent",
        "claude",
        "--state",
        "idle",
    ]);
    let reference = iso
        .agent_epoch(&pane)
        .expect("agent info carries the token");
    assert!(
        is_lower_hex_32(&reference),
        "not 128 bits of hex: {reference}"
    );

    // Commands are built up front and MOVED into the reader threads, so every
    // thread runs the same invocation an ordinary client would.
    const READERS: usize = 12;
    let commands: Vec<Command> = (0..READERS)
        .map(|_| iso.cmd(&["agent", "get", &pane]))
        .collect();
    let values: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = commands
            .into_iter()
            .map(|mut c| {
                scope.spawn(move || {
                    let out = c.output().unwrap();
                    let raw = String::from_utf8_lossy(&out.stdout);
                    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
                    v["result"]["agent"]["server_epoch"]
                        .as_str()
                        .map(str::to_string)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for (i, value) in values.iter().enumerate() {
        let value = value
            .as_ref()
            .unwrap_or_else(|| panic!("concurrent read {i} produced no token; refusing a vacuous agreement over an incomplete set"));
        assert_eq!(
            value, &reference,
            "concurrent read {i} disagreed: one incarnation must present one token"
        );
    }
}

/// A FAILED entropy mint must refuse to serve — exit, with the reason, with
/// no socket — never fall back to a stable or fabricated token. A server that
/// starts anyway is presenting an incarnation it cannot vouch for, which is
/// worse than not starting: every consumer's staleness check would then trust
/// a token that proves nothing.
///
/// The starvation is the r18-server-epoch drill's LD_PRELOAD shim,
/// reconstructed (tests/support/fail_getrandom.c — its provenance header
/// records the one divergence: it starves crypto-grade `flags == 0` requests,
/// exactly what the mint's `getrandom::fill` issues, rather than every call,
/// because std's own hash-key draw now precedes the mint and a total
/// starvation kills THAT first, pinning a std panic instead of the refusal).
/// Compiled here at test time. Linux only: LD_PRELOAD symbol interposition is
/// a Linux mechanism, and it is also where the drill ran.
///
/// The drill recorded exit 70 (EX_SOFTWARE) against the pre-commit binary;
/// the committed tree deliberately chose 69 — EX_UNAVAILABLE, "a required
/// service is not available" — in `mint_server_epoch_or_exit` (src/main.rs),
/// blaming the environment rather than the program. This test pins the
/// committed choice.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_entropy_mint_refuses_to_serve_rather_than_falling_back() {
    let iso = Isolated::prepare("noent");

    // Build the shim beside the isolated tree, from the committed source.
    let shim_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fail_getrandom.c");
    let shim = iso.base.join("fail_getrandom.so");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let build = Command::new(&cc)
        .args(["-shared", "-fPIC", "-o"])
        .arg(&shim)
        .arg(&shim_src)
        .arg("-ldl")
        .output()
        .unwrap_or_else(|e| panic!("failed to run C compiler {cc}: {e}"));
    assert!(
        build.status.success(),
        "compiling the entropy shim failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    // The server under starvation. Spawned rather than `.output()`: a server
    // that wrongly starts would never exit, and this test must then FAIL with
    // the reason rather than hang.
    let mut cmd = iso.cmd(&["server"]);
    cmd.env("LD_PRELOAD", &shim);
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if iso.socket.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server started serving despite a failed entropy mint: fallback token");
        }
        assert!(
            Instant::now() < deadline,
            "the server neither exited nor served under entropy starvation"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        status.code(),
        Some(69),
        "a failed mint must exit 69 (EX_UNAVAILABLE), got {status:?}; stderr: {stderr}"
    );
    assert!(
        stderr.contains("cannot mint the server incarnation token"),
        "the refusal must say why; stderr: {stderr}"
    );
    assert!(
        !iso.socket.exists(),
        "a server that refused to start must not have left a listening socket"
    );

    // The positive/negative pair from the drill: a CLIENT path under the same
    // shim still works. This is what proves the server's refusal above was
    // the shim firing on the mint, not the shim breaking the binary wholesale
    // — without it, exit 69 could be any crash wearing the right code.
    let mut help = iso.cmd(&["--help"]);
    help.env("LD_PRELOAD", &shim);
    let help_out = help.output().unwrap();
    assert!(
        help_out.status.success(),
        "--help must not need the entropy mint; stderr: {}",
        String::from_utf8_lossy(&help_out.stderr)
    );

    // `prepare` never started a server, and the refusal must not have either.
    assert!(
        iso.server_pids().expect("/proc exists on Linux").is_empty(),
        "no server process may survive a refused mint"
    );
}

/// The token must never reach disk. If it could be restored, a replayed
/// snapshot would be indistinguishable from a live server — the exact forgery
/// this field exists to make detectable.
#[test]
fn the_token_is_absent_from_everything_written_to_disk() {
    let iso = Isolated::start("nopers");
    iso.run(&["workspace", "create", "--label", "persist", "--focus"]);
    iso.run(&["tab", "create", "--label", "t", "--focus"]);
    let token = iso.snapshot_epoch().expect("token");

    // Stop must SUCCEED and the owned server must actually exit. A fixed sleep
    // plus "some file exists" can pass on config.toml alone, proving nothing
    // about what the server persisted.
    let stop = iso.cmd(&["server", "stop"]).output().unwrap();
    assert!(
        stop.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match iso.server_pids() {
            Some(p) if p.is_empty() => break,
            // Where /proc is unavailable, fall back to the socket disappearing.
            None if !iso.socket.exists() => break,
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "the owned server did not exit after a successful stop"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The session file the server is expected to write. Asserting it EXISTS is
    // what makes the search below meaningful: without it, "no token found"
    // could simply mean nothing was saved.
    //
    // Located by NAME rather than a fixed path: the config directory is
    // `herdr` for a release build and `herdr-dev` for a debug one, and a
    // hardcoded path silently missed the file on the profile the suite
    // actually runs under.
    let session_json = find_file_named(&iso.base, "session.json").unwrap_or_else(|| {
        panic!(
            "no session.json under {}; without a persisted session this test proves nothing",
            iso.base.display()
        )
    });
    let persisted = std::fs::read(&session_json).unwrap();
    assert!(
        !persisted.is_empty(),
        "the persisted session file is empty, so nothing was actually saved"
    );
    assert!(
        !find_bytes(&persisted, token.as_bytes()),
        "the incarnation token was written into the persisted session"
    );
    assert!(
        !find_bytes(&persisted, b"server_epoch"),
        "the token's field name reached the persisted session"
    );

    // ...and nowhere else in the tree either.
    let mut searched = 0usize;
    let mut hits = Vec::new();
    let mut stack = vec![iso.base.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p) {
                searched += 1;
                if find_bytes(&bytes, token.as_bytes()) || find_bytes(&bytes, b"server_epoch") {
                    hits.push(p.display().to_string());
                }
            }
        }
    }
    assert!(searched > 1, "only {searched} file(s) searched");
    assert!(
        hits.is_empty(),
        "the token or its field name reached disk in {searched} files: {hits:?}"
    );
}

/// First file with this name anywhere under `root`.
fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|f| f == name) {
                return Some(p);
            }
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The field must be ABSENT, never an empty string: a consumer must not be
/// able to read "no epoch" as "the empty epoch".
#[test]
fn the_token_is_never_served_as_an_empty_string() {
    let iso = Isolated::start("never-empty");
    let raw = iso.run(&["api", "snapshot"]);
    assert!(
        !raw.contains("\"server_epoch\":\"\""),
        "an empty token was served: {raw}"
    );
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let epoch = &v["result"]["snapshot"]["server_epoch"];
    assert!(
        epoch.is_string() && !epoch.as_str().unwrap().is_empty(),
        "a running server must carry a non-empty token"
    );
}

/// The identity as a USER sees it, from a real subprocess.
///
/// Every other identity assertion here reads a served field. If `--version`
/// printed something else, none of them would notice — and `--version` is the
/// first thing anyone checks to find out what they are running.
#[test]
fn the_binary_reports_the_fork_identity_on_both_version_flags() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag} exited non-zero");
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            text,
            concat!("herdr ", env!("CARGO_PKG_VERSION"), "-heb.1"),
            "{flag} must print the fork identity exactly, not stock"
        );
    }
}

/// On Windows the live-handoff path does not exist, so the token must NOT
/// rotate — and the file must still carry real coverage there rather than
/// compiling to nothing. This asserts the absence explicitly: a platform where
/// every meaningful test is `cfg`'d out is a platform with no coverage at all,
/// which reads identically to a platform where everything passed.
#[cfg(not(unix))]
#[test]
fn the_token_is_stable_where_live_handoff_is_unsupported() {
    let iso = Isolated::start("nohandoff");
    let before = iso.snapshot_epoch().expect("token");
    let out = iso
        .cmd(&[
            "server",
            "live-handoff",
            "--import-exe",
            env!("CARGO_BIN_EXE_herdr"),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "live handoff is not supported here and must not report success"
    );
    let after = iso.snapshot_epoch().expect("token after");
    assert_eq!(
        before, after,
        "nothing was replaced, so the incarnation must not have changed"
    );
}

/// A configured update source must not reach the Windows installer.
///
/// On Windows `install_windows_update_with_installer` runs
/// `irm https://herdr.dev/install.ps1 | iex` and passes it only a channel and
/// a build id. The `download_url` and `sha256` the manifest supplied are
/// discarded, so a configured source would decide the release notes and
/// whether an update is offered while upstream's script decided the binary -
/// replacing this fork with an unrelated build.
///
/// Native, and behavioural rather than structural: it runs the real binary
/// with the variable set and requires the refusal to arrive without the
/// installer being invoked. `PATH` is emptied so that any attempt to launch
/// `powershell` fails loudly rather than silently succeeding against the real
/// one - a refusal that happens to run the installer first would otherwise
/// look identical to a refusal that never did.
#[cfg(not(unix))]
#[test]
fn a_configured_update_source_never_reaches_the_windows_installer() {
    let identity_before = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("--version")
        .output()
        .expect("version");
    let identity_before = String::from_utf8_lossy(&identity_before.stdout)
        .trim()
        .to_string();
    assert!(
        !identity_before.is_empty(),
        "the running build must report an identity for this test to mean anything"
    );

    let out = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("update")
        .env("HERDR_UPDATE_SOURCE", "https://example.invalid/fork.json")
        .env("PATH", "")
        .output()
        .expect("update");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        !out.status.success(),
        "update must refuse; stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("HERDR_UPDATE_SOURCE"),
        "the refusal must name the setting it is refusing: {combined}"
    );
    assert!(
        !combined.contains("install.ps1"),
        "the Windows installer was reached despite a configured source: {combined}"
    );

    // Identity cannot have mixed: the binary that answered before is the one
    // answering now.
    let identity_after = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("--version")
        .output()
        .expect("version");
    assert_eq!(
        String::from_utf8_lossy(&identity_after.stdout).trim(),
        identity_before,
        "the running build's identity changed across a refused update"
    );
}
