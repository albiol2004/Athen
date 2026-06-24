//! LLM-based entity extraction for the knowledge graph.

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::error::Result;
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::memory::{Entity, EntityExtractor, EntityType, ExtractionResult};

/// Extracts entities and relationships from text using an LLM.
pub struct LlmEntityExtractor {
    router: Box<dyn LlmRouter>,
}

impl LlmEntityExtractor {
    pub fn new(router: Box<dyn LlmRouter>) -> Self {
        Self { router }
    }
}

const EXTRACTION_PROMPT: &str = r#"Extract entities and relationships from this text. Return JSON only, no other text:
{"entities": [{"name": "...", "type": "Person|Organization|Project|Event|Document|Concept"}], "relations": [{"from": "entity_name", "relation": "verb/relationship", "to": "entity_name", "importance": 0.0-1.0}]}
importance: 0.9 = critical relationship (family, partner), 0.5 = notable, 0.2 = minor detail.
Only include clearly stated entities. Be concise."#;

#[async_trait]
impl EntityExtractor for LlmEntityExtractor {
    async fn extract(&self, text: &str) -> Result<ExtractionResult> {
        let request = LlmRequest {
            profile: ModelProfile::Judges,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(format!("{EXTRACTION_PROMPT}\n\nText:\n{text}")),
            }],
            max_tokens: Some(500),
            temperature: Some(0.0),
            tools: None,
            system_prompt: None,
            reasoning_effort: athen_core::llm::ReasoningEffort::default(),
        };

        // 30-second timeout — local models can be slow.
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.router.route(&request),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                warn!("LLM entity extraction failed: {e}");
                return Ok(ExtractionResult {
                    entities: vec![],
                    relations: vec![],
                });
            }
            Err(_) => {
                warn!("LLM entity extraction timed out");
                return Ok(ExtractionResult {
                    entities: vec![],
                    relations: vec![],
                });
            }
        };

        // Parse the JSON response. Try to find JSON in the response content.
        let content = response.content.trim();
        let json_str = extract_json_block(content);

        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(val) => Ok(parse_extraction_json(&val)),
            Err(e) => {
                debug!("Failed to parse entity extraction JSON: {e}");
                Ok(ExtractionResult {
                    entities: vec![],
                    relations: vec![],
                })
            }
        }
    }
}

/// Try to extract a JSON block from LLM output that may contain markdown fences.
fn extract_json_block(text: &str) -> &str {
    // Try to find ```json ... ``` first
    if let Some(start) = text.find("```json") {
        let after_fence = &text[start + 7..];
        if let Some(end) = after_fence.find("```") {
            return after_fence[..end].trim();
        }
    }
    // Try ``` ... ```
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        if let Some(end) = after_fence.find("```") {
            return after_fence[..end].trim();
        }
    }
    // Try to find the first { and last }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            return &text[start..=end];
        }
    }
    text
}

fn parse_entity_type(s: &str) -> EntityType {
    match s {
        "Person" => EntityType::Person,
        "Organization" => EntityType::Organization,
        "Project" => EntityType::Project,
        "Event" => EntityType::Event,
        "Document" => EntityType::Document,
        _ => EntityType::Concept,
    }
}

/// Classification of a candidate entity name w.r.t. role-label handling.
///
/// The LLM frequently surfaces the speaker as `"user"`, `"the user"`, or `"you"`.
/// We must keep the user as a stable subject node (so the KG isn't full of
/// orphans like `puppy ← arrives_in August` with no owner), but we must also
/// drop true role labels (`assistant`, `system`) — those are conversational
/// scaffolding, not entities.
///
/// Returns:
/// - `NameDecision::Drop` for `assistant` / `system` (case-insensitive)
/// - `NameDecision::Keep("user")` for `user` / `the user` / `you` (case-insensitive)
/// - `NameDecision::Keep(original)` for everything else
///
/// Only those three exact aliases collapse to `user`; anything containing
/// "user" as a substring (e.g. "power user", "user_id") is left alone.
#[derive(Debug, PartialEq, Eq)]
enum NameDecision {
    Drop,
    Keep(String),
}

