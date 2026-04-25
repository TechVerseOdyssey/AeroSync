//! Error advice module
//!
//! Provides human-readable descriptions and actionable suggestions for the
//! most common AeroSync errors. Called from the CLI to print friendly output
//! when a top-level operation fails.

use crate::AeroSyncError;

/// A structured suggestion returned for a given error.
pub struct ErrorAdvice {
    /// One-line summary of what went wrong
    pub summary: &'static str,
    /// Step-by-step suggestions to resolve the issue
    pub suggestions: &'static [&'static str],
}

/// Returns an `ErrorAdvice` for the given error, or `None` if the error is
/// generic / has no dedicated advice.
pub fn advice_for(err: &AeroSyncError) -> Option<ErrorAdvice> {
    let msg = err.to_string();
    let msg_lower = msg.to_lowercase();

    // ── 0. RFC-004 R2 (rendezvous + punch) tagged failures ──────────────────
    if msg_lower.contains("[r2_no_token]") {
        return Some(ErrorAdvice {
            summary: "R2 rendezvous token is missing for `peer@rendezvous` destination",
            suggestions: &[
                "Set `AEROSYNC_RENDEZVOUS_TOKEN` in the sender environment",
                "Use a token issued by your rendezvous (`POST /v1/peers/register` / heartbeat)",
                "If multitenant, also set `AEROSYNC_RENDEZVOUS_NAMESPACE` to match JWT `ns`",
            ],
        });
    }
    if msg_lower.contains("[r2_peer_unseen]") {
        return Some(ErrorAdvice {
            summary: "Target peer is registered but has no reachable observed address yet",
            suggestions: &[
                "Start the receiver and ensure it is heartbeating to rendezvous",
                "Re-register / heartbeat the target peer so `observed_addr` is populated",
                "Retry once both sides are online",
            ],
        });
    }
    if msg_lower.contains("[r2_initiate]")
        || msg_lower.contains("[r2_signaling]")
        || msg_lower.contains("[r2_candidate_empty]")
        || msg_lower.contains("[r2_warmup]")
    {
        return Some(ErrorAdvice {
            summary: "WAN R2 negotiation failed before QUIC data transfer started",
            suggestions: &[
                "Verify both peers can reach the same rendezvous and use valid JWTs",
                "Check clock skew and WebSocket reachability to `/v1/sessions/{id}/ws`",
                "If this keeps failing, retry from a simpler network (e.g. IPv6/public) first",
            ],
        });
    }

    // ── 1. Connection refused ────────────────────────────────────────────────
    if msg_lower.contains("connection refused")
        || msg_lower.contains("econnrefused")
        || msg_lower.contains("os error 61")
        || msg_lower.contains("os error 111")
    {
        return Some(ErrorAdvice {
            summary: "Connection refused — the receiver is not running or the port is wrong",
            suggestions: &[
                "Make sure the receiver is started: `aerosync receive --port <PORT>`",
                "Verify the destination address and port match the receiver's settings",
                "Check if a firewall is blocking the port",
                "If using QUIC, try --http-only to fall back to HTTP",
            ],
        });
    }

    // ── 2. Authentication / 401 ──────────────────────────────────────────────
    if msg_lower.contains("unauthorized")
        || msg_lower.contains("401")
        || msg_lower.contains("authentication error")
        || msg_lower.contains("auth")
    {
        return Some(ErrorAdvice {
            summary: "Authentication failed — missing or incorrect token",
            suggestions: &[
                "Add the correct token: `aerosync send ... --token <TOKEN>`",
                "On the receiver, check the required token: `aerosync receive --auth-token <TOKEN>`",
                "Ensure the token has not expired (`aerosync token list`)",
                "Generate a new token: `aerosync token add`",
            ],
        });
    }

    // ── 3. File too large ────────────────────────────────────────────────────
    if msg_lower.contains("file too large")
        || msg_lower.contains("max_file_size")
        || msg_lower.contains("exceeds")
        || msg_lower.contains("413")
    {
        return Some(ErrorAdvice {
            summary: "File exceeds the receiver's maximum allowed size",
            suggestions: &[
                "Start the receiver with a higher limit: `aerosync receive --max-size <BYTES>`",
                "Compress the file before sending to reduce its size",
                "Split large files into smaller chunks manually",
            ],
        });
    }

    // ── 4. Disk space insufficient ───────────────────────────────────────────
    if msg_lower.contains("no space left")
        || msg_lower.contains("insufficient disk")
        || msg_lower.contains("disk full")
        || msg_lower.contains("os error 28")
    {
        return Some(ErrorAdvice {
            summary: "Not enough disk space on the receiver",
            suggestions: &[
                "Free up space in the receive directory",
                "Point the receiver at a disk with more free space: `--save-to /path/with/space`",
                "Check available space: `df -h`",
            ],
        });
    }

    // ── 5. Timeout ───────────────────────────────────────────────────────────
    if msg_lower.contains("timed out")
        || msg_lower.contains("timeout")
        || msg_lower.contains("deadline")
    {
        return Some(ErrorAdvice {
            summary: "Transfer timed out — the connection is too slow or unstable",
            suggestions: &[
                "Increase the timeout: `--timeout <SECONDS>` (default 30s)",
                "Try a smaller file first to verify connectivity",
                "Check network conditions between sender and receiver",
                "Use chunked transfer for large files to benefit from resume",
            ],
        });
    }

    // ── 6. TLS / certificate error (HTTPS self-signed) ───────────────────────
    if msg_lower.contains("certificate")
        || msg_lower.contains("tls")
        || msg_lower.contains("ssl")
        || msg_lower.contains("invalid cert")
    {
        return Some(ErrorAdvice {
            summary: "TLS certificate verification failed (common with self-signed certs)",
            suggestions: &[
                "If using the receiver's auto-generated self-signed cert, the sender must trust it",
                "Use an external trusted certificate: `--tls-cert cert.pem --tls-key key.pem`",
                "For testing only, skip cert verification (not recommended for production)",
                "Or use plain HTTP instead of HTTPS if security is not required",
            ],
        });
    }

    // ── 7. Address already in use / port conflict ────────────────────────────
    if msg_lower.contains("address already in use")
        || msg_lower.contains("os error 48")
        || msg_lower.contains("os error 98")
        || msg_lower.contains("bind")
    {
        return Some(ErrorAdvice {
            summary: "Port is already in use — another process is listening on that port",
            suggestions: &[
                "Choose a different port: `aerosync receive --port <OTHER_PORT>`",
                "Find and stop the conflicting process: `lsof -i :<PORT>` or `ss -tlnp`",
                "If a previous AeroSync instance crashed, wait a few seconds and retry",
            ],
        });
    }

    // ── 8. Path / file not found ─────────────────────────────────────────────
    if msg_lower.contains("no such file")
        || msg_lower.contains("not found")
        || msg_lower.contains("os error 2")
    {
        return Some(ErrorAdvice {
            summary: "File or directory not found",
            suggestions: &[
                "Verify the source path exists: `ls <PATH>`",
                "Use an absolute path to avoid working-directory confusion",
                "Check for typos in the file name",
            ],
        });
    }

    // ── 9. SHA-256 / integrity mismatch ─────────────────────────────────────
    if msg_lower.contains("sha256")
        || msg_lower.contains("checksum")
        || msg_lower.contains("integrity")
        || msg_lower.contains("hash mismatch")
    {
        return Some(ErrorAdvice {
            summary: "File integrity check failed — the received file is corrupt or was modified",
            suggestions: &[
                "Retry the transfer; transient network errors can corrupt data",
                "Check that no proxy or firewall is altering the payload",
                "Skip the check (only if you trust the channel): `--no-verify`",
            ],
        });
    }

    // ── 10. Network unreachable ──────────────────────────────────────────────
    if msg_lower.contains("network unreachable")
        || msg_lower.contains("no route to host")
        || msg_lower.contains("os error 101")
        || msg_lower.contains("os error 65")
    {
        return Some(ErrorAdvice {
            summary: "Network unreachable — cannot connect to the receiver",
            suggestions: &[
                "Verify the receiver's IP address is correct",
                "Ensure both machines are on the same network or there is a valid route",
                "Ping the receiver first: `ping <RECEIVER_IP>`",
                "Check firewall rules on both sides",
            ],
        });
    }

    None
}

