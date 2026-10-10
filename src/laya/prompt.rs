// Question normalization and the `[CLS] <type> question: ins [SEP]
// [MASK] opt0 [MASK] opt1 ... [SEP] state [SEP]` sequence.
use super::pyjson::dumps;
use super::tokenizer::Tokenizer;
use super::{Error, Result};
use serde_json::Value;
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Choice,
    Score,
    Noul,
}

impl Kind {
    pub fn index(self) -> u32 {
        match self {
            Self::Choice => 0,
            Self::Score => 1,
            Self::Noul => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Score => "score",
            Self::Noul => "noul",
        }
    }
}

/// A validated question: rendered option texts plus what the answer
/// reports back - choice labels, or the score legend as the raw criteria.
#[derive(Debug, Clone)]
pub struct Question {
    pub kind: Kind,
    pub instructions: String,
    pub options: Vec<String>,
    pub labels: Vec<String>,
    pub legend: Vec<Value>,
}

fn render_criterion(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(s) => Cow::Borrowed(s),
        other => Cow::Owned(dumps(other, false)),
    }
}

fn blank(value: &Value) -> bool {
    matches!(value, Value::Null) || matches!(value, Value::String(s) if s.is_empty())
}

impl Question {
    /// Validate `{"type", "instructions", "criteria"}` and render its options.
    pub fn from_json(def: &Value) -> Result<Self> {
        let Value::Object(def) = def else {
            return Err(Error::Question("each question must be a dictionary".into()));
        };
        let kind = match def.get("type").and_then(Value::as_str) {
            Some("choice") => Kind::Choice,
            Some("score") => Kind::Score,
            Some("noul") => Kind::Noul,
            other => return Err(Error::Question(format!("unknown question type {other:?}; expected choice, score, or noul"))),
        };
        let instructions = match def.get("instructions") {
            None => return Err(Error::Question("question is missing instructions".into())),
            Some(Value::String(s)) => s.clone(),
            Some(other) => dumps(other, true),
        };
        let criteria = def.get("criteria").unwrap_or(&Value::Null);
        let mut legend = Vec::new();
        let (options, labels) = match kind {
            Kind::Choice => {
                let entries: Vec<(&str, &Value)> = match criteria {
                    Value::Array(items) => {
                        let labels: Vec<&str> = items.iter().map(|c| c.as_str().ok_or_else(|| Error::Question("choice labels must be strings".into()))).collect::<Result<_>>()?;
                        let mut seen = std::collections::HashSet::new();
                        if !labels.iter().all(|l| seen.insert(*l)) {
                            return Err(Error::Question("choice labels must be unique".into()));
                        }
                        labels.into_iter().map(|l| (l, &Value::Null)).collect()
                    }
                    Value::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
                    _ => return Err(Error::Question("choice criteria must be a nonempty dictionary or list".into())),
                };
                if entries.is_empty() {
                    return Err(Error::Question("choice criteria must be a nonempty dictionary or list".into()));
                }
                let options = entries
                    .iter()
                    .map(|&(k, v)| if blank(v) { k.to_owned() } else { format!("{k}: {}", render_criterion(v)) })
                    .collect();
                (options, entries.into_iter().map(|(k, _)| k.to_owned()).collect())
            }
            Kind::Score => {
                let Value::Array(items) = criteria else {
                    return Err(Error::Question("score criteria must be a nonempty list".into()));
                };
                if items.is_empty() {
                    return Err(Error::Question("score criteria must be a nonempty list".into()));
                }
                let options = items.iter().enumerate().map(|(i, c)| format!("level {i}: {}", render_criterion(c))).collect();
                legend.clone_from(items);
                (options, Vec::new())
            }
            Kind::Noul => {
                let map = match criteria {
                    Value::Null => None,
                    Value::Object(map) => Some(map),
                    _ => return Err(Error::Question("noul criteria must be a dictionary with false/true descriptions".into())),
                };
                let desc = |key: &str, default: &'static str| match map.and_then(|m| m.get(key)) {
                    Some(v) if !blank(v) => render_criterion(v),
                    _ => Cow::Borrowed(default),
                };
                let options = vec![
                    format!("false: {}", desc("false", "no, the statement does not hold")),
                    format!("true: {}", desc("true", "yes, the statement holds")),
                ];
                (options, Vec::new())
            }
        };
        Ok(Self { kind, instructions, options, labels, legend })
    }
}

