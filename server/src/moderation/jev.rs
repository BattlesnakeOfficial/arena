//! Multi-question client for `TypeSafe`'s System One API (Jev).
//!
//! One endpoint: `POST /v1/systemone` with a `state` value and a map of
//! typed questions (`noul` / `choice` / `score`); answers come back under
//! the same keys. Wire shapes are ported from mull's `typesafe.rs` client —
//! shapes only, no cross-repo dependency. Callers fail open on any error.

use std::collections::BTreeMap;

use color_eyre::eyre::{Context as _, eyre};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// Cap on the response body read. `response.json()`/`.text()` buffer the
/// whole body unbounded; the endpoint is env-configurable
/// (`MODERATION_JEV_URL`), so a hostile or misconfigured endpoint must not
/// be able to OOM the process even under a short timeout.
const BODY_READ_CAP_BYTES: usize = 64 * 1024;

/// One answer, discriminated by the `type` the question was asked with.
///
/// Deserialized through [`RawAnswer`] rather than serde's internally tagged
/// representation: if any crate in the build turns on
/// `serde_json/arbitrary_precision` (feature unification applies
/// build-wide), a tagged enum buffers numbers as maps and every `f64` field
/// fails with "invalid type: map, expected f64". A flat struct plus
/// `TryFrom` is immune.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", try_from = "RawAnswer")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

/// Flat wire shape of an answer; a plain struct sidesteps the tagged-enum
/// number buffering described on [`Answer`].
#[derive(Deserialize)]
struct RawAnswer {
    #[serde(rename = "type")]
    kind: String,
    noul: Option<f64>,
    choice: Option<String>,
    score: Option<f64>,
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
    #[serde(default)]
    legend: BTreeMap<String, String>,
}

/// Probabilities, noul values, and confidence are all in `[0, 1]`; scores
/// are ordered rubric values and may exceed it.
fn unit_interval(kind: &str, field: &str, value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{kind} answer has {field} {value} outside [0, 1]"))
    }
}

/// Validate (in place) that every probability mass is finite and in
/// `[0, 1]`.
fn validate_probabilities(
    kind: &str,
    probabilities: &mut BTreeMap<String, f64>,
) -> Result<(), String> {
    for mass in probabilities.values_mut() {
        *mass = unit_interval(kind, "probabilities", *mass)?;
    }
    Ok(())
}

impl TryFrom<RawAnswer> for Answer {
    type Error = String;