fn classify_name(name: &str) -> NameDecision {
    let trimmed = name.trim();
    if trimmed.eq_ignore_ascii_case("assistant") || trimmed.eq_ignore_ascii_case("system") {
        return NameDecision::Drop;
    }
    if trimmed.eq_ignore_ascii_case("user")
        || trimmed.eq_ignore_ascii_case("the user")
        || trimmed.eq_ignore_ascii_case("you")
    {
        return NameDecision::Keep("user".to_string());
    }
    NameDecision::Keep(trimmed.to_string())
}

fn parse_extraction_json(val: &serde_json::Value) -> ExtractionResult {
    let entities = val
        .get("entities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let raw = e.get("name")?.as_str()?.trim().to_string();
                    // Normalise role-ish aliases ("user" / "the user" / "you" → "user";
                    // "assistant" / "system" → dropped). Keeps the user as a stable
                    // subject node so the KG doesn't get orphaned objects.
                    let (name, entity_type) = match classify_name(&raw) {
                        NameDecision::Drop => return None,
                        NameDecision::Keep(n) if n == "user" => (n, EntityType::Person),
                        NameDecision::Keep(n) => {
                            let et = parse_entity_type(
                                e.get("type").and_then(|v| v.as_str()).unwrap_or("Concept"),
                            );
                            (n, et)
                        }
                    };
                    // Filter out garbage: too short, tool names, parenthesised junk.
                    // (We DON'T re-apply role-label filtering here — classify_name
                    // already handled that.)
                    if name.len() < 2 || name.contains('_') || name.contains('(') {
                        return None;
                    }
                    Some(Entity {
                        id: None,
                        entity_type,
                        name,
                        metadata: serde_json::json!({}),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let relations = val
        .get("relations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let from_raw = r.get("from")?.as_str()?.to_string();
                    let relation = r.get("relation")?.as_str()?.to_string();
                    let to_raw = r.get("to")?.as_str()?.to_string();
                    // Normalise endpoints so they match what the entity-side
                    // pass stored: drop relations whose endpoint is a true
                    // role label, collapse user-aliases to canonical "user".
                    let from = match classify_name(&from_raw) {
                        NameDecision::Drop => return None,
                        NameDecision::Keep(n) => n,
                    };
                    let to = match classify_name(&to_raw) {
                        NameDecision::Drop => return None,
                        NameDecision::Keep(n) => n,
                    };
                    let importance =
                        r.get("importance").and_then(|v| v.as_f64()).unwrap_or(0.5) as f32;
                    Some((from, relation, to, importance.clamp(0.0, 1.0)))
                })
                .collect()
        })
        .unwrap_or_default();

    ExtractionResult {
        entities,
        relations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_block_plain() {
        let input = r#"{"entities": [], "relations": []}"#;
        assert_eq!(extract_json_block(input), input);
    }

    #[test]
    fn test_extract_json_block_fenced() {
        let input = "Here is the result:\n```json\n{\"entities\": []}\n```\nDone.";
        assert_eq!(extract_json_block(input), "{\"entities\": []}");
    }

    #[test]
    fn test_extract_json_block_braces() {
        let input = "Result: {\"entities\": []} end";
        assert_eq!(extract_json_block(input), "{\"entities\": []}");
    }

    #[test]
    fn test_parse_extraction_json_full() {
        let val = serde_json::json!({
            "entities": [
                {"name": "Alice", "type": "Person"},
                {"name": "Acme", "type": "Organization"}
            ],
            "relations": [
                {"from": "Alice", "relation": "works_at", "to": "Acme"}
            ]
        });

        let result = parse_extraction_json(&val);
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.entities[0].name, "Alice");
        assert_eq!(result.entities[0].entity_type, EntityType::Person);
        assert_eq!(result.entities[1].name, "Acme");
        assert_eq!(result.entities[1].entity_type, EntityType::Organization);
        assert_eq!(result.relations.len(), 1);
        assert_eq!(result.relations[0].0, "Alice");
        assert_eq!(result.relations[0].1, "works_at");
        assert_eq!(result.relations[0].2, "Acme");
        assert!((result.relations[0].3 - 0.5).abs() < f32::EPSILON); // default importance
    }

    #[test]
    fn test_parse_extraction_json_empty() {
        let val = serde_json::json!({});
        let result = parse_extraction_json(&val);
        assert!(result.entities.is_empty());
        assert!(result.relations.is_empty());
    }

    #[test]
    fn test_parse_extraction_json_missing_fields() {
        let val = serde_json::json!({
            "entities": [
                {"name": "Bob"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].entity_type, EntityType::Concept); // default
    }

    #[test]
    fn test_filters_entities_with_underscores() {
        let val = serde_json::json!({
            "entities": [
                {"name": "memory_recall", "type": "Concept"},
                {"name": "shell_execute", "type": "Concept"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert!(
            result.entities.is_empty(),
            "Tool-like names with underscores should be filtered"
        );
    }

    #[test]
    fn test_filters_entities_with_parentheses() {
        let val = serde_json::json!({
            "entities": [
                {"name": "foo(bar)", "type": "Concept"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert!(
            result.entities.is_empty(),
            "Names with parentheses should be filtered"
        );
    }

    #[test]
    fn test_filters_short_entity_names() {
        let val = serde_json::json!({
            "entities": [
                {"name": "X", "type": "Person"},
                {"name": "", "type": "Person"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert!(
            result.entities.is_empty(),
            "Names shorter than 2 chars should be filtered"
        );
    }

    #[test]
    fn test_drops_assistant_and_system_but_keeps_user() {
        let val = serde_json::json!({
            "entities": [
                {"name": "user", "type": "Person"},
                {"name": "Assistant", "type": "Person"},
                {"name": "SYSTEM", "type": "Concept"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert_eq!(
            result.entities.len(),
            1,
            "assistant + system drop, user stays as subject node"
        );
        assert_eq!(result.entities[0].name, "user");
        assert_eq!(result.entities[0].entity_type, EntityType::Person);
    }

    #[test]
    fn test_normalises_user_aliases_to_canonical_user() {
        let val = serde_json::json!({
            "entities": [
                {"name": "User", "type": "Person"},
                {"name": "the user", "type": "Person"},
                {"name": "You", "type": "Person"}
            ]
        });
        let result = parse_extraction_json(&val);
        assert_eq!(result.entities.len(), 3);
        for ent in &result.entities {
            assert_eq!(ent.name, "user", "all three aliases collapse to 'user'");
            assert_eq!(ent.entity_type, EntityType::Person);
        }
    }

    #[test]
    fn test_normalises_user_aliases_in_relations() {
        let val = serde_json::json!({
            "relations": [
                {"from": "The User", "relation": "is_getting", "to": "puppy"},
                {"from": "you", "relation": "owns", "to": "car"},
                {"from": "Assistant", "relation": "says", "to": "hello"}
            ]
        });
        let result = parse_extraction_json(&val);
        // Two user-aliased relations survive, normalised; the assistant one drops.
        assert_eq!(result.relations.len(), 2);
        assert_eq!(result.relations[0].0, "user");
        assert_eq!(result.relations[0].2, "puppy");
        assert_eq!(result.relations[1].0, "user");
        assert_eq!(result.relations[1].2, "car");
    }

    #[test]
    fn test_classify_name_does_not_collapse_substring_matches() {
        // Substring "user" inside another word must NOT be normalised.
        assert_eq!(
            classify_name("power user"),
            NameDecision::Keep("power user".to_string())
        );
        assert_eq!(
            classify_name("user_id"),
            NameDecision::Keep("user_id".to_string())
        );
    }

    #[test]
    fn test_keeps_valid_entities() {
        let val = serde_json::json!({
            "entities": [
                {"name": "Nadia", "type": "Person"},
                {"name": "Acme Corp", "type": "Organization"},
                {"name": "memory_recall", "type": "Concept"},
                {"name": "user", "type": "Person"},
                {"name": "A", "type": "Person"}
            ]
        });
        let result = parse_extraction_json(&val);
        // Nadia, Acme Corp, user — memory_recall (underscore) and "A" (<2 chars) drop.
        assert_eq!(result.entities.len(), 3);
        assert_eq!(result.entities[0].name, "Nadia");
        assert_eq!(result.entities[1].name, "Acme Corp");
        assert_eq!(result.entities[2].name, "user");
    }
}
