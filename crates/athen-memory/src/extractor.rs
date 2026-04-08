//! LLM-based entity extraction for the knowledge graph.

use async_trait::async_trait;
use tracing::{debug, warn};

use athen_core::error::Result;
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::memory::{Entity, EntityType, ExtractionResult, EntityExtractor};

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
{"entities": [{"name": "...", "type": "Person|Organization|Project|Event|Document|Concept"}], "relations": [{"from": "entity_name", "relation": "verb/relationship", "to": "entity_name"}]}
Only include clearly stated entities. Be concise."#;

#[async_trait]
impl EntityExtractor for LlmEntityExtractor {
    async fn extract(&self, text: &str) -> Result<ExtractionResult> {
        let request = LlmRequest {
            profile: ModelProfile::Cheap,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "{EXTRACTION_PROMPT}\n\nText:\n{text}"
                )),
            }],
            max_tokens: Some(500),
            temperature: Some(0.0),
            tools: None,
            system_prompt: None,
        };

        // 5-second timeout on the LLM call.
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
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

fn parse_extraction_json(val: &serde_json::Value) -> ExtractionResult {
    let entities = val
        .get("entities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let name = e.get("name")?.as_str()?.to_string();
                    let entity_type = parse_entity_type(
                        e.get("type").and_then(|v| v.as_str()).unwrap_or("Concept"),
                    );
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
                    let from = r.get("from")?.as_str()?.to_string();
                    let relation = r.get("relation")?.as_str()?.to_string();
                    let to = r.get("to")?.as_str()?.to_string();
                    Some((from, relation, to))
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
        assert_eq!(result.relations[0], ("Alice".into(), "works_at".into(), "Acme".into()));
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
}
