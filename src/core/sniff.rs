//! Content-type sniffing pipeline (RFC-003 §5).
//!
//! Sniffing is performed exactly once on the **sender**, before the
//! first chunk of the transfer is emitted. The result is sealed into
//! [`aerosync_proto::Metadata::content_type`] and never re-evaluated
//! by the receiver.
//!
//! Algorithm:
//!
//! 1. If the application explicitly passed `content_type=...` via
//!    [`crate::core::metadata::MetadataBuilder::content_type`], that
//!    value is honored verbatim and this module is **not** called.
//! 2. Otherwise, the engine peeks the first 8192 bytes of the file
//!    and calls [`sniff_content_type`].
//! 3. We try [`infer::get`] (magic-number table, ~100 formats).
//! 4. If `infer` returns nothing we fall back to
//!    `mime_guess::from_path`, which is extension-based.
//! 5. Final fallback: `application/octet-stream` (RFC-2046).

use std::path::Path;

/// Number of bytes the engine peeks from the start of the file when
/// content-type sniffing. Matches `infer`'s expected lookahead window.
pub const SNIFF_PEEK_BYTES: usize = 8192;

/// Universal binary fallback — RFC-2046.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// Sniff a MIME type from a file's bytes + path. See module docs for
/// the full algorithm.
///
/// The `peek_bytes` slice MAY be shorter than [`SNIFF_PEEK_BYTES`] —
/// for very small files, callers pass the entire file content. An
/// empty slice short-circuits past `infer` and goes straight to the
/// extension-based fallback.
pub fn sniff_content_type(path: &Path, peek_bytes: &[u8]) -> String {
    if !peek_bytes.is_empty() {
        if let Some(kind) = infer::get(peek_bytes) {
            return kind.mime_type().to_string();
        }
    }
    if let Some(guess) = mime_guess::from_path(path).first() {
        return guess.essence_str().to_string();
    }
    DEFAULT_CONTENT_TYPE.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sniff_png_magic_bytes() {
        // PNG signature: 89 50 4E 47 0D 0A 1A 0A
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let mime = sniff_content_type(&PathBuf::from("anything.bin"), &bytes);
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn sniff_pdf_magic_bytes() {
        // PDF signature: %PDF-
        let bytes = b"%PDF-1.7\n%\xC7\xEC\x8F\xA2";
        let mime = sniff_content_type(&PathBuf::from("doc.pdf"), bytes);
        assert_eq!(mime, "application/pdf");
    }

    #[test]
    fn sniff_falls_back_to_extension_when_magic_unknown() {
        // A plain ASCII text file has no infer signature, so we must
        // fall through to mime_guess for `*.txt`.
        let bytes = b"hello world\n";
        let mime = sniff_content_type(&PathBuf::from("note.txt"), bytes);
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn sniff_falls_back_to_octet_stream_when_unknown() {
        // No magic, no recognizable extension → octet-stream.
        let bytes = b"random opaque bytes\x00\xff";
        let mime = sniff_content_type(&PathBuf::from("blob.unknownext"), bytes);
        assert_eq!(mime, DEFAULT_CONTENT_TYPE);
    }

    #[test]
    fn sniff_handles_empty_peek_with_known_extension() {
        let mime = sniff_content_type(&PathBuf::from("page.html"), &[]);
        assert_eq!(mime, "text/html");
    }

    #[test]
    fn sniff_handles_empty_peek_and_no_extension() {
        let mime = sniff_content_type(&PathBuf::from("noext"), &[]);
        assert_eq!(mime, DEFAULT_CONTENT_TYPE);
    }

    #[test]
    fn sniff_prefers_magic_over_misleading_extension() {
        // PNG bytes but `.txt` extension → infer wins, not mime_guess.
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let mime = sniff_content_type(&PathBuf::from("oops.txt"), &bytes);
        assert_eq!(mime, "image/png");
    }
}
