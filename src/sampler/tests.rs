use super::{greedy, Sampler, SamplerConfig};
use rand::rngs::StdRng;
use rand::SeedableRng;

#[test]
fn greedy_picks_max_logit() {
    let logits = [0.1, 5.0, -3.0, 2.0];
    assert_eq!(greedy(&logits), 1);
}

#[test]
fn repetition_penalty_can_flip_the_greedy_choice() {
    let sampler = Sampler::new(SamplerConfig {
        temperature: 0.0,
        top_p: 1.0,
        min_p: 0.0,
        repetition_penalty: 5.0,
        repetition_window: 8,
    });
    let logits = [5.0, 4.9];
    let mut rng = StdRng::seed_from_u64(1);
    // token 0 dominates but was just emitted; the penalty should push
    // its effective logit (5.0 / 5.0 = 1.0) below token 1's untouched 4.9.
    assert_eq!(sampler.sample(&logits, &[0], &mut rng), 1);
}

#[test]
fn min_p_and_top_p_collapse_to_the_dominant_token() {
    let sampler = Sampler::new(SamplerConfig {
        temperature: 1.0,
        top_p: 0.9,
        min_p: 0.05,
        repetition_penalty: 1.0,
        repetition_window: 0,
    });
    let mut logits = vec![-10.0; 100];
    logits[42] = 10.0;
    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..20 {
        assert_eq!(sampler.sample(&logits, &[], &mut rng), 42);
    }
}