    fn try_from(raw: RawAnswer) -> Result<Self, Self::Error> {
        let missing = |field: &str| format!("{} answer is missing `{field}`", raw.kind);
        match raw.kind.as_str() {
            "noul" => Ok(Self::Noul {
                noul: unit_interval("noul", "noul", raw.noul.ok_or_else(|| missing("noul"))?)?,
            }),
            "choice" => {
                let mut probabilities = raw.probabilities;
                validate_probabilities("choice", &mut probabilities)?;
                Ok(Self::Choice {
                    choice: raw.choice.ok_or_else(|| missing("choice"))?,
                    confidence: unit_interval(
                        "choice",
                        "confidence",
                        raw.confidence.ok_or_else(|| missing("confidence"))?,
                    )?,
                    probabilities,
                })
            }
            "score" => {
                let score = raw.score.ok_or_else(|| missing("score")).and_then(|s| {
                    s.is_finite()
                        .then_some(s)
                        .ok_or_else(|| format!("score answer has non-finite score {s}"))
                })?;
                let mut probabilities = raw.probabilities;
                validate_probabilities("score", &mut probabilities)?;
                Ok(Self::Score {
                    score,
                    confidence: unit_interval(
                        "score",
                        "confidence",
                        raw.confidence.ok_or_else(|| missing("confidence"))?,
                    )?,
                    legend: raw.legend,
                    probabilities,
                })
            }
            other => Err(format!("unknown answer type {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemOneResponse {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

impl SystemOneResponse {
    /// Parse a raw response body.
    ///
    /// # Errors
    ///
    /// Returns an error when the body is not the documented JSON shape.
    pub fn parse(text: &str) -> cja::Result<Self> {
        serde_json::from_str(text).map_err(|e| eyre!("typesafe response parse error: {e}"))
    }
}

/// POST `{model, state, questions}` to the endpoint with the bearer key
/// already installed in the client's default headers.
///
/// Errors on transport failure, non-2xx (warn!-logged with status and a
/// truncated response body — Jev's own error text, never the submitted
/// text), or an unparseable body. Callers fail open.
pub async fn system_one(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    state: &serde_json::Value,
    questions: &serde_json::Value,
) -> cja::Result<SystemOneResponse> {
    let body = serde_json::json!({
        "model": model,
        "state": state,
        "questions": questions,
    });
    let response = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .wrap_err("typesafe request failed")?;
    let status = response.status();
    // Capped read: never buffer an unbounded body from an
    // env-configurable endpoint.
    let text = crate::snake_client::read_body_capped(response, BODY_READ_CAP_BYTES)
        .await
        .wrap_err("typesafe response read failed")?;
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        tracing::warn!("typesafe returned HTTP {status}: {snippet}");
        return Err(eyre!("typesafe returned HTTP {status}"));
    }
    SystemOneResponse::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"model":"jev-1.13.0","answers":{
        "is_urgent":{"type":"noul","noul":0.95},
        "department":{"type":"choice","choice":"billing","confidence":0.83,"probabilities":{"sales":0.0,"billing":0.89,"technical":0.11}},
        "frustration":{"type":"score","score":1.04,"confidence":0.94,"legend":{"0":"Calm","1":"Frustrated","2":"Very angry"},"probabilities":{"0":0.0,"1":0.96,"2":0.04}}},
        "usage":{"input_tokens":402,"output_tokens":73}}"#;

    #[test]
    fn parses_all_three_answer_types() {
        let r = SystemOneResponse::parse(SAMPLE).unwrap();
        assert_eq!(r.model, "jev-1.13.0");
        assert_eq!(r.answers["is_urgent"], Answer::Noul { noul: 0.95 });
        let Answer::Choice {
            choice,
            probabilities,
            confidence,
        } = &r.answers["department"]
        else {
            panic!("not a choice");
        };
        assert_eq!(choice, "billing");
        assert_eq!(probabilities["billing"], 0.89);
        assert_eq!(*confidence, 0.83);
        let Answer::Score { score, .. } = r.answers["frustration"] else {
            panic!("not a score");
        };
        assert_eq!(score, 1.04);
        assert_eq!(r.usage.input_tokens, 402);
    }

    #[test]
    fn out_of_range_float_is_rejected() {
        let body = r#"{"model":"m","answers":{"a":{"type":"noul","noul":1.4}}}"#;
        let err = SystemOneResponse::parse(body).unwrap_err().to_string();
        assert!(err.contains("outside [0, 1]"), "{err}");

        let body = r#"{"model":"m","answers":{"a":{"type":"noul","noul":null}}}"#;
        assert!(SystemOneResponse::parse(body).is_err());
    }

    #[test]
    fn unknown_type_string_is_rejected() {
        let body = r#"{"model":"m","answers":{"a":{"type":"mystery","noul":0.5}}}"#;
        let err = SystemOneResponse::parse(body).unwrap_err().to_string();
        assert!(err.contains("unknown answer type"), "{err}");
    }

    #[test]
    fn missing_required_field_is_rejected() {
        // A choice answer missing `confidence`.
        let body = r#"{"model":"m","answers":{"a":{"type":"choice","choice":"allow","probabilities":{"allow":1.0}}}}"#;
        let err = SystemOneResponse::parse(body).unwrap_err().to_string();
        assert!(err.contains("missing"), "{err}");
        // Empty / non-JSON bodies.
        assert!(SystemOneResponse::parse("{}").is_err());
        assert!(SystemOneResponse::parse("not json").is_err());
    }

    #[test]
    fn non_finite_probability_is_rejected() {
        let body = r#"{"model":"m","answers":{"a":{"type":"choice","choice":"allow","confidence":1.0,"probabilities":{"allow":Infinity}}}}"#;
        assert!(SystemOneResponse::parse(body).is_err());
    }
}
