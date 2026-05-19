//! Per-tool truncation policy applied at the executor's serialization point.
//!
//! Tool results flow into two places: the audit trail (`TaskStep.output`,
//! which keeps the full JSON) and the next LLM call's conversation context
//! (a stringified copy threaded back as a `Tool` role message). Only the
//! second is capped here — we never want a multi-MB build log or a fetched
//! web page to blow the context window, but the audit must stay complete.
//!
//! Per-tool defaults live in `policy_for`. Athen owns its tools, so each
//! one's expected shape is known: shell output benefits from head+tail
//! (prologue + epilogue carry signal), file/page output benefits from a
//! plain head cap, and small structured results (memory, search clamped at
//! 20, send-email status) pass through untouched.

use std::fmt::Write;

/// Strategy for capping a single tool result before it re-enters the LLM
/// conversation context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationPolicy {
    /// Pass through unchanged. Reserved for tools whose results are bounded
    /// at the source (memory_*, web_search clamped to 20 results upstream)
    /// or already small (send-email status, spawn/kill acks).
    None,
    /// Keep the first `max` bytes; replace the tail with an elision marker.
    Chars { max: usize },
    /// Keep the first `head` bytes and the last `tail` bytes; replace the
    /// middle with an elision marker. Best for shell output where both the
    /// prologue (command being run, early errors) and the epilogue (final
    /// exit message, summary) carry signal.
    HeadTail { head: usize, tail: usize },
    /// Like `HeadTail`, but additionally scans the elided middle for lines
    /// that look like errors / warnings / panics / tracebacks, and keeps up
    /// to `max_signal_lines` of them between the head and tail. The signal
    /// block is bracketed with a clear marker so the model can tell it's
    /// not contiguous output. Used for shell tools where the most
    /// debug-useful line is often in the middle (compiler error, test
    /// failure, stack trace), not at the prologue or epilogue.
    SignalHeadTail {
        head: usize,
        tail: usize,
        max_signal_lines: usize,
    },
}

/// Default truncation policy for a built-in tool name. Unknown tools (e.g.
/// MCP tools registered at runtime) get a generous `Chars` cap so nothing
/// reaches the model unbounded.
pub fn policy_for(name: &str) -> TruncationPolicy {
    match name {
        "shell_execute" => TruncationPolicy::SignalHeadTail {
            head: 8_000,
            tail: 4_000,
            max_signal_lines: 20,
        },
        "shell_logs" => TruncationPolicy::SignalHeadTail {
            head: 4_000,
            tail: 8_000,
            max_signal_lines: 20,
        },
        "shell_spawn" | "shell_kill" => TruncationPolicy::None,

        "read" => TruncationPolicy::Chars { max: 40_000 },
        "grep" => TruncationPolicy::Chars { max: 20_000 },
        "list_directory" => TruncationPolicy::Chars { max: 8_000 },
        "write" | "edit" => TruncationPolicy::Chars { max: 2_000 },

        "web_fetch" => TruncationPolicy::Chars { max: 20_000 },
        "web_search" => TruncationPolicy::None,

        "memory_store" | "memory_recall" => TruncationPolicy::None,
        "email_send" => TruncationPolicy::None,
        "send_telegram" => TruncationPolicy::None,
        "install_package" | "uninstall_package" | "list_installed_packages" => {
            TruncationPolicy::Chars { max: 8_000 }
        }

        _ => TruncationPolicy::Chars { max: 20_000 },
    }
}

