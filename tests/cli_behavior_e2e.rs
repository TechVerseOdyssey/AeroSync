//! Phase-1 wrap-up (Task B): exercise the frozen CLI surface beyond
//! `--help`, locking the user-visible behaviour of every top-level
//! subcommand listed in `docs/v0.3.0-frozen-api.md` §10.
//!
//! Coverage:
//!   1. `aerosync token generate` → produces a token, then
//!      `aerosync token verify` validates it (round-trip).
//!   2. `aerosync send` → `aerosync receive` end-to-end over loopback
//!      HTTP with a small temp file. Verifies exit codes AND that the
//!      received bytes match the sender's payload.
//!   3. `aerosync history list` against a freshly isolated
//!      `XDG_CONFIG_HOME`/`HOME` returns 0 with a stable empty-state
//!      message — no panic, no leakage into the developer's real home
//!      directory.
//!   4. `aerosync resume clear <bad-uuid>` exits non-zero with a
//!      stable `Error: Invalid task ID:` prefix.
//!   5. `aerosync status --no-such-flag` exits non-zero (clap-level
//!      argument validation) with a stable `error:` prefix.
//!   6. `aerosync discover --timeout 1` returns within a few seconds
//!      and prints either the empty-set hint or a header.
//!
//! Tests pin only on **stable** prefixes (`Token:`, `Cleared`,
//! `error:`, …) — the body of each line is allowed to evolve so we
//! don't pull this suite onto the critical path of every UX tweak.

use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Bind ephemeral port → drop the listener → return the freed port.
/// There's a tiny TOCTOU window before `aerosync receive` re-binds,
/// but it's fine for serial integration tests on loopback.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Build a `Command` for the freshly-built `aerosync` binary with
/// HOME/XDG_CONFIG_HOME pinned to the supplied tempdir so the CLI
/// never reads or writes the developer's real config / history /
/// token store.
fn isolated_cli(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("aerosync").expect("aerosync bin built by cargo");
    cmd.env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        // Avoid the real user's RUST_LOG bleeding into the assertions.
        .env_remove("RUST_LOG");
    cmd
}

