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

        let mut me = Self {
            base,
            socket,
            server: None,
        };
        me.server = Some(me.cmd(&["server"]).spawn().unwrap());
        me.wait_until_serving();
        me
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
