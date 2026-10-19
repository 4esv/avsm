//! The research role: a question goes to the configured command backend
//! as JSON, and what comes back becomes inferred records, each with its
//! source and how sure it is. Nothing here reaches the world itself: the
//! command is the provider, and the warden runs it. Everything that is not
//! a field of the record schema is stripped on the way in, and the size is
//! bounded.

use serde::Deserialize;

use crate::record::{Claim, Record, Seq};

/// The most results one question may yield.
pub const MAX_RESULTS: usize = 20;
/// The most characters a result's text may hold.
pub const MAX_TEXT: usize = 2000;
/// The most characters a source may hold.
pub const MAX_SOURCE: usize = 500;
/// The most characters a question may hold.
pub const MAX_QUESTION: usize = 2000;

/// What the command reads on its standard input.
pub fn request(question: &str, venture: &str) -> String {
    serde_json::json!({
        "question": clip(question.trim(), MAX_QUESTION),
        "venture": venture,
        "limit": MAX_RESULTS,
    })
    .to_string()
}

/// One result as the command may print it. Anything else in it is
/// ignored, and a field of the wrong type is dropped rather than refused.
#[derive(Deserialize)]
struct Found {
    #[serde(default)]
    text: serde_json::Value,
    #[serde(default)]
    source: serde_json::Value,
    #[serde(default)]
    confidence: serde_json::Value,
    #[serde(default)]
    value: serde_json::Value,
    #[serde(default)]
    topic: serde_json::Value,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    results: Vec<Found>,
}

/// A result, validated and bounded.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub topic: String,
    pub text: String,
    pub source: Option<String>,
    pub confidence: Option<f64>,
    pub value: Option<f64>,
}

/// Read the command's reply. Results beyond the cap are dropped, text and
/// sources are cut to size, confidence is held to 0..1, a value that is
/// not a finite number is dropped, and a topic that is not one word
/// becomes `research`.
pub fn parse(reply: &str) -> Result<Vec<Finding>, String> {
    let text = reply.trim();
    let json = text
        .find('{')
        .and_then(|s| text.rfind('}').map(|e| &text[s..=e]))
        .ok_or("the research command did not print a JSON object")?;
    let reply: Reply = serde_json::from_str(json)
        .map_err(|e| format!("the research reply is not as expected: {e}"))?;
    Ok(reply
        .results
        .into_iter()
        .filter_map(|f| {
            let text = f.text.as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            Some(Finding {
                topic: match f.topic.as_str().map(str::trim) {
                    Some(t) if is_word(t) => t.to_string(),
                    _ => "research".to_string(),
                },
                text: clip(text, MAX_TEXT),
                source: f
                    .source
                    .as_str()
                    .map(|s| clip(s.trim(), MAX_SOURCE))
                    .filter(|s| !s.is_empty()),
                confidence: f
                    .confidence
                    .as_f64()
                    .filter(|c| c.is_finite())
                    .map(|c| c.clamp(0.0, 1.0)),
                value: f.value.as_f64().filter(|v| v.is_finite()),
            })
        })
        .take(MAX_RESULTS)
        .collect())
}

/// The inferred records for a venture's findings, each citing the action
/// that asked.
pub fn records(venture: &str, findings: &[Finding], action: Seq) -> Vec<Record> {
    findings
        .iter()
        .map(|f| Record::Report {
            claim: Claim::Inferred,
            venture: venture.to_string(),
            metric: f.topic.clone(),
            value: f.value,
            note: f.text.clone(),
            source: f.source.clone(),
            confidence: f.confidence,
            cites: vec![action],
            implausible: None,
            facts: None,
        })
        .collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_one_json_line_with_the_question() {
        let r = request("  what sells?  ", "shop");
        assert_eq!(
            r,
            r#"{"limit":20,"question":"what sells?","venture":"shop"}"#
        );
        let long = "x".repeat(MAX_QUESTION + 5);
        assert!(request(&long, "shop").len() < MAX_QUESTION + 80);
    }

    #[test]
    fn replies_are_validated_bounded_and_stripped() {
        let reply = r#"Here you go: {"results": [
            {"text": "Rivals charge 9", "source": "https://x", "confidence": 1.7, "value": 9, "topic": "pricing", "secret": "dropped"},
            {"text": "   ", "source": "nothing"},
            {"text": "No source", "confidence": -2, "topic": "two words", "value": "NaN"}
        ], "extra": true}"#;
        let f = parse(reply).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(
            f[0],
            Finding {
                topic: "pricing".into(),
                text: "Rivals charge 9".into(),
                source: Some("https://x".into()),
                confidence: Some(1.0),
                value: Some(9.0),
            }
        );
        assert_eq!(f[1].topic, "research");
        assert_eq!(f[1].confidence, Some(0.0));
        assert_eq!(f[1].source, None);
        assert_eq!(f[1].value, None);
        let records = records("shop", &f, 7);
        assert!(matches!(
            &records[0],
            Record::Report { claim: Claim::Inferred, metric, source: Some(s), confidence: Some(c), cites, .. }
            if metric == "pricing" && s == "https://x" && *c == 1.0 && *cites == vec![7]
        ));
        assert_eq!(records[0].canonical().matches("secret").count(), 0);

        let many: Vec<String> = (0..30).map(|i| format!(r#"{{"text": "r{i}"}}"#)).collect();
        let f = parse(&format!(r#"{{"results": [{}]}}"#, many.join(","))).unwrap();
        assert_eq!(f.len(), MAX_RESULTS);
        let big = format!(
            r#"{{"results": [{{"text": "{}", "source": "{}"}}]}}"#,
            "t".repeat(3000),
            "s".repeat(600)
        );
        let f = parse(&big).unwrap();
        assert_eq!(f[0].text.chars().count(), MAX_TEXT + 1);
        assert_eq!(
            f[0].source.as_ref().unwrap().chars().count(),
            MAX_SOURCE + 1
        );

        assert!(parse("no json here")
            .unwrap_err()
            .contains("did not print a JSON object"));
        assert!(parse(r#"{"results": "nope"}"#).is_err());
        assert!(parse(r#"{"other": 1}"#).unwrap().is_empty());
        assert!(
            parse(r#"{"results": [{"text": 5}, {"text": "ok", "source": 7}]}"#)
                .unwrap()
                .len()
                == 1
        );
    }
}