pub fn serialize_state(state: &Value) -> String {
    render_criterion(state).into_owned()
}

/// Token ids and the position of each option's `[MASK]` marker.
pub struct Sequence {
    pub ids: Vec<u32>,
    pub markers: Vec<usize>,
}

fn build_prefix(tok: &Tokenizer, q: &Question, head_max_len: usize) -> Result<Sequence> {
    let mask = tok.mask_token.as_str();
    let ins = q.instructions.replace(mask, " ");
    let mut head_ids = tok.encode(&format!("{} question: {ins}", q.kind.name()))?;
    let mut opt_ids = Vec::with_capacity(q.options.len());
    for opt in &q.options {
        let mut ids = vec![tok.mask_id];
        let text = tok.encode(&format!(" {}", opt.replace(mask, " ")))?;
        ids.extend(text.into_iter().take(48));
        opt_ids.push(ids);
    }
    let total = |opts: &[Vec<u32>]| opts.iter().map(Vec::len).sum::<usize>();
    let mut opt_budget = head_max_len.cast_signed() - total(&opt_ids).cast_signed();
    if opt_budget < 16 {
        let per = (head_max_len.saturating_sub(16) / opt_ids.len().max(1)).max(4);
        for o in &mut opt_ids {
            o.truncate(per);
        }
        opt_budget = head_max_len.cast_signed() - total(&opt_ids).cast_signed();
    }
    head_ids.truncate(opt_budget.max(8).cast_unsigned());
    let mut ids = vec![tok.cls_id];
    ids.extend(head_ids);
    ids.push(tok.sep_id);
    let mut markers = Vec::with_capacity(opt_ids.len());
    for o in opt_ids {
        markers.push(ids.len());
        ids.extend(o);
    }
    ids.push(tok.sep_id);
    Ok(Sequence { ids, markers })
}

pub fn build_sequence(tok: &Tokenizer, state: &Value, q: &Question, max_len: usize, head_max_len: usize) -> Result<Sequence> {
    let Sequence { mut ids, markers } = build_prefix(tok, q, head_max_len)?;
    let room = max_len.saturating_sub(ids.len() + 1);
    let state_ids = tok.encode(&render_criterion(state).replace(tok.mask_token.as_str(), " "))?;
    ids.extend(state_ids.into_iter().take(room));
    ids.push(tok.sep_id);
    ids.truncate(max_len);
    let markers = markers.into_iter().filter(|&m| m < max_len).collect();
    Ok(Sequence { ids, markers })
}

/// One prepared question of a batch.
pub struct Item {
    pub ids: Vec<u32>,
    pub markers: Vec<usize>,
    pub kind: Kind,
}

/// Right-padded batch: `ids` is `[n, seq_len]`, `markers` is `[n, kmax]`
/// with unused slots at 0 (upstream clamps them the same way).
pub struct Batch {
    pub n: usize,
    pub seq_len: usize,
    pub kmax: usize,
    pub ids: Vec<u32>,
    pub lens: Vec<u32>,
    pub markers: Vec<usize>,
    pub counts: Vec<usize>,
    pub kinds: Vec<Kind>,
}

pub fn collate(items: &[Item], pad_id: u32) -> Result<Batch> {
    collate_iter(items.iter(), pad_id)
}

/// Copy borrowed items directly into the padded batch.
pub(super) fn collate_iter<'a>(items: impl ExactSizeIterator<Item = &'a Item> + Clone, pad_id: u32) -> Result<Batch> {
    let n = items.len();
    if n == 0 {
        return Err(Error::Question("cannot collate an empty batch".into()));
    }
    let (seq_len, kmax) = items.clone().fold((0, 2), |(seq, k), it| {
        (seq.max(it.ids.len()), k.max(it.markers.len()))
    });
    let mut batch = Batch {
        n,
        seq_len,
        kmax,
        ids: vec![pad_id; n * seq_len],
        lens: Vec::with_capacity(n),
        markers: vec![0; n * kmax],
        counts: Vec::with_capacity(n),
        kinds: Vec::with_capacity(n),
    };
    for (i, it) in items.enumerate() {
        batch.ids[i * seq_len..i * seq_len + it.ids.len()].copy_from_slice(&it.ids);
        batch.markers[i * kmax..i * kmax + it.markers.len()].copy_from_slice(&it.markers);
        batch.lens.push(it.ids.len() as u32);
        batch.counts.push(it.markers.len());
        batch.kinds.push(it.kind);
    }
    Ok(batch)
}