/// Wait up to `timeout` for an HTTP `GET /health` on the given port to
/// succeed. Used as a readiness barrier before we tell `aerosync send`
/// to talk to the in-process receiver — beats `sleep(N)` because we
/// don't have to over-budget for slow CI machines.
fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let probe = std::process::Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "1",
                &url,
            ])
            .output();
        if let Ok(out) = probe {
            if std::str::from_utf8(&out.stdout)
                .map(|s| s.starts_with('2'))
                .unwrap_or(false)
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

// ── 1. token generate → verify round-trip ─────────────────────────────────

/// `token generate` must emit `Token: <hex>` and `Secret: <hex>`; the
/// emitted token must then validate against the same secret. Locks
/// the JWT round-trip path that ships with v0.3.0.
#[test]
fn token_generate_then_verify_round_trips() {
    let home = TempDir::new().unwrap();

    // Use an explicit secret so we don't have to parse the random one
    // back out — verifying with an explicit `--secret` is the same
    // code path the receiver uses internally.
    let secret = "phase1-wrap-test-secret-do-not-reuse";

    let gen_out = isolated_cli(&home)
        .args(["token", "generate", "--secret", secret, "--hours", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Token:"))
        .stdout(predicate::str::contains("Secret:"))
        .stdout(predicate::str::contains("Expires:"))
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(gen_out).expect("utf8 stdout");
    let token_line = stdout
        .lines()
        .find(|l| l.starts_with("Token:"))
        .expect("Token: line");
    let token = token_line.trim_start_matches("Token:").trim().to_string();
    assert!(!token.is_empty(), "extracted token must be non-empty");

    isolated_cli(&home)
        .args(["token", "verify", &token, "--secret", secret])
        .assert()
        .success()
        .stdout(predicate::str::contains("Token is VALID"));
}

// ── 2. send + receive end-to-end loopback ─────────────────────────────────

/// Stand up `aerosync receive --one-shot --http-only` in the
/// background, point `aerosync send` at it, and verify the received
/// bytes match. Catches any regression in the basic sender/receiver
/// HTTP plumbing without depending on QUIC, mDNS, or external
/// services.
#[test]
fn send_to_receive_loopback_http_round_trips() {
    let home = TempDir::new().unwrap();
    let save_dir = TempDir::new().unwrap();

    // Pick an ephemeral port so concurrent test runs don't collide.
    let port = pick_free_port();

    let payload = b"AeroSync-phase1-cli-e2e\n";
    let src_dir = TempDir::new().unwrap();
    let src_path = src_dir.path().join("hello.bin");
    {
        let mut f = std::fs::File::create(&src_path).expect("create payload");
        f.write_all(payload).expect("write payload");
        f.flush().ok();
    }

    let mut receiver = isolated_cli(&home)
        .args([
            "receive",
            "--port",
            &port.to_string(),
            "--bind",
            "127.0.0.1",
            "--save-to",
            save_dir.path().to_str().unwrap(),
            "--http-only",
            "--one-shot",
            "--overwrite",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn receiver");

    if !wait_for_health(port, Duration::from_secs(15)) {
        let _ = receiver.kill();
        let _ = receiver.wait();
        panic!("receiver did not become healthy within 15s on port {port}");
    }

    // `http://...` form forces HTTP (skips negotiate_protocol's QUIC
    // upgrade probe), and `--no-preflight` lets us avoid the disk
    // free-space probe — neither is in scope for this regression.
    let dest = format!("http://127.0.0.1:{port}/upload");
    isolated_cli(&home)
        .args(["send", src_path.to_str().unwrap(), &dest, "--no-preflight"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Completed: 1/1 files"));

    // The receiver in --one-shot mode exits cleanly after the first
    // file lands. Bound the wait so a regression doesn't hang CI.
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit_status = loop {
        match receiver.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = receiver.kill();
                let _ = receiver.wait();
                panic!("receiver did not exit within 15s after one-shot delivery");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    assert!(
        exit_status.success(),
        "one-shot receiver exited with non-zero status: {exit_status:?}"
    );

    let saved = save_dir.path().join("hello.bin");
    assert!(
        saved.exists(),
        "expected received file at {}",
        saved.display()
    );
    let got = std::fs::read(&saved).expect("read received file");
    assert_eq!(
        got, payload,
        "received bytes must match sent bytes (loopback round-trip)"
    );
}

// ── 3. history list against an empty isolated home ────────────────────────

/// Empty home dir → `history list` must succeed (exit 0) and print
/// the stable empty-state hint, NOT panic on missing
/// `~/.config/aerosync/history.jsonl`.
#[test]
fn history_list_on_empty_home_succeeds_with_empty_state() {
    let home = TempDir::new().unwrap();

    isolated_cli(&home)
        .args(["history", "--limit", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No transfer history yet."));
}

// ── 4. resume clear with a bad UUID → non-zero exit, stable prefix ────────

/// Catches accidental rewrites that would either `unwrap()` (panic
/// → SIGABRT) or silently swallow the error.
#[test]
fn resume_clear_rejects_bad_task_id() {
    let home = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();

    isolated_cli(&home)
        .args([
            "resume",
            "clear",
            "not-a-uuid",
            "--state-dir",
            state_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid task ID"));
}

// ── 5. status with an unknown flag → clap-level rejection ─────────────────

/// `aerosync status` is the most stable read-only subcommand on the
/// frozen surface; an unknown flag must exit non-zero with clap's
/// stable `error:` prefix so agent wrappers can detect mis-use.
#[test]
fn status_with_unknown_flag_fails_with_stable_error_prefix() {
    let home = TempDir::new().unwrap();

    isolated_cli(&home)
        .args(["status", "--definitely-not-a-real-flag"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

// ── 6. discover --timeout returns within a few seconds ────────────────────

/// `discover --timeout 1` must complete quickly (we allow a generous
/// 10s ceiling for slow CI), exit 0, and produce well-formed output:
/// either the "No AeroSync receivers found." hint OR the table
/// header — both are stable.
#[test]
fn discover_with_short_timeout_returns_promptly() {
    let home = TempDir::new().unwrap();

    let start = Instant::now();
    let assert = isolated_cli(&home)
        .args(["discover", "--timeout", "1"])
        .assert()
        .success();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "discover --timeout 1 took {elapsed:?}, expected < 10s"
    );

    let out = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let acceptable_empty = out.contains("No AeroSync receivers found.");
    let acceptable_populated = out.contains("NAME") && out.contains("ADDRESS");
    assert!(
        acceptable_empty || acceptable_populated,
        "discover output didn't match either empty-state or table header.\nstdout:\n{out}"
    );
}
