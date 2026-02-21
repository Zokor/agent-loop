use std::time::Instant;

/// Signal emitted by the stuck detector after observing a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckSignal {
    /// No stuck condition detected.
    Ok,
    /// No diff produced for `consecutive_rounds` rounds in a row.
    NoDiffProgress { consecutive_rounds: u32 },
    /// Diff pattern is oscillating (A -> B -> A).
    Oscillating,
    /// Wall-clock time exceeded the configured threshold.
    TimeThresholdExceeded { elapsed_minutes: u64 },
}

/// Detects when an agent loop is stuck by tracking diff output across rounds.
pub struct StuckDetector {
    /// Consecutive rounds with no diff output.
    no_diff_count: u32,
    /// Number of consecutive no-diff rounds before signalling.
    no_diff_threshold: u32,
    /// FNV-1a hashes of diffs observed each round.
    diff_hashes: Vec<u64>,
    /// Instant when the detector was created.
    start_time: Instant,
    /// Minutes of wall-clock time before signalling.
    time_threshold_minutes: u64,
    /// Whether stuck detection is enabled at all.
    enabled: bool,
}

impl StuckDetector {
    /// Create a new `StuckDetector`.
    ///
    /// * `enabled` - whether detection is active (from config `stuck_detection_enabled`)
    /// * `no_diff_threshold` - rounds without a diff before signalling (from config `stuck_no_diff_rounds`)
    /// * `time_threshold_minutes` - wall-clock minutes before signalling (from config `stuck_threshold_minutes`)
    pub fn new(enabled: bool, no_diff_threshold: u32, time_threshold_minutes: u64) -> Self {
        Self {
            no_diff_count: 0,
            no_diff_threshold,
            diff_hashes: Vec::new(),
            start_time: Instant::now(),
            time_threshold_minutes,
            enabled,
        }
    }

    /// Observe the result of a round and return a `StuckSignal`.
    ///
    /// `diff` is the textual diff output for the round (empty string means no changes).
    pub fn observe_round(&mut self, diff: &str) -> StuckSignal {
        if !self.enabled {
            return StuckSignal::Ok;
        }

        // Check wall-clock time first.
        let elapsed = self.start_time.elapsed();
        let elapsed_minutes = elapsed.as_secs() / 60;
        if elapsed_minutes >= self.time_threshold_minutes {
            return StuckSignal::TimeThresholdExceeded { elapsed_minutes };
        }

        if diff.is_empty() {
            self.no_diff_count += 1;
            self.diff_hashes.push(0);

            if self.no_diff_count >= self.no_diff_threshold {
                return StuckSignal::NoDiffProgress {
                    consecutive_rounds: self.no_diff_count,
                };
            }
        } else {
            self.no_diff_count = 0;
            let hash = fnv1a_hash(diff.as_bytes());
            self.diff_hashes.push(hash);

            // Check oscillation: current hash equals hash from 2 rounds ago.
            let len = self.diff_hashes.len();
            if len >= 3 && self.diff_hashes[len - 1] == self.diff_hashes[len - 3] {
                return StuckSignal::Oscillating;
            }
        }

        StuckSignal::Ok
    }
}

/// Compute the FNV-1a hash of a byte slice.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_detector_returns_ok() {
        let mut detector = StuckDetector::new(true, 3, 60);
        assert_eq!(detector.observe_round("some diff"), StuckSignal::Ok);
    }

    #[test]
    fn empty_diff_increments_counter_and_signals_at_threshold() {
        let mut detector = StuckDetector::new(true, 3, 60);

        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(
            detector.observe_round(""),
            StuckSignal::NoDiffProgress {
                consecutive_rounds: 3,
            }
        );
    }

    #[test]
    fn non_empty_diff_resets_no_diff_counter() {
        let mut detector = StuckDetector::new(true, 3, 60);

        // Accumulate two empty rounds.
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);

        // A non-empty diff resets the counter.
        assert_eq!(detector.observe_round("changed something"), StuckSignal::Ok);

        // Two more empty rounds should not yet trigger (counter was reset).
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);

        // Third empty round after reset hits the threshold.
        assert_eq!(
            detector.observe_round(""),
            StuckSignal::NoDiffProgress {
                consecutive_rounds: 3,
            }
        );
    }

    #[test]
    fn oscillating_pattern_detected() {
        let mut detector = StuckDetector::new(true, 10, 60);

        assert_eq!(detector.observe_round("diff A"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff B"), StuckSignal::Ok);
        // Third round matches the first (A -> B -> A).
        assert_eq!(detector.observe_round("diff A"), StuckSignal::Oscillating);
    }

    #[test]
    fn time_threshold_fires() {
        // Threshold of 0 minutes means any elapsed time exceeds it.
        let mut detector = StuckDetector::new(true, 10, 0);
        assert_eq!(
            detector.observe_round("some diff"),
            StuckSignal::TimeThresholdExceeded { elapsed_minutes: 0 }
        );
    }

    #[test]
    fn disabled_detector_always_returns_ok() {
        let mut detector = StuckDetector::new(false, 1, 0);

        // Would normally trigger NoDiffProgress and TimeThresholdExceeded, but disabled.
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(detector.observe_round(""), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff A"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff B"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff A"), StuckSignal::Ok);
    }

    #[test]
    fn different_diffs_do_not_trigger_oscillation() {
        let mut detector = StuckDetector::new(true, 10, 60);

        assert_eq!(detector.observe_round("diff A"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff B"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff C"), StuckSignal::Ok);
        assert_eq!(detector.observe_round("diff D"), StuckSignal::Ok);
    }
}
