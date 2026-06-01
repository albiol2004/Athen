//! Helpers for redacting secret-looking values before they reach logs,
//! prompts, persisted traces, or UI surfaces.
//!
//! This module is deliberately dependency-free and conservative. It does not
//! try to validate provider-specific token formats perfectly; instead it
//! catches common high-signal prefixes used by credentials Athen already asks
//! users to configure.

/// Redact common API-key / access-token shapes while preserving enough
/// information for debugging which credential family was involved.
///
/// The helper is intentionally pure so crates can use it at log, tool-output,
/// and provider-error boundaries without pulling in additional dependencies.
///
/// # Examples
///
/// ```
/// use athen_core::redaction::redact_known_secret_shapes;
///
/// let prefix = ["s", "k", "-"].concat();
/// let msg = format!("provider returned 401 for {prefix}{}", "example-token-body");
/// assert_eq!(
///     redact_known_secret_shapes(&msg),
///     "provider returned 401 for sk-…[redacted]",
/// );
/// ```
pub fn redact_known_secret_shapes(input: &str) -> String {
    const PREFIXES: &[&str] = &[
        "sk-",         // OpenAI-compatible providers
        "github_pat_", // GitHub fine-grained PATs
        "ghp_",        // GitHub classic PATs
        "gho_",        // GitHub OAuth tokens
        "ghu_",        // GitHub user-to-server tokens
        "ghs_",        // GitHub server-to-server tokens
        "ghr_",        // GitHub refresh tokens
        "xoxb-",       // Slack bot tokens
        "xoxp-",       // Slack user tokens
        "BSA",         // Brave Search API keys
        "tvly-",       // Tavily API keys
    ];

    let mut redacted = input.to_owned();
    for prefix in PREFIXES {
        redacted = redact_prefixed_tokens(&redacted, prefix);
    }
    redacted
}

fn redact_prefixed_tokens(input: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_start) = input[cursor..].find(prefix) {
        let start = cursor + relative_start;
        out.push_str(&input[cursor..start]);

        let mut end = start + prefix.len();
        for (offset, ch) in input[end..].char_indices() {
            if is_secret_body_char(ch) {
                end = start + prefix.len() + offset + ch.len_utf8();
            } else {
                break;
            }
        }

        let candidate = &input[start..end];
        if candidate.len() >= prefix.len() + 8 {
            out.push_str(prefix);
            out.push_str("…[redacted]");
        } else {
            out.push_str(candidate);
        }

        cursor = end;
    }

    out.push_str(&input[cursor..]);
    out
}

fn is_secret_body_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

#[cfg(test)]
mod tests {
    use super::redact_known_secret_shapes;

    fn joined(parts: &[&str]) -> String {
        parts.concat()
    }

    fn token(prefix_parts: &[&str], body: &str) -> String {
        format!("{}{}", joined(prefix_parts), body)
    }

    #[test]
    fn redacts_openai_compatible_keys() {
        let secret = token(&["s", "k", "-"], "1234567890abcdefghijklmnopqrstuvwxyz");
        let input = format!("provider returned 401 for {secret}");
        let redacted = redact_known_secret_shapes(&input);

        assert_eq!(redacted, "provider returned 401 for sk-…[redacted]");
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redacts_github_fine_grained_tokens() {
        let secret = token(&["github", "_pat", "_"], "1234567890_SECRET_PART");
        let input = format!("using {secret} for git auth");
        let redacted = redact_known_secret_shapes(&input);

        assert_eq!(redacted, "using github_pat_…[redacted] for git auth");
        assert!(!redacted.contains("SECRET_PART"));
    }

    #[test]
    fn redacts_multiple_secret_families_in_one_line() {
        let brave = token(&["B", "S", "A"], "1234567890abcdef");
        let tavily = token(&["tv", "ly", "-"], "1234567890abcdef");
        let input = format!("brave={brave} tavily={tavily}");
        let redacted = redact_known_secret_shapes(&input);

        assert_eq!(redacted, "brave=BSA…[redacted] tavily=tvly-…[redacted]");
    }

    #[test]
    fn preserves_surrounding_punctuation() {
        let secret = token(&["g", "h", "p", "_"], "1234567890abcdef");
        let input = format!("token=({secret}), next=value");
        let redacted = redact_known_secret_shapes(&input);

        assert_eq!(redacted, "token=(ghp_…[redacted]), next=value");
    }

    #[test]
    fn does_not_redact_short_prefix_mentions() {
        let sk_example = token(&["s", "k", "-"], "test");
        let github_example = joined(&["github", "_pat", "_"]);
        let input = format!("docs mention {sk_example} and {github_example} as examples");
        let redacted = redact_known_secret_shapes(&input);

        assert_eq!(redacted, input);
    }

    #[test]
    fn leaves_normal_text_untouched() {
        let input = "normal log line without secrets";
        assert_eq!(redact_known_secret_shapes(input), input);
    }
}
