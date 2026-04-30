wit_bindgen::generate!({
    path: "../../wit/evaluator.wit",
    world: "evaluator-world",
});

use exports::vigilo::evaluator::evaluator::{Guest, Input, Output};
use serde_json::json;
use vigilo::evaluator::executor;
use vigilo::evaluator::types::{
    EvaluationDimension, EvaluationStatus, EvaluatorFinding, EvaluatorIdentity, Score, Severity,
};

struct Evaluator;

const POSITIVE_TERMS: &[&str] = &[
    "good",
    "great",
    "excellent",
    "amazing",
    "love",
    "helpful",
    "fast",
    "happy",
    "success",
    "pleasant",
];

const NEGATIVE_TERMS: &[&str] = &[
    "bad", "terrible", "awful", "hate", "slow", "angry", "sad", "bug", "failure", "poor",
];

fn extract_text(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("context.db must not be empty".to_string());
    }

    // Accept either plain text input or a JSON object containing a text payload.
    if trimmed.starts_with('{') {
        let parsed: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|err| format!("invalid JSON in context.db: {err}"))?;

        let obj = parsed
            .as_object()
            .ok_or_else(|| "context.db JSON must be an object".to_string())?;

        if let Some(text) = obj.get("text").and_then(|value| value.as_str()) {
            let text = text.trim();
            if text.is_empty() {
                return Err("context.db.text must not be empty".to_string());
            }
            return Ok(text.to_string());
        }

        return Err("context.db JSON must contain a non-empty string field 'text'".to_string());
    }

    Ok(trimmed.to_string())
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn score_text(text: &str) -> (i32, usize, usize) {
    let mut score = 0;
    let mut positive_matches = 0;
    let mut negative_matches = 0;

    for token in tokenize(text) {
        if POSITIVE_TERMS.contains(&token.as_str()) {
            score += 1;
            positive_matches += 1;
        }

        if NEGATIVE_TERMS.contains(&token.as_str()) {
            score -= 1;
            negative_matches += 1;
        }
    }

    (score, positive_matches, negative_matches)
}

fn label_from_score(score: i32) -> &'static str {
    if score > 0 {
        "positive"
    } else if score < 0 {
        "negative"
    } else {
        "neutral"
    }
}

impl Guest for Evaluator {
    fn evaluate(input: Input) -> Result<Output, String> {
        executor::trace("sentiment evaluator started");
        executor::debug(&format!("db context: {}", input.context.db));

        let text = extract_text(&input.context.db)?;
        let (score, positive_matches, negative_matches) = score_text(&text);
        let label = label_from_score(score);

        let evidence_json = json!({
            "label": label,
            "score": score,
            "positive_matches": positive_matches,
            "negative_matches": negative_matches,
            "matched_terms": positive_matches + negative_matches,
            "text": text,
        })
        .to_string();

        let normalized = match label {
            "positive" => 1.0,
            "neutral" => 0.5,
            _ => 0.0,
        };

        let status = if label == "negative" {
            EvaluationStatus::Failed
        } else {
            EvaluationStatus::Passed
        };

        let severity = if label == "negative" {
            Severity::Medium
        } else {
            Severity::None
        };

        Ok(Output {
            evaluator: EvaluatorIdentity {
                namespace: "vigilo".to_string(),
                name: "sentiment".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                content_hash: None,
                interface_version: Some("0.1.0".to_string()),
            },
            results: vec![EvaluatorFinding {
                dimension: EvaluationDimension::Quality,
                status,
                score: Score::Normalized(normalized),
                blocking: false,
                severity,
                failure_category: None,
                reason: Some(format!("sentiment label is {label}")),
                evidence_json,
                tags: vec!["sentiment".to_string(), label.to_string()],
            }],
            metadata_json: json!({
                "source": "lexicon",
            })
            .to_string(),
        })
    }
}

export!(Evaluator);

#[cfg(test)]
mod tests {
    use super::{extract_text, label_from_score, score_text, tokenize};

    #[test]
    fn tokenize_normalizes_and_strips_symbols() {
        let tokens = tokenize("Great service, fast response!");
        assert_eq!(tokens, vec!["great", "service", "fast", "response"]);
    }

    #[test]
    fn score_text_detects_positive_sentiment() {
        let (score, positives, negatives) = score_text("Great and helpful support");
        assert_eq!((score, positives, negatives), (2, 2, 0));
        assert_eq!(label_from_score(score), "positive");
    }

    #[test]
    fn score_text_detects_negative_sentiment() {
        let (score, positives, negatives) = score_text("Awful, slow, bad experience");
        assert_eq!((score, positives, negatives), (-3, 0, 3));
        assert_eq!(label_from_score(score), "negative");
    }

    #[test]
    fn score_text_returns_neutral_for_mixed_or_unknown() {
        let (score, positives, negatives) = score_text("Great but slow");
        assert_eq!((score, positives, negatives), (0, 1, 1));
        assert_eq!(label_from_score(score), "neutral");
    }

    #[test]
    fn extract_text_accepts_plain_text() {
        let text = extract_text("  plain text input  ").unwrap();
        assert_eq!(text, "plain text input");
    }

    #[test]
    fn extract_text_accepts_json_payload() {
        let text = extract_text(r#"{"text":"Excellent response"}"#).unwrap();
        assert_eq!(text, "Excellent response");
    }

    #[test]
    fn extract_text_rejects_invalid_or_empty_input() {
        assert!(extract_text("   ").is_err());
        assert!(extract_text(r#"{"message":"missing text"}"#).is_err());
    }
}