/// Apply a policy to a serialized tool result. Returns the (possibly capped)
/// string. Slicing snaps to UTF-8 char boundaries so we never split a
/// multi-byte codepoint.
pub fn apply(policy: TruncationPolicy, s: String) -> String {
    let len = s.len();
    match policy {
        TruncationPolicy::None => s,
        TruncationPolicy::Chars { max } => {
            if len <= max {
                return s;
            }
            let cut = floor_char_boundary(&s, max);
            let mut out = String::with_capacity(cut + 96);
            out.push_str(&s[..cut]);
            let _ = write!(
                out,
                "\n\n[TRUNCATED: {} bytes elided of {} total. Refine your query (tighter pattern, smaller range, sub-page) to see the rest.]",
                len - cut,
                len
            );
            out
        }
        TruncationPolicy::HeadTail { head, tail } => {
            if len <= head.saturating_add(tail) {
                return s;
            }
            let head_end = floor_char_boundary(&s, head);
            let tail_start = ceil_char_boundary(&s, len - tail);
            let elided = tail_start.saturating_sub(head_end);
            let mut out = String::with_capacity(head_end + (len - tail_start) + 96);
            out.push_str(&s[..head_end]);
            let _ = write!(
                out,
                "\n\n[TRUNCATED: {} bytes elided in the middle of {} total. Refine your query to see the missing region.]\n\n",
                elided, len
            );
            out.push_str(&s[tail_start..]);
            out
        }
        TruncationPolicy::SignalHeadTail {
            head,
            tail,
            max_signal_lines,
        } => {
            if len <= head.saturating_add(tail) {
                return s;
            }
            let head_end = floor_char_boundary(&s, head);
            let tail_start = ceil_char_boundary(&s, len - tail);
            let elided = tail_start.saturating_sub(head_end);
            let middle = &s[head_end..tail_start];
            let signals = collect_signal_lines(middle, max_signal_lines);
            let mut out = String::with_capacity(head_end + (len - tail_start) + 256);
            out.push_str(&s[..head_end]);
            let _ = write!(
                out,
                "\n\n[TRUNCATED: {} bytes elided in the middle of {} total. Refine your query to see the missing region.]\n",
                elided, len
            );
            if !signals.is_empty() {
                let kept = signals.len();
                let _ = write!(
                    out,
                    "\n[SIGNAL LINES from elided middle ({} kept, max {}): error/warn/fail/panic/traceback matches]\n",
                    kept, max_signal_lines
                );
                for line in signals {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("[END SIGNAL LINES]\n");
            }
            out.push('\n');
            out.push_str(&s[tail_start..]);
            out
        }
    }
}

/// Scan `middle` for lines that look like errors / warnings / panics /
/// tracebacks (case-insensitive substring match). Returns the first `max`
/// matches in their original order; ties are broken by document order.
/// Lines longer than 400 bytes are clipped to keep the signal block bounded.
fn collect_signal_lines(middle: &str, max: usize) -> Vec<&str> {
    if max == 0 {
        return Vec::new();
    }
    let needles = [
        "error",
        "warn",
        "fail",
        "panic",
        "fatal",
        "denied",
        "traceback",
        "segfault",
        "abort",
    ];
    let mut out: Vec<&str> = Vec::with_capacity(max);
    for raw in middle.lines() {
        let line = raw.trim_end_matches(['\r', '\u{0085}']);
        if line.is_empty() {
            continue;
        }
        // Case-insensitive substring match. ASCII lowercasing is fine for
        // these English keywords; non-ASCII bytes simply won't match and
        // are passed through.
        let lower_check = line.to_ascii_lowercase();
        if needles.iter().any(|n| lower_check.contains(n)) {
            let clipped = if line.len() > 400 {
                let cut = floor_char_boundary(line, 400);
                &line[..cut]
            } else {
                line
            };
            out.push(clipped);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_passes_through_unchanged() {
        let s = "hello world".to_string();
        let out = apply(TruncationPolicy::Chars { max: 100 }, s.clone());
        assert_eq!(out, s);
    }

    #[test]
    fn over_cap_truncates_with_marker() {
        let s = "x".repeat(1000);
        let out = apply(TruncationPolicy::Chars { max: 200 }, s);
        assert!(out.starts_with(&"x".repeat(200)));
        assert!(out.contains("[TRUNCATED:"));
        assert!(out.contains("800 bytes elided"));
        assert!(out.contains("of 1000 total"));
    }

    #[test]
    fn head_tail_keeps_both_ends() {
        let mut s = String::new();
        s.push_str(&"H".repeat(100));
        s.push_str(&"M".repeat(2000));
        s.push_str(&"T".repeat(100));
        let out = apply(TruncationPolicy::HeadTail { head: 80, tail: 80 }, s);
        assert!(out.starts_with(&"H".repeat(80)));
        assert!(out.ends_with(&"T".repeat(80)));
        assert!(out.contains("[TRUNCATED:"));
        assert!(out.contains("middle"));
    }

    #[test]
    fn head_tail_under_combined_passes_through() {
        let s = "abcde".repeat(20);
        let original = s.clone();
        let out = apply(
            TruncationPolicy::HeadTail {
                head: 100,
                tail: 100,
            },
            s,
        );
        assert_eq!(out, original);
    }

    #[test]
    fn none_passes_through_even_when_huge() {
        let s = "x".repeat(1_000_000);
        let out = apply(TruncationPolicy::None, s.clone());
        assert_eq!(out.len(), s.len());
    }

    #[test]
    fn slicing_respects_utf8_boundaries() {
        // 4-byte codepoint right at the cut point — must not split.
        let mut s = String::new();
        for _ in 0..50 {
            s.push('🦀'); // 4 bytes each → 200 bytes total
        }
        let out = apply(TruncationPolicy::Chars { max: 101 }, s);
        // Must be valid UTF-8 (already enforced by &str), and the cut should
        // have backed off from 101 → 100 (a clean boundary).
        assert!(out.contains("[TRUNCATED:"));
        // Original 200 bytes; 100 bytes of crab survive (25 codepoints).
        let head = out.split("\n\n[TRUNCATED").next().unwrap();
        assert_eq!(head.chars().count(), 25);
    }

    #[test]
    fn policy_for_known_tools() {
        assert!(matches!(
            policy_for("shell_execute"),
            TruncationPolicy::SignalHeadTail { .. }
        ));
        assert!(matches!(
            policy_for("shell_logs"),
            TruncationPolicy::SignalHeadTail { .. }
        ));
        assert!(matches!(policy_for("memory_store"), TruncationPolicy::None));
        assert!(matches!(policy_for("web_search"), TruncationPolicy::None));
        assert!(matches!(policy_for("read"), TruncationPolicy::Chars { .. }));
    }

    #[test]
    fn signal_head_tail_surfaces_error_line_from_middle() {
        let mut s = String::new();
        s.push_str(&"H".repeat(8_000));
        s.push_str("\nignored noise line one\n");
        s.push_str("compiler error[E0382]: borrow of moved value: `x`\n");
        s.push_str("more noise\n");
        s.push_str("WARN: deprecated API used\n");
        s.push_str(&"M".repeat(20_000));
        s.push_str(&"T".repeat(4_000));

        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 10,
            },
            s,
        );
        assert!(out.contains("[SIGNAL LINES from elided middle"));
        assert!(out.contains("compiler error[E0382]"));
        assert!(out.contains("WARN: deprecated API used"));
        assert!(out.contains("[END SIGNAL LINES]"));
        // Head and tail bracket the signal block.
        assert!(out.starts_with(&"H".repeat(80)));
        assert!(out.ends_with(&"T".repeat(80)));
    }

    #[test]
    fn signal_head_tail_caps_signal_line_count() {
        let mut s = String::new();
        s.push_str(&"H".repeat(8_000));
        for i in 0..50 {
            s.push_str(&format!("\nerror #{i} in module foo\n"));
        }
        s.push_str(&"T".repeat(4_000));

        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 5,
            },
            s,
        );
        // First 5 must be kept; 6th onwards dropped.
        for i in 0..5 {
            assert!(
                out.contains(&format!("error #{i} ")),
                "expected error #{i} kept"
            );
        }
        assert!(!out.contains("error #5 "));
    }

    #[test]
    fn signal_head_tail_skips_block_when_no_signal_matches() {
        let mut s = String::new();
        s.push_str(&"H".repeat(8_000));
        s.push_str(&"plain output with no keywords\n".repeat(500));
        s.push_str(&"T".repeat(4_000));
        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 10,
            },
            s,
        );
        assert!(!out.contains("[SIGNAL LINES"));
        assert!(out.contains("[TRUNCATED:"));
    }

    #[test]
    fn signal_head_tail_under_combined_passes_through() {
        let s = "short output with no error here\n".repeat(20);
        let original = s.clone();
        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 10,
            },
            s,
        );
        assert_eq!(out, original);
    }

    #[test]
    fn signal_head_tail_clips_very_long_signal_lines() {
        let mut s = String::new();
        s.push_str(&"H".repeat(8_000));
        s.push('\n');
        s.push_str("error: ");
        s.push_str(&"x".repeat(2_000));
        s.push('\n');
        s.push_str(&"T".repeat(4_000));
        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 10,
            },
            s,
        );
        // The 2000-x line must be clipped to ~400 bytes inside the signal block.
        let signal_block = out
            .split("[SIGNAL LINES")
            .nth(1)
            .and_then(|s| s.split("[END SIGNAL LINES]").next())
            .expect("signal block present");
        // Find the longest line in the signal block.
        let longest = signal_block.lines().map(|l| l.len()).max().unwrap_or(0);
        assert!(longest <= 400, "signal line not clipped: {} bytes", longest);
    }

    #[test]
    fn signal_head_tail_zero_max_skips_signal_block() {
        let mut s = String::new();
        s.push_str(&"H".repeat(8_000));
        s.push_str("\nerror in middle\n");
        s.push_str(&"M".repeat(5_000));
        s.push_str(&"T".repeat(4_000));
        let out = apply(
            TruncationPolicy::SignalHeadTail {
                head: 8_000,
                tail: 4_000,
                max_signal_lines: 0,
            },
            s,
        );
        assert!(!out.contains("[SIGNAL LINES"));
    }

    #[test]
    fn policy_for_unknown_tool_falls_back_to_safe_cap() {
        match policy_for("some_dynamic_mcp__do_thing") {
            TruncationPolicy::Chars { max } => assert!(max > 0 && max <= 50_000),
            other => panic!("expected Chars fallback, got {:?}", other),
        }
    }
}
