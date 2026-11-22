//! Optional preparation gate. These are estimates, not bills or a cache API.
use crate::{backends::ModelInfo, session::CompletedTurnActivity};
use std::time::{Duration, Instant};

const MIN_WARM_RUNWAY: usize = 5;

#[derive(Clone, Copy)]
pub(super) struct Prices {
    input: f64,
    output: f64,
    read: f64,
    write: f64,
    ttl: Duration,
}

impl Prices {
    pub(super) fn from_model(model: &ModelInfo) -> Option<Self> {
        let valid = |rate: Option<f64>| rate.filter(|p| p.is_finite() && *p >= 0.0);
        Some(Self {
            input: valid(model.price_input)?,
            output: valid(model.price_output)?,
            read: valid(model.price_cache_read)?,
            write: valid(model.price_cache_write)?,
            ttl: Duration::from_secs(model.cache_lifetime_seconds.filter(|&s| s > 0)?),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    ExpiredBeforeTurn,
    WarmMidTurn,
}

pub(super) struct Timing {
    pub prices: Prices,
    pub activity: CompletedTurnActivity,
    pub calls: usize,
    pub last_warm_response: Option<Instant>,
}

impl Timing {
    pub(super) fn phase(&self) -> Option<Phase> {
        if self.calls == 0 {
            if self.activity.call_counts.is_empty() {
                return None;
            }
            let age = chrono::Utc::now()
                .signed_duration_since(self.activity.latest_completion?)
                .to_std()
                .ok()?;
            (age >= self.prices.ttl).then_some(Phase::ExpiredBeforeTurn)
        } else {
            (self.last_warm_response?.elapsed() < self.prices.ttl).then_some(Phase::WarmMidTurn)
        }
    }

    pub(super) fn expected_calls(&self) -> usize {
        expected_calls(self.calls, &self.activity.call_counts)
    }
}

fn expected_calls(this_turn: usize, completed_turns: &[usize]) -> usize {
    if completed_turns.is_empty() {
        return this_turn.max(1);
    }
    let mut counts = completed_turns.to_vec();
    let len = counts.len();
    let (_, median, _) = counts.select_nth_unstable(len / 2);
    median
        .saturating_sub(this_turn)
        .saturating_add(median.saturating_mul(len))
        .max(this_turn)
        .max(1)
}

/// Compare projected savings with one summary call and (mid-turn only) the
/// cache rewrite. `summary_input` is the actual estimated auxiliary prompt,
/// not a fraction of the source or a price ratio.
pub(super) fn pays(
    phase: Phase,
    prices: Prices,
    calls: usize,
    current: usize,
    target: usize,
    summary_input: usize,
    summary_output: usize,
) -> bool {
    let saved = current.saturating_sub(target) as f64;
    let summary = summary_input as f64 * prices.input + summary_output as f64 * prices.output;
    let (gain, cost) = match phase {
        Phase::ExpiredBeforeTurn => (
            saved * (prices.write + calls.saturating_sub(1) as f64 * prices.read),
            summary,
        ),
        Phase::WarmMidTurn if calls >= MIN_WARM_RUNWAY => (
            saved * prices.read * calls as f64,
            summary + target as f64 * prices.write,
        ),
        Phase::WarmMidTurn => return false,
    };
    saved > 0.0
        && gain.is_finite()
        && cost.is_finite()
        && if phase == Phase::ExpiredBeforeTurn {
            gain > cost
        } else {
            gain > 0.0 && gain >= cost
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_economics_priced_parity_and_unpriced_refusal() {
        // Independent priced oracle: fold_economics.py, 120k current / 70k
        // target, 45k sent / 50k output. Runtime instead estimates real Chaz
        // summary input and uses the configured output ceiling.
        for (read, write, input, output, breakeven, idle) in [
            (0.10, 1.25, 1.0, 3.0, 57, [false; 5]),
            (0.10, 1.25, 0.2, 0.8, 28, [true; 5]),
            (0.25, 1.0, 1.0, 4.0, 26, [false; 5]),
        ] {
            let model = ModelInfo {
                price_input: Some(input),
                price_output: Some(output),
                price_cache_read: Some(read),
                price_cache_write: Some(write),
                cache_lifetime_seconds: Some(300),
                ..Default::default()
            };
            let priced = Prices::from_model(&model).unwrap();
            let gate = |phase, n| pays(phase, priced, n, 120_000, 70_000, 45_000, 50_000);
            assert_eq!(
                (1..400).find(|&n| gate(Phase::WarmMidTurn, n)),
                Some(breakeven)
            );
            assert_eq!(
                [1, 2, 3, 5, 10].map(|n| gate(Phase::ExpiredBeforeTurn, n)),
                idle
            );
            let mut missing = model;
            missing.price_cache_write = None;
            assert!(Prices::from_model(&missing).is_none());
            missing.price_cache_write = Some(write);
            missing.cache_lifetime_seconds = None;
            assert!(Prices::from_model(&missing).is_none());
        }
        assert_eq!(expected_calls(6, &[]), 6);
        assert_eq!(expected_calls(6, &[10; 4]), 44);
        assert_eq!(expected_calls(6, &[10; 16]), 164);
    }

    #[test]
    fn warm_timing_requires_a_recent_matching_response() {
        let ttl = Duration::from_secs(300);
        let mut timing = Timing {
            prices: Prices {
                input: 1.0,
                output: 1.0,
                read: 0.1,
                write: 1.25,
                ttl,
            },
            activity: CompletedTurnActivity::default(),
            calls: 1,
            last_warm_response: None,
        };
        assert_eq!(timing.phase(), None);
        timing.last_warm_response = Some(Instant::now() - ttl);
        assert_eq!(timing.phase(), None);
        timing.last_warm_response = Some(Instant::now());
        assert_eq!(timing.phase(), Some(Phase::WarmMidTurn));
    }
}
