//! Turning a block of text into the bytes a pty should receive for a **paste**.
//!
//! Every path that puts more than a keystroke into a pane — clipboard paste, a file drop,
//! a dictated transcript — has to go through here. Writing the raw string instead is a way
//! to lose most of it, and the loss is invisible from this side because the write itself
//! succeeds.
//!
//! Why: the tty line discipline holds a bounded input queue (1024 bytes on macOS/BSD), so a
//! blob larger than that is handed to the program in *several* reads no matter how it was
//! written. A TUI reading raw stdin sees those reads as separate events and is entitled to
//! treat them differently — Claude Code's input box kept only the final 254-byte read of a
//! 1277-character transcript and silently dropped the first 1023. Bracketing the payload in
//! `ESC[200~ … ESC[201~` tells the reader "this is one paste, buffer until the end marker",
//! which is exactly the contract that makes a chunked delivery safe.

/// Turn raw text into the exact bytes to write to the pty for a paste.
///
/// Two transforms, both matching how Windows Terminal feeds a paste to conpty:
/// 1. **Normalize line endings to CR (`\r`).** Windows console input treats CR as Enter; a bare
///    LF (`\n`) is mishandled by conpty/PSReadLine, which strands the caret and fragments a
///    multi-line paste across `>>` continuation prompts. Selection text joins rows with `\n`,
///    and external clipboards carry `\r\n`/`\n` — all collapse to `\r` here.
/// 2. **Bracket** the payload in `ESC[200~ … ESC[201~` *only* when the app enabled bracketed-paste
///    mode (DECSET 2004 — modern PSReadLine / PowerShell 7, and every TUI worth pasting into).
///    Then the reader inserts it as one literal paste (caret at the end, no premature execution,
///    no dependence on how the kernel split the bytes). Old shells (Windows PowerShell 5.1)
///    don't set the mode, so the CR-normalized text is sent bare — still the correct Enter
///    handling, and a line editor reassembles a split read on its own.
#[tracing::instrument(level = "debug", ret)]
pub fn prepare_paste(text: &str, bracketed: bool) -> String {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\u{1b}[200~{normalized}\u{1b}[201~")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_text_when_the_app_never_enabled_bracketed_paste() {
        assert_eq!(prepare_paste("hello", false), "hello");
    }

    #[test]
    fn brackets_the_payload_when_the_app_asked_for_it() {
        assert_eq!(prepare_paste("hello", true), "\u{1b}[200~hello\u{1b}[201~");
    }

    #[test]
    fn every_line_ending_collapses_to_cr() {
        assert_eq!(prepare_paste("a\r\nb\nc", false), "a\rb\rc");
        assert_eq!(
            prepare_paste("a\r\nb\nc", true),
            "\u{1b}[200~a\rb\rc\u{1b}[201~"
        );
    }

    /// The regression this module exists for: a transcript longer than the tty's 1024-byte
    /// input queue must arrive as ONE bracketed unit, so a reader that gets it in several
    /// reads still reassembles the whole thing. The exact length is the dictation that was
    /// lost — 1277 characters, of which only the last 254 reached the pane.
    #[test]
    fn a_transcript_past_the_tty_queue_is_still_one_paste() {
        let long = "x".repeat(1277);
        let out = prepare_paste(&long, true);
        assert!(out.starts_with("\u{1b}[200~"), "opens the paste");
        assert!(out.ends_with("\u{1b}[201~"), "closes the paste");
        assert_eq!(
            out.len(),
            long.len() + 12,
            "the payload is carried whole, not truncated at the queue boundary"
        );
    }
}
