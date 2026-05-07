//! Content-side phishing/scam heuristics.
//!
//! Complements [`crate::rules`] (which scores *actions*) by scoring the
//! *content* of an inbound message — the email body, Telegram text, etc.
//! Used alongside sender trust to bump risk on suspicious messages even
//! when the action itself looks benign.
//!
//! Signals fired (independent — a message can match many):
//! - **Lookalike domain**: envelope sender is `paypa1.com`, `arnazon.com`,
//!   `microsft.com`, etc. — Levenshtein 1-2 from a known brand.
//! - **IDN homograph**: envelope domain contains non-ASCII characters
//!   (Cyrillic `а` masquerading as Latin `a`, etc.).
//! - **Display/envelope mismatch**: From-header display name contains a
//!   brand ("PayPal Support") but the envelope domain isn't that brand.
//! - **Urgency phrasing**: "act now", "verify immediately", "your account
//!   will be suspended", "limited time".
//! - **Too-good-to-be-true**: lottery wins, free crypto, inheritance.
//! - **Credential request**: "verify your password", "confirm your card",
//!   "update billing details".
//! - **Suspicious link**: raw-IP URLs, deep subdomain chains, link
//!   shorteners (bit.ly + urgency).
//!
//! The output is data, not a decision — the caller folds it into a final
//! risk score along with sender trust and any rule-based action signals.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Aggregated content-risk evaluation for a single message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContentRiskSignals {
    pub lookalike_domain: bool,
    pub idn_homograph: bool,
    pub display_vs_envelope_mismatch: bool,
    pub urgency_phrasing: bool,
    pub too_good_to_be_true: bool,
    pub credential_request: bool,
    pub suspicious_link: bool,
    /// Names of every pattern that fired, for transparent UI surface.
    pub matched_patterns: Vec<String>,
    /// Aggregate severity in `0.0..=1.0`. Caller multiplies into the
    /// final risk score (or treats >0.5 as a hard "needs human" signal).
    pub score: f64,
}

impl ContentRiskSignals {
    /// True if any individual signal fired.
    pub fn is_suspicious(&self) -> bool {
        !self.matched_patterns.is_empty()
    }
}

/// Per-call inputs to the analyzer. Borrowed so callers don't allocate
/// just to pass them in.
#[derive(Debug, Clone, Copy)]
pub struct MessageInput<'a> {
    /// Body text of the message (email body, Telegram text+caption).
    pub text: &'a str,
    /// Envelope-from address, e.g. `support@paypa1.com`. Used for
    /// lookalike + IDN checks.
    pub envelope_sender: Option<&'a str>,
    /// Display name from the From header, e.g. `"PayPal Support"`. Used
    /// for the brand-spoofing check.
    pub display_name: Option<&'a str>,
}

/// Reasonably common brand domains we want to protect against
/// lookalikes of. Extend as users report misses — phishers iterate
/// faster than we can curate, but the hot 20 cover a lot of real spam.
const PROTECTED_BRANDS: &[(&str, &str)] = &[
    ("paypal", "paypal.com"),
    ("amazon", "amazon.com"),
    ("microsoft", "microsoft.com"),
    ("apple", "apple.com"),
    ("google", "google.com"),
    ("netflix", "netflix.com"),
    ("facebook", "facebook.com"),
    ("instagram", "instagram.com"),
    ("linkedin", "linkedin.com"),
    ("github", "github.com"),
    ("dropbox", "dropbox.com"),
    ("docusign", "docusign.com"),
    ("chase", "chase.com"),
    ("wellsfargo", "wellsfargo.com"),
    ("bankofamerica", "bankofamerica.com"),
    ("hsbc", "hsbc.com"),
    ("citibank", "citi.com"),
    ("revolut", "revolut.com"),
    ("stripe", "stripe.com"),
    ("coinbase", "coinbase.com"),
    ("binance", "binance.com"),
];

