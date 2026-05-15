//! Skill store types: user-authored procedural playbooks the agent loads on
//! demand via the `load_skill` tool. Modeled after Claude Code's Skills — a
//! folder per skill with a `SKILL.md` (frontmatter + body) plus optional
//! sibling files.
//!
//! Skills are distinct from Identity (always-on persona) and Memory
//! (auto-recalled episodic facts):
//!
//! - **Identity** rides in the static prompt prefix on every request.
//! - **Memory** is recalled per-query with a relevance threshold.
//! - **Skills** are *listed* (name + description) in the static prefix; their
//!   bodies are pulled lazily when the agent calls `load_skill(slug)`.
//!
//! See `docs/SKILLS.md` for the full design.
//!
//! The frontmatter parser ([`parse_skill_md`]) lives here so the SKILL.md
//! format is owned by `athen-core` (single source of truth) rather than by
//! whichever crate happens to read files. Persistence-layer implementations
//! re-use it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AthenError, Result};
use crate::identity::ProfileTag;

/// One skill — metadata only. The body lives on disk at `body_path` and is
/// read via [`crate::traits::skill::SkillStore::load_body`] on demand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Skill {
    /// Folder name, kebab-case by convention. Primary key.
    pub slug: String,
    /// Frontmatter `name` — human-readable display name.
    pub name: String,
    /// Frontmatter `description` — what the model sees in the static-prefix
    /// listing. One sentence: "Use when ...".
    pub description: String,
    /// Which profiles see this skill in their prefix listing. Empty means
    /// "no profile" (degenerate but legal). Absent in the frontmatter
    /// resolves to `[ProfileTag::Always]`.
    pub applies_to: Vec<ProfileTag>,
    /// Where this skill came from. Shadowing rule: a `User` skill with the
    /// same slug as a `Bundled` skill takes precedence in listings.
    pub source: SkillSource,
    /// Absolute path of `SKILL.md` on disk.
    pub body_path: PathBuf,
    /// Content hash (frontmatter + body) — bumped on every save. Lets the
    /// sync pass detect filesystem-side edits without re-parsing every
    /// file.
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Where a skill came from. Affects display grouping in the UI and the
/// shadowing rule. Persisted as a string ("Bundled" / "User" / "Imported").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillSource {
    /// Shipped with the Athen binary, written to disk on first boot if
    /// absent. Users can shadow with a same-slug `User` skill.
    Bundled,
    /// Hand-authored via Settings → Skills, or created by the future
    /// `write_skill` agent tool.
    User,
    /// Installed via zip/URL import. Same trust level as `User` in v0;
    /// distinct flag for provenance display only.
    Imported,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::Bundled => "Bundled",
            SkillSource::User => "User",
            SkillSource::Imported => "Imported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Bundled" => Some(SkillSource::Bundled),
            "User" => Some(SkillSource::User),
            "Imported" => Some(SkillSource::Imported),
            _ => None,
        }
    }
}

/// The parsed frontmatter half of a `SKILL.md` file. Body is returned
/// separately by [`parse_skill_md`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    /// Defaults to `[ProfileTag::Always]` when the frontmatter omits the key
    /// or sets `applies_to: all`.
    pub applies_to: Vec<ProfileTag>,
}

impl SkillFrontmatter {
    pub fn default_applies_to() -> Vec<ProfileTag> {
        vec![ProfileTag::Always]
    }
}

/// Parse a `SKILL.md` document into `(frontmatter, body)`.
///
/// Hand-rolled `key: value` parser — we deliberately avoid a YAML dependency
/// because the schema is fixed and tiny:
///
/// ```text
/// ---
/// name: <string>
/// description: <string>
/// applies_to: [<id>, <id>, ...]   # optional; defaults to `all`
/// ---
/// <body markdown>
/// ```
///
/// Rules:
///
/// - The leading `---` must be the first non-whitespace content. A missing
///   opener is an error (we want every skill to declare metadata).
/// - `applies_to` accepts:
///   - omitted → `[Always]`
///   - `all` → `[Always]`
///   - `[a, b, c]` → `[Profile(a), Profile(b), Profile(c)]`
///   - `a, b, c` (no brackets) → same as above
///   - `[!coder]` or `!coder` → `[NotProfile("coder")]` (power-user)
/// - Unknown keys are ignored (forward-compat) but logged by the caller.
/// - Values are trimmed; surrounding quotes are stripped.
pub fn parse_skill_md(input: &str) -> Result<(SkillFrontmatter, String)> {
    let trimmed = input.trim_start_matches('\u{feff}');
    let after_open = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
        .ok_or_else(|| {
            AthenError::Other("SKILL.md must start with `---` frontmatter opener".into())
        })?;

    let (front, body) = split_frontmatter(after_open)?;

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut applies_to: Option<Vec<ProfileTag>> = None;

    for raw_line in front.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = strip_quotes(value.trim());
        match key {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "applies_to" => applies_to = Some(parse_applies_to(value)),
            _ => {
                // Forward-compat: unknown keys are ignored. We don't fail
                // because a future Athen version may add fields.
            }
        }
    }

    let name = name.ok_or_else(|| AthenError::Other("SKILL.md missing `name`".into()))?;
    let description =
        description.ok_or_else(|| AthenError::Other("SKILL.md missing `description`".into()))?;
    let applies_to = applies_to.unwrap_or_else(SkillFrontmatter::default_applies_to);

    Ok((
        SkillFrontmatter {
            name,
            description,
            applies_to,
        },
        body.to_string(),
    ))
}