/// Format an error for CLI output with optional advice.
///
/// Returns a multi-line string ready to print to stderr.
pub fn format_error_with_advice(err: &AeroSyncError) -> String {
    let mut out = format!("Error: {}", err);

    if let Some(adv) = advice_for(err) {
        out.push_str(&format!("\n\n  {}", adv.summary));
        out.push_str("\n\n  Suggestions:");
        for (i, s) in adv.suggestions.iter().enumerate() {
            out.push_str(&format!("\n    {}. {}", i + 1, s));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advice_connection_refused() {
        let err = AeroSyncError::Network("Connection refused (os error 111)".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("receiver is not running"));
        assert!(!adv.suggestions.is_empty());
    }

    #[test]
    fn test_advice_r2_missing_token() {
        let err = AeroSyncError::InvalidConfig("[R2_NO_TOKEN] missing".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("token"));
    }

    #[test]
    fn test_advice_r2_signaling_failure() {
        let err = AeroSyncError::Network("[R2_SIGNALING] ws closed".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("R2 negotiation"));
    }

    #[test]
    fn test_advice_auth_failure() {
        let err = AeroSyncError::Auth("401 Unauthorized".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("token"));
    }

    #[test]
    fn test_advice_disk_full() {
        let err = AeroSyncError::System("No space left on device (os error 28)".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("disk"));
    }

    #[test]
    fn test_advice_sha256_mismatch() {
        let err = AeroSyncError::Protocol("SHA256 hash mismatch".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("integrity"));
    }

    #[test]
    fn test_advice_timeout() {
        let err = AeroSyncError::Network("request timed out".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("timed out"));
    }

    #[test]
    fn test_advice_port_in_use() {
        let err = AeroSyncError::System("Address already in use (os error 98)".to_string());
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("Port is already in use"));
    }

    #[test]
    fn test_advice_file_not_found() {
        let err = AeroSyncError::FileIo(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ));
        let adv = advice_for(&err).unwrap();
        assert!(adv.summary.contains("not found"));
    }

    #[test]
    fn test_no_advice_for_generic_error() {
        let err = AeroSyncError::Unknown("something completely unexpected".to_string());
        // Should return None (no specific advice)
        assert!(advice_for(&err).is_none());
    }

    #[test]
    fn test_format_error_with_advice_includes_suggestions() {
        let err = AeroSyncError::Network("connection refused".to_string());
        let formatted = format_error_with_advice(&err);
        assert!(formatted.contains("Error:"));
        assert!(formatted.contains("Suggestions:"));
        assert!(formatted.contains("1."));
    }

    #[test]
    fn test_format_error_without_advice_is_plain() {
        let err = AeroSyncError::Unknown("weird internal state".to_string());
        let formatted = format_error_with_advice(&err);
        assert!(formatted.starts_with("Error:"));
        assert!(!formatted.contains("Suggestions:"));
    }
}
