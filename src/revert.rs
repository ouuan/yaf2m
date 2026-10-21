use blake3::Hash;
use chrono::{DateTime, TimeDelta, Utc};

/// Observation history is independent of the last mailed content. In particular, suppressing a
/// notification must not move the baseline used for the next diff.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RevertState {
    pub observed_hash: Option<Hash>,
    pub stable_since: Option<DateTime<Utc>>,
    pub stable_count: i32,
    pub recent_reverts: Vec<DateTime<Utc>>,
    pub suppressing: bool,
}

impl RevertState {
    /// Observe one item once per successful check. `revert` means the existing notification
    /// logic found a known version with a visible diff from the last mailed content. The return
    /// value permits that notification; new versions must always be mailed independently.
    pub fn observe(
        &mut self,
        update_hash: Hash,
        revert: bool,
        now: DateTime<Utc>,
        window: TimeDelta,
        stable_for: TimeDelta,
    ) -> bool {
        let changed = self.observed_hash != Some(update_hash);
        if changed {
            self.observed_hash = Some(update_hash);
            self.stable_since = Some(now);
            self.stable_count = 1;
        } else {
            self.stable_count = self.stable_count.saturating_add(1);
        }

        if self.suppressing {
            let stable = self.stable_count >= 3
                && self
                    .stable_since
                    .is_some_and(|since| now.signed_duration_since(since) >= stable_for);
            if stable {
                self.suppressing = false;
                self.recent_reverts.clear();
            }
            return stable;
        }

        self.recent_reverts
            .retain(|&time| now.signed_duration_since(time) <= window);
        if changed && revert {
            // Only the two previous transitions are needed to detect the third one.
            if self.recent_reverts.len() >= 2 {
                self.suppressing = true;
                self.recent_reverts.clear();
                return false;
            }
            self.recent_reverts.push(now);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn observe(state: &mut RevertState, version: &str, revert: bool, minutes: i64) -> bool {
        state.observe(
            blake3::hash(version.as_bytes()),
            revert,
            Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap() + TimeDelta::minutes(minutes),
            TimeDelta::hours(6),
            TimeDelta::hours(3),
        )
    }

    fn suppressing() -> RevertState {
        let mut state = RevertState::default();
        assert!(observe(&mut state, "B", false, 0));
        assert!(observe(&mut state, "A", true, 60));
        assert!(observe(&mut state, "B", true, 120));
        assert!(!observe(&mut state, "A", true, 180));
        assert!(state.suppressing);
        state
    }

    #[test]
    fn third_historical_transition_starts_suppression() {
        suppressing();
    }

    #[test]
    fn repeated_observations_do_not_count_as_transitions() {
        let mut state = RevertState::default();
        for minute in 0..10 {
            assert!(observe(&mut state, "A", true, minute));
        }
        assert_eq!(state.recent_reverts.len(), 1);
        assert!(!state.suppressing);
    }

    #[test]
    fn window_is_rolling_and_includes_its_boundary() {
        let mut state = RevertState::default();
        assert!(observe(&mut state, "A", true, 0));
        assert!(observe(&mut state, "B", true, 300));
        let mut at_boundary = state.clone();
        assert!(!observe(&mut at_boundary, "A", true, 360));
        assert!(observe(&mut state, "A", true, 361));
        assert_eq!(state.recent_reverts.len(), 2);
        assert!(!state.suppressing);
    }

    #[test]
    fn alternating_versions_never_stabilize_even_after_the_window_expires() {
        let mut state = suppressing();
        for hour in 4..100 {
            let version = if hour % 2 == 0 { "B" } else { "A" };
            assert!(!observe(&mut state, version, true, hour * 60));
        }
        assert!(state.suppressing);
    }

    #[test]
    fn stability_requires_both_time_and_observation_count() {
        let mut state = suppressing();
        assert!(!observe(&mut state, "A", true, 181));
        assert!(!observe(&mut state, "A", true, 182));
        assert!(observe(&mut state, "A", true, 360));
        assert!(!state.suppressing);
        assert!(state.recent_reverts.is_empty());

        let mut state = suppressing();
        // A long gap (failed fetches or a missing item) is not another observation.
        assert!(!observe(&mut state, "A", true, 1000));
        assert!(observe(&mut state, "A", true, 1001));
    }

    #[test]
    fn version_changes_restart_stability_and_new_versions_keep_suppression() {
        let mut state = suppressing();
        assert!(!observe(&mut state, "A", true, 240));
        // The caller still mails C immediately. Observing it does not end suppression.
        assert!(!observe(&mut state, "C", false, 300));
        assert!(!observe(&mut state, "C", false, 360));
        assert!(!observe(&mut state, "C", false, 420));
        assert!(state.suppressing);
        assert!(observe(&mut state, "C", false, 480));
        assert!(!state.suppressing);
        assert!(observe(&mut state, "A", true, 481));
        assert_eq!(state.recent_reverts.len(), 1);
    }

    #[test]
    fn changes_without_a_visible_revert_do_not_trigger_suppression() {
        let mut state = RevertState::default();
        for minute in 0..10 {
            assert!(observe(&mut state, &minute.to_string(), false, minute));
        }
        assert!(state.recent_reverts.is_empty());
        assert!(!state.suppressing);
    }

    #[test]
    fn configuration_changes_apply_to_existing_history() {
        let mut state = suppressing();
        assert!(!observe(&mut state, "A", true, 181));
        assert!(state.observe(
            blake3::hash(b"A"),
            true,
            state.stable_since.unwrap() + TimeDelta::hours(1),
            TimeDelta::hours(6),
            TimeDelta::hours(1),
        ));
        assert!(!state.suppressing);
    }
}