/// Serialize a `(frontmatter, body)` pair back to the on-disk `SKILL.md`
/// shape. The roundtrip through [`parse_skill_md`] is lossy on unknown keys
/// and on `applies_to` formatting (we always emit the bracketed form), but
/// preserves the semantic fields.
pub fn serialize_skill_md(front: &SkillFrontmatter, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 256);
    out.push_str("---\n");
    out.push_str("name: ");
    out.push_str(&front.name);
    out.push('\n');
    out.push_str("description: ");
    out.push_str(&front.description);
    out.push('\n');
    out.push_str("applies_to: ");
    out.push_str(&serialize_applies_to(&front.applies_to));
    out.push('\n');
    out.push_str("---\n");
    out.push_str(body);
    out
}

fn split_frontmatter(after_open: &str) -> Result<(&str, &str)> {
    // Find the closing `---` on its own line. We scan line-by-line so a `---`
    // that happens inside the body (e.g. a markdown HR) doesn't trip us as
    // long as it isn't the first body line.
    let mut cursor = 0;
    let bytes = after_open.as_bytes();
    while cursor < bytes.len() {
        let line_end = after_open[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(bytes.len());
        let line = after_open[cursor..line_end].trim_end_matches('\r');
        if line.trim() == "---" {
            let front = &after_open[..cursor];
            let body_start = (line_end + 1).min(bytes.len());
            return Ok((front, &after_open[body_start..]));
        }
        cursor = line_end + 1;
    }
    Err(AthenError::Other(
        "SKILL.md frontmatter missing closing `---`".into(),
    ))
}

fn strip_quotes(value: &str) -> &str {
    let v = value.trim();
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn parse_applies_to(value: &str) -> Vec<ProfileTag> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("all") || v.is_empty() {
        return vec![ProfileTag::Always];
    }
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    let mut out = Vec::new();
    for tok in inner.split(',') {
        let tok = strip_quotes(tok.trim());
        if tok.is_empty() {
            continue;
        }
        if let Some(neg) = tok.strip_prefix('!') {
            out.push(ProfileTag::NotProfile(neg.trim().to_string()));
        } else if tok.eq_ignore_ascii_case("all") {
            out.push(ProfileTag::Always);
        } else {
            out.push(ProfileTag::Profile(tok.to_string()));
        }
    }
    if out.is_empty() {
        vec![ProfileTag::Always]
    } else {
        out
    }
}

fn serialize_applies_to(tags: &[ProfileTag]) -> String {
    if tags.len() == 1 && matches!(tags[0], ProfileTag::Always) {
        return "all".to_string();
    }
    let mut parts = Vec::with_capacity(tags.len());
    for tag in tags {
        match tag {
            ProfileTag::Always => parts.push("all".to_string()),
            ProfileTag::Profile(p) => parts.push(p.clone()),
            ProfileTag::NotProfile(p) => parts.push(format!("!{p}")),
        }
    }
    format!("[{}]", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_frontmatter() {
        let input = "---\nname: cold-email\ndescription: Use for cold emails.\n---\n# Body\n";
        let (front, body) = parse_skill_md(input).unwrap();
        assert_eq!(front.name, "cold-email");
        assert_eq!(front.description, "Use for cold emails.");
        assert_eq!(front.applies_to, vec![ProfileTag::Always]);
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn parse_applies_to_bracketed_list() {
        let input =
            "---\nname: x\ndescription: y\napplies_to: [outreach, personal_assistant]\n---\nbody";
        let (front, _) = parse_skill_md(input).unwrap();
        assert_eq!(
            front.applies_to,
            vec![
                ProfileTag::Profile("outreach".into()),
                ProfileTag::Profile("personal_assistant".into()),
            ]
        );
    }

    #[test]
    fn parse_applies_to_unbracketed() {
        let input = "---\nname: x\ndescription: y\napplies_to: outreach, coder\n---\n";
        let (front, _) = parse_skill_md(input).unwrap();
        assert_eq!(
            front.applies_to,
            vec![
                ProfileTag::Profile("outreach".into()),
                ProfileTag::Profile("coder".into()),
            ]
        );
    }

    #[test]
    fn parse_applies_to_all_keyword() {
        let input = "---\nname: x\ndescription: y\napplies_to: all\n---\n";
        let (front, _) = parse_skill_md(input).unwrap();
        assert_eq!(front.applies_to, vec![ProfileTag::Always]);
    }

    #[test]
    fn parse_applies_to_negation() {
        let input = "---\nname: x\ndescription: y\napplies_to: [!coder]\n---\n";
        let (front, _) = parse_skill_md(input).unwrap();
        assert_eq!(
            front.applies_to,
            vec![ProfileTag::NotProfile("coder".into())]
        );
    }

    #[test]
    fn missing_opener_errors() {
        let input = "name: x\ndescription: y\n---\n";
        assert!(parse_skill_md(input).is_err());
    }

    #[test]
    fn missing_closer_errors() {
        let input = "---\nname: x\ndescription: y\n";
        assert!(parse_skill_md(input).is_err());
    }

    #[test]
    fn missing_name_errors() {
        let input = "---\ndescription: y\n---\n";
        assert!(parse_skill_md(input).is_err());
    }

    #[test]
    fn missing_description_errors() {
        let input = "---\nname: x\n---\n";
        assert!(parse_skill_md(input).is_err());
    }

    #[test]
    fn unknown_keys_ignored() {
        let input = "---\nname: x\ndescription: y\nfuture_field: 42\n---\nbody";
        let (front, body) = parse_skill_md(input).unwrap();
        assert_eq!(front.name, "x");
        assert_eq!(body, "body");
    }

    #[test]
    fn quoted_values_stripped() {
        let input = "---\nname: \"quoted name\"\ndescription: 'single'\n---\n";
        let (front, _) = parse_skill_md(input).unwrap();
        assert_eq!(front.name, "quoted name");
        assert_eq!(front.description, "single");
    }

    #[test]
    fn body_preserved_verbatim_after_close() {
        let input = "---\nname: x\ndescription: y\n---\n\nLine 1\n---\nNot a closer\n";
        let (_, body) = parse_skill_md(input).unwrap();
        assert_eq!(body, "\nLine 1\n---\nNot a closer\n");
    }

    #[test]
    fn roundtrip_serialize_parse() {
        let original = SkillFrontmatter {
            name: "cold-email".to_string(),
            description: "Use for cold emails.".to_string(),
            applies_to: vec![
                ProfileTag::Profile("outreach".into()),
                ProfileTag::Profile("personal_assistant".into()),
            ],
        };
        let serialized = serialize_skill_md(&original, "# Body\n");
        let (parsed, body) = parse_skill_md(&serialized).unwrap();
        assert_eq!(parsed, original);
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn roundtrip_always_emits_all_keyword() {
        let front = SkillFrontmatter {
            name: "x".to_string(),
            description: "y".to_string(),
            applies_to: vec![ProfileTag::Always],
        };
        let serialized = serialize_skill_md(&front, "body");
        assert!(serialized.contains("applies_to: all\n"));
        let (parsed, _) = parse_skill_md(&serialized).unwrap();
        assert_eq!(parsed.applies_to, vec![ProfileTag::Always]);
    }

    #[test]
    fn source_string_roundtrip() {
        for src in [
            SkillSource::Bundled,
            SkillSource::User,
            SkillSource::Imported,
        ] {
            assert_eq!(SkillSource::parse(src.as_str()), Some(src));
        }
        assert_eq!(SkillSource::parse("nope"), None);
    }

    #[test]
    fn windows_line_endings_supported() {
        let input = "---\r\nname: x\r\ndescription: y\r\n---\r\nbody";
        let (front, body) = parse_skill_md(input).unwrap();
        assert_eq!(front.name, "x");
        assert_eq!(body, "body");
    }
}
