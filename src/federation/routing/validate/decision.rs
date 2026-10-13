use super::*;
use crate::decision::{Contract, minicheck};
use crate::laya::prompt::{Kind, Question};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use std::{collections::BTreeMap, marker::PhantomData};

/// Reject repeated question IDs/probability labels/legend keys. Ordinary
/// serde maps accept duplicate keys; forwarding their original JSON would
/// expose ambiguous results to clients with a different duplicate policy.
fn unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Unique<V>(PhantomData<V>);
    impl<'de, V: Deserialize<'de>> Visitor<'de> for Unique<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unique object keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut output = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if output.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate key"));
                }
            }
            Ok(output)
        }
    }
    deserializer.deserialize_map(Unique(PhantomData))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultBody {
    model: String,
    #[serde(deserialize_with = "unique_map")]
    answers: BTreeMap<String, Answer>,
    usage: Usage,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    act_probability: f64,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Answer {
    Choice {
        choice: String,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
        action: Action,
    },
    Score {
        score: f64,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        #[serde(deserialize_with = "unique_map")]
        legend: BTreeMap<String, serde_json::Value>,
        confidence: f64,
        action: Action,
    },
    Noul {
        noul: f64,
        confidence: f64,
        action: Action,
    },
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0. ..=1.).contains(&value)
}
fn distribution(values: &BTreeMap<String, f64>) -> bool {
    !values.is_empty()
        && values.values().all(|v| probability(*v))
        && (values.values().sum::<f64>() - 1.).abs() <= values.len() as f64 * 0.000051 + 1e-6
}
fn common(confidence: f64, action: &Action) -> bool {
    probability(confidence) && probability(action.act_probability)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckBody {
    model: String,
    #[serde(deserialize_with = "unique_map")]
    answers: BTreeMap<String, Check>,
    usage: Usage,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    supported: bool,
    probability: f64,
}

/// The answers a request asks for: Laya's typed questions, or MiniCheck's
/// claim ids.
enum Expected {
    Laya(BTreeMap<String, Question>),
    MiniCheck(std::collections::BTreeSet<String>),
}

/// MiniCheck's answers: exactly the claims, each a probability that is
/// `supported` above 0.5, and nothing generated.
fn checked(claims: &std::collections::BTreeSet<String>, result_json: &str) -> bool {
    let Ok(body) = serde_json::from_str::<CheckBody>(result_json) else {
        return false;
    };
    body.model == minicheck::NAME
        && body.usage.output_tokens == 0
        && body.usage.input_tokens <= claims.len() as u64 * u64::from(u32::MAX)
        && body.answers.keys().eq(claims.iter())
        && body.answers.values().all(|check| {
            probability(check.probability) && check.supported == (check.probability > 0.5)
        })
}

pub(in crate::federation::routing) struct Decision {
    envelope: ExecutionEnvelope,
    max_result_bytes: usize,
    expected: Expected,
}
impl Decision {
    pub(in crate::federation::routing) fn new(
        request: &DecideRequest,
        descriptor: &ModelDescriptor,
    ) -> Result<Self, Status> {
        let envelope = selected(request.envelope.as_ref(), descriptor)?;
        let Limits::Decide {
            max_questions,
            max_result_bytes,
        } = descriptor.limits
        else {
            return Err(failure());
        };
        if request
            .state_json
            .len()
            .saturating_add(request.questions_json.len())
            > descriptor.max_input_bytes as usize
        {
            return Err(Status::invalid_argument("invalid_request"));
        }
        if descriptor.decision == Some(Contract::MiniCheck) {
            let request = minicheck::Request::parse(&request.state_json, &request.questions_json)
                .map_err(|_| Status::invalid_argument("invalid_request"))?;
            if request.claims.len() > max_questions as usize {
                return Err(Status::invalid_argument("invalid_request"));
            }
            return Ok(Self {
                envelope,
                max_result_bytes: max_result_bytes as usize,
                expected: Expected::MiniCheck(
                    request.claims.into_iter().map(|(id, _)| id).collect(),
                ),
            });
        }
        serde_json::from_str::<serde_json::Value>(&request.state_json)
            .map_err(|_| Status::invalid_argument("invalid_request"))?;
        let questions = serde_json::from_str::<serde_json::Value>(&request.questions_json)
            .map_err(|_| Status::invalid_argument("invalid_request"))?;
        let questions = questions
            .as_object()
            .filter(|q| q.len() <= max_questions as usize)
            .ok_or_else(|| Status::invalid_argument("invalid_request"))?;
        let expected = questions
            .iter()
            .map(|(id, value)| {
                Question::from_json(value)
                    .map(|q| (id.clone(), q))
                    .map_err(|_| Status::invalid_argument("invalid_request"))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            envelope,
            max_result_bytes: max_result_bytes as usize,
            expected: Expected::Laya(expected),
        })
    }

    /// Rama's unary client returns a response only after its final gRPC status.
    #[expect(
        clippy::needless_collect,
        reason = "the label set sorts and deduplicates before comparing with sorted map keys"
    )]
    pub(in crate::federation::routing) fn response(
        &self,
        wire: &DecideResponse,
    ) -> Result<(), Status> {
        if !identity(
            &self.envelope,
            &wire.request_id,
            &wire.executor,
            &wire.model,
            &wire.artifact_id,
            &wire.profile_id,
        ) || wire.result_json.len() > self.max_result_bytes
        {
            return Err(failure());
        }
        let expected = match &self.expected {
            Expected::Laya(expected) => expected,
            Expected::MiniCheck(claims) => {
                return checked(claims, &wire.result_json)
                    .then_some(())
                    .ok_or_else(failure);
            }
        };
        let body: ResultBody = serde_json::from_str(&wire.result_json).map_err(|_| failure())?;
        if body.model != "laya-rl-agent"
            || body.usage.output_tokens != 0
            || body.usage.input_tokens > expected.len() as u64 * u64::from(u32::MAX)
            || !body.answers.keys().eq(expected.keys())
        {
            return Err(failure());
        }
        for (id, question) in expected {
            let answer = &body.answers[id];
            let valid = match (question.kind, answer) {
                (
                    Kind::Choice,
                    Answer::Choice {
                        choice,
                        probabilities,
                        confidence,
                        action,
                    },
                ) => {
                    common(*confidence, action)
                        && distribution(probabilities)
                        && probabilities.keys().eq(question
                            .labels
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>())
                        && question.labels.contains(choice)
                }
                (
                    Kind::Score,
                    Answer::Score {
                        score,
                        probabilities,
                        legend,
                        confidence,
                        action,
                    },
                ) => {
                    let expected: BTreeMap<_, _> = question
                        .legend
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v))
                        .collect();
                    common(*confidence, action)
                        && distribution(probabilities)
                        && score.is_finite()
                        && *score >= 0.
                        && *score <= question.legend.len().saturating_sub(1) as f64
                        && legend.len() == expected.len()
                        && legend
                            .iter()
                            .all(|(k, v)| expected.get(k).is_some_and(|e| *e == v))
                        && probabilities.keys().eq(expected.keys())
                }
                (
                    Kind::Noul,
                    Answer::Noul {
                        noul,
                        confidence,
                        action,
                    },
                ) => common(*confidence, action) && probability(*noul),
                _ => false,
            };
            if !valid {
                return Err(failure());
            }
        }
        Ok(())
    }
}