static URGENCY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(act now|verify\s+(immediately|now|within|your\s+account)|limited\s+time|expir(es?|ing)\s+(today|soon|in\s+\d+\s*(hours?|minutes?))|your\s+account\s+will\s+be\s+(suspended|deactivated|closed|locked)|urgent(ly)?|immediate\s+action|final\s+(notice|warning))\b",
    )
    .unwrap()
});

static TGTBT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(you\s+have\s+won|congratulations,?\s+you|won\s+\$?\d|claim\s+your\s+prize|free\s+(crypto|btc|bitcoin|gift\s*card)|inheritance|nigerian?\s+prince|unclaimed\s+(funds|inheritance)|government\s+grant|investment\s+opportunity\s+of\s+a\s+lifetime|guaranteed\s+returns?)\b",
    )
    .unwrap()
});

static CREDENTIAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(verify\s+your\s+(password|account|identity|credentials?|ssn|social)|confirm\s+your\s+(password|card|payment|billing|details)|update\s+your\s+(billing|payment|password)|re-?enter\s+your\s+(password|details)|sign\s*in\s+to\s+(verify|confirm))\b",
    )
    .unwrap()
});

static RAW_IP_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}").unwrap());

static SHORTENER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)https?://(bit\.ly|tinyurl\.com|t\.co|goo\.gl|ow\.ly|is\.gd|buff\.ly|rebrand\.ly)/",
    )
    .unwrap()
});

static DEEP_SUBDOMAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    // 5+ dots in the host = "secure.login.paypal.suspicious.example.com"
    // territory. Real brand domains rarely go that deep.
    Regex::new(r"https?://(?:[A-Za-z0-9-]+\.){5,}[A-Za-z]{2,}").unwrap()
});

/// Analyzer state (currently empty — kept as a struct so we can attach
/// per-instance config later: brand list, regex tweaks, scoring weights).
pub struct ContentRiskAnalyzer;

impl Default for ContentRiskAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentRiskAnalyzer {
    pub fn new() -> Self {
        Self
    }

    pub fn analyze(&self, msg: &MessageInput<'_>) -> ContentRiskSignals {
        let mut s = ContentRiskSignals::default();

        // Sender-domain heuristics.
        if let Some(sender) = msg.envelope_sender {
            let domain = sender_domain(sender);
            if let Some(d) = domain.as_deref() {
                if domain_has_non_ascii(d) {
                    s.idn_homograph = true;
                    s.matched_patterns.push("idn_homograph".into());
                }
                if let Some(brand) = matches_brand_lookalike(d) {
                    s.lookalike_domain = true;
                    s.matched_patterns.push(format!("lookalike_domain:{brand}"));
                }
            }

            // Display-vs-envelope mismatch: display name claims a brand
            // but the envelope domain isn't that brand's real domain.
            if let (Some(display), Some(d)) = (msg.display_name, domain.as_deref()) {
                if let Some(brand_keyword) = brand_in_display(display) {
                    let real_domain = PROTECTED_BRANDS
                        .iter()
                        .find(|(k, _)| *k == brand_keyword)
                        .map(|(_, dom)| *dom)
                        .unwrap_or("");
                    if !real_domain.is_empty() && !ends_with_domain(d, real_domain) {
                        s.display_vs_envelope_mismatch = true;
                        s.matched_patterns
                            .push(format!("display_vs_envelope:{brand_keyword}"));
                    }
                }
            }
        }

        // Body-text heuristics.
        if URGENCY_RE.is_match(msg.text) {
            s.urgency_phrasing = true;
            s.matched_patterns.push("urgency_phrasing".into());
        }
        if TGTBT_RE.is_match(msg.text) {
            s.too_good_to_be_true = true;
            s.matched_patterns.push("too_good_to_be_true".into());
        }
        if CREDENTIAL_RE.is_match(msg.text) {
            s.credential_request = true;
            s.matched_patterns.push("credential_request".into());
        }
        if RAW_IP_URL_RE.is_match(msg.text)
            || DEEP_SUBDOMAIN_RE.is_match(msg.text)
            || (SHORTENER_RE.is_match(msg.text)
                && (URGENCY_RE.is_match(msg.text) || CREDENTIAL_RE.is_match(msg.text)))
        {
            s.suspicious_link = true;
            s.matched_patterns.push("suspicious_link".into());
        }

        s.score = aggregate_score(&s);
        s
    }
}

