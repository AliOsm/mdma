//! Sampling from host-side logits and token history.
use rand::RngExt;

#[derive(Clone, Debug)]
pub struct SamplerConfig {
    pub temperature: f32,
    pub top_p: f32,
    pub min_p: f32,
    pub repetition_penalty: f32,
    pub repetition_window: usize,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        Self { temperature: 0.8, top_p: 0.95, min_p: 0.05, repetition_penalty: 1.1, repetition_window: 64 }
    }
}

pub struct Sampler {
    config: SamplerConfig,
}

impl Sampler {
    pub fn new(config: SamplerConfig) -> Self {
        Self { config }
    }

    pub fn sample(&self, logits: &[f32], history: &[u32], rng: &mut impl RngExt) -> u32 {
        let mut adjusted = logits.to_vec();
        apply_repetition_penalty(&mut adjusted, history, self.config.repetition_penalty, self.config.repetition_window);

        if self.config.temperature <= 0.0 {
            return argmax(&adjusted);
        }
        for l in &mut adjusted {
            *l /= self.config.temperature;
        }

        let mut ranked: Vec<(u32, f32)> =
            softmax(&adjusted).into_iter().enumerate().map(|(i, p)| (i as u32, p)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

        apply_min_p(&mut ranked, self.config.min_p);
        apply_top_p(&mut ranked, self.config.top_p);

        sample_categorical(&ranked, rng)
    }
}

pub fn greedy(logits: &[f32]) -> u32 {
    argmax(logits)
}

fn argmax(logits: &[f32]) -> u32 {
    logits
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i as u32)
}

fn apply_repetition_penalty(logits: &mut [f32], history: &[u32], penalty: f32, window: usize) {
    if penalty == 1.0 {
        return;
    }
    let start = history.len().saturating_sub(window);
    for &id in &history[start..] {
        if let Some(l) = logits.get_mut(id as usize) {
            *l = if *l > 0.0 { *l / penalty } else { *l * penalty };
        }
    }
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|&l| (l - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

/// `ranked` must be sorted by probability, descending.
fn apply_min_p(ranked: &mut Vec<(u32, f32)>, min_p: f32) {
    if min_p <= 0.0 || ranked.is_empty() {
        return;
    }
    let threshold = ranked[0].1 * min_p;
    ranked.retain(|&(_, p)| p >= threshold);
}

/// `ranked` must be sorted by probability, descending.
fn apply_top_p(ranked: &mut Vec<(u32, f32)>, top_p: f32) {
    if top_p >= 1.0 || ranked.is_empty() {
        return;
    }
    let mut cum = 0.0;
    let mut cutoff = ranked.len();
    for (i, &(_, p)) in ranked.iter().enumerate() {
        cum += p;
        if cum >= top_p {
            cutoff = i + 1;
            break;
        }
    }
    ranked.truncate(cutoff.max(1));
}

fn sample_categorical(ranked: &[(u32, f32)], rng: &mut impl RngExt) -> u32 {
    let total: f32 = ranked.iter().map(|&(_, p)| p).sum();
    if total <= 0.0 {
        return ranked.first().map_or(0, |&(id, _)| id);
    }
    let target = rng.random::<f32>() * total;
    let mut cum = 0.0;
    for &(id, p) in ranked {
        cum += p;
        if cum >= target {
            return id;
        }
    }
    ranked.last().map_or(0, |&(id, _)| id)
}

#[cfg(test)]
mod tests;
