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

use std::path::PathBuf;
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
        std::fs::create_dir_all(base.join("config/herdr")).unwrap();
        std::fs::write(
            base.join("config/herdr/config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
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
        me.wait_for_socket();
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

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server socket never appeared at {}", self.socket.display());
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

    fn server_pids(&self) -> Vec<String> {
        // Only processes whose environment names OUR socket - never a server
        // belonging to the developer running the suite.
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
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
        out
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop"]).output();
        if let Some(mut c) = self.server.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        for pid in self.server_pids() {
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
    std::thread::sleep(Duration::from_millis(1500));
    iso.wait_for_socket();

    let after = iso.snapshot_epoch().expect("token after handoff");
    assert_ne!(before, after, "a handoff must rotate the token");
    assert!(is_lower_hex_32(&after));
    assert_ne!(
        pids_before,
        iso.server_pids(),
        "the token rotated, so the process must actually have been replaced"
    );
}

/// A handoff that FAILS must leave the incarnation alone. Rotating on a failed
/// handoff would tell every client its cache was stale when nothing moved.
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
    let iso = Isolated::start("no-persist");
    iso.run(&["workspace", "create", "--label", "persist", "--focus"]);
    let token = iso.snapshot_epoch().expect("token");
    // Give the server every chance to persist: create state, then stop it
    // cleanly so any save-on-shutdown path runs.
    iso.run(&["tab", "create", "--label", "t", "--focus"]);
    let _ = iso.cmd(&["server", "stop"]).output();
    std::thread::sleep(Duration::from_millis(800));

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
    assert!(searched > 0, "nothing was written, so nothing was proven");
    assert!(
        hits.is_empty(),
        "the token or its field name reached disk in {searched} files: {hits:?}"
    );
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