/// Severity weights chosen so any single high-value signal (lookalike,
/// credential request, IDN) crosses the 0.5 "needs human" threshold,
/// while two low-severity signals stack to the same level.
fn aggregate_score(s: &ContentRiskSignals) -> f64 {
    let mut sum: f64 = 0.0;
    if s.lookalike_domain {
        sum += 0.55;
    }
    if s.idn_homograph {
        sum += 0.50;
    }
    if s.display_vs_envelope_mismatch {
        sum += 0.45;
    }
    if s.credential_request {
        sum += 0.40;
    }
    if s.too_good_to_be_true {
        sum += 0.30;
    }
    if s.urgency_phrasing {
        sum += 0.20;
    }
    if s.suspicious_link {
        sum += 0.30;
    }
    sum.min(1.0)
}

fn sender_domain(addr: &str) -> Option<String> {
    let trimmed = addr.trim().trim_matches(|c| c == '<' || c == '>');
    let at = trimmed.find('@')?;
    let dom = &trimmed[at + 1..];
    let dom = dom.split('>').next().unwrap_or(dom);
    Some(dom.trim().to_ascii_lowercase())
}

fn domain_has_non_ascii(domain: &str) -> bool {
    !domain.is_ascii()
}

fn ends_with_domain(domain: &str, brand_domain: &str) -> bool {
    let d = domain.trim_end_matches('.');
    d == brand_domain || d.ends_with(&format!(".{brand_domain}"))
}

fn brand_in_display(display: &str) -> Option<&'static str> {
    let lower = display.to_ascii_lowercase();
    PROTECTED_BRANDS
        .iter()
        .find(|(k, _)| lower.contains(k))
        .map(|(k, _)| *k)
}

/// Returns `Some(brand_keyword)` if the domain is a near-miss of a
/// protected brand domain (Levenshtein 1-2, but not exact).
fn matches_brand_lookalike(domain: &str) -> Option<&'static str> {
    let d = domain.trim_end_matches('.');
    // Exact match against any brand or any brand-subdomain → not a
    // lookalike.
    for (_, brand_dom) in PROTECTED_BRANDS {
        if ends_with_domain(d, brand_dom) {
            return None;
        }
    }
    // Compare to each brand domain. The "host without TLD" (paypal vs
    // paypa1) is what's typically substituted, so we strip the public
    // suffix-ish trailing `.com`/`.net`/etc. before measuring.
    let d_root = strip_tld(d);
    for (brand_keyword, brand_dom) in PROTECTED_BRANDS {
        let b_root = strip_tld(brand_dom);
        if d_root == b_root {
            continue;
        }
        let dist = levenshtein(d_root, b_root);
        let max_dist = if b_root.len() <= 5 { 1 } else { 2 };
        if dist > 0 && dist <= max_dist {
            return Some(brand_keyword);
        }
    }
    None
}

fn strip_tld(domain: &str) -> &str {
    domain
        .rsplit_once('.')
        .map(|(host, _)| host)
        .unwrap_or(domain)
}

/// Iterative Levenshtein with a single-row buffer. Small inputs only
/// (domain names cap out around 30 chars), so allocator pressure is
/// negligible.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(text: &str, sender: Option<&str>, display: Option<&str>) -> ContentRiskSignals {
        let a = ContentRiskAnalyzer::new();
        a.analyze(&MessageInput {
            text,
            envelope_sender: sender,
            display_name: display,
        })
    }

    #[test]
    fn flags_paypal_lookalike_domain() {
        let s = analyze("hi", Some("support@paypa1.com"), None);
        assert!(s.lookalike_domain, "should flag paypa1.com");
        assert!(s.score >= 0.5);
    }

    #[test]
    fn flags_amazon_lookalike() {
        let s = analyze("hi", Some("orders@arnazon.com"), None);
        assert!(s.lookalike_domain);
    }

    #[test]
    fn does_not_flag_real_paypal() {
        let s = analyze("hi", Some("service@paypal.com"), None);
        assert!(!s.lookalike_domain);
    }

    #[test]
    fn does_not_flag_paypal_subdomain() {
        let s = analyze("hi", Some("noreply@notify.paypal.com"), None);
        assert!(!s.lookalike_domain);
    }

    #[test]
    fn flags_idn_homograph() {
        // 'а' (U+0430 CYRILLIC) instead of Latin 'a':
        let s = analyze("hi", Some("admin@аpple.com"), None);
        assert!(s.idn_homograph);
    }

    #[test]
    fn flags_display_envelope_mismatch() {
        let s = analyze(
            "Please verify your account",
            Some("notice@randomdomain.xyz"),
            Some("PayPal Support"),
        );
        assert!(s.display_vs_envelope_mismatch);
    }

    #[test]
    fn does_not_flag_matching_display_and_envelope() {
        let s = analyze("hi", Some("noreply@paypal.com"), Some("PayPal Support"));
        assert!(!s.display_vs_envelope_mismatch);
    }

    #[test]
    fn flags_urgency() {
        let s = analyze(
            "Your account will be suspended unless you act now",
            None,
            None,
        );
        assert!(s.urgency_phrasing);
    }

    #[test]
    fn flags_credential_request() {
        let s = analyze("Please verify your password to continue", None, None);
        assert!(s.credential_request);
    }

    #[test]
    fn flags_too_good_to_be_true() {
        let s = analyze("Congratulations, you have won $5000!", None, None);
        assert!(s.too_good_to_be_true);
    }

    #[test]
    fn flags_raw_ip_link() {
        let s = analyze("Click http://203.0.113.5/login here", None, None);
        assert!(s.suspicious_link);
    }

    #[test]
    fn flags_shortener_with_urgency() {
        let s = analyze("Verify immediately at https://bit.ly/abc123", None, None);
        assert!(s.suspicious_link);
    }

    #[test]
    fn does_not_flag_shortener_alone() {
        // A shortener URL without urgency or credential phrasing is too
        // common (newsletter footers, share links) to flag on its own.
        let s = analyze("Hey check out my photo: https://bit.ly/abc123", None, None);
        assert!(!s.suspicious_link);
    }

    #[test]
    fn benign_message_scores_zero() {
        let s = analyze("Hi, want to grab coffee tomorrow?", Some("a@b.com"), None);
        assert!(!s.is_suspicious());
        assert_eq!(s.score, 0.0);
    }

    #[test]
    fn signals_stack_into_higher_score() {
        let s = analyze(
            "URGENT: verify your password to avoid account suspension. Click https://bit.ly/abc",
            Some("security@paypa1.com"),
            Some("PayPal Security"),
        );
        // Lookalike + display mismatch + urgency + credential + suspicious link.
        assert!(s.lookalike_domain);
        assert!(s.urgency_phrasing);
        assert!(s.credential_request);
        assert!(s.suspicious_link);
        assert!(s.score >= 0.9);
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("paypal", "paypa1"), 1);
        assert_eq!(levenshtein("amazon", "arnazon"), 2);
        assert_eq!(levenshtein("hello", "hello"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn sender_domain_extracts_correctly() {
        assert_eq!(
            sender_domain("alice@example.com"),
            Some("example.com".into())
        );
        assert_eq!(
            sender_domain("  Alice <alice@Example.COM>  "),
            Some("example.com".into())
        );
    }
}
