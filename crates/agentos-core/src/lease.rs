use std::time::Duration;

/// The longest lease an agent session may have (four hours). A session lease is also never
/// longer than the task's remaining deadline; every other kind keeps the shorter cap.
pub const MAX_SESSION_TIMEOUT_MS: i64 = 14_400_000;

/// Per-effect wall-clock timeouts used to size leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectTimeouts {
    pub verification: Duration,
    pub other: Duration,
    /// An agent session's timeout before the task deadline clips it (at most
    /// [`MAX_SESSION_TIMEOUT_MS`]).
    pub session: Duration,
}

impl Default for EffectTimeouts {
    fn default() -> Self {
        EffectTimeouts {
            verification: Duration::from_secs(70),
            other: Duration::from_secs(30),
            session: Duration::from_millis(MAX_SESSION_TIMEOUT_MS as u64),
        }
    }
}

/// Lease expiry: `now + min(timeout, deadline - now)`. A `task_deadline_ms` of 0 means no
/// deadline. A result `<= now_ms` means the effect must not be launched.
pub fn lease_expiry_ms(now_ms: i64, timeout_ms: i64, task_deadline_ms: i64) -> i64 {
    if task_deadline_ms == 0 {
        return now_ms.saturating_add(timeout_ms);
    }
    now_ms.saturating_add(timeout_ms.min(task_deadline_ms.saturating_sub(now_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_capped_by_timeout_and_deadline() {
        assert_eq!(lease_expiry_ms(1_000_000, 70_000, 1_030_000), 1_030_000);
        assert_eq!(lease_expiry_ms(1_000_000, 70_000, 2_000_000), 1_070_000);
        assert!(lease_expiry_ms(1_000_000, 70_000, 900_000) <= 1_000_000);
        assert_eq!(lease_expiry_ms(1_000_000, 70_000, 0), 1_070_000);
    }

    #[test]
    fn lease_math_never_overflows() {
        let ext = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
        for &now in &ext {
            for &timeout in &ext {
                for &deadline in &ext {
                    let _ = lease_expiry_ms(now, timeout, deadline);
                }
            }
        }
        assert_eq!(lease_expiry_ms(i64::MAX, 70_000, 0), i64::MAX);
        assert_eq!(lease_expiry_ms(1_000_000, i64::MAX, 0), i64::MAX);
        assert_eq!(lease_expiry_ms(1_000_000, i64::MAX, i64::MAX), i64::MAX);
        assert_eq!(
            lease_expiry_ms(i64::MIN, 70_000, i64::MAX),
            i64::MIN + 70_000
        );
    }

    #[test]
    fn lease_edge_cases_mean_do_not_launch() {
        let now = 1_000_000;
        assert_eq!(lease_expiry_ms(now, 70_000, now), now);
        assert!(lease_expiry_ms(now, 70_000, -5) <= now);
        assert!(lease_expiry_ms(now, 70_000, i64::MIN) <= now);
        assert!(lease_expiry_ms(now, 0, 2_000_000) <= now);
        assert!(lease_expiry_ms(now, -1, 2_000_000) <= now);
        assert!(lease_expiry_ms(now, -1, 0) <= now);
    }

    #[test]
    fn effect_timeouts_default_to_70s_verification_30s_other() {
        let t = EffectTimeouts::default();
        assert_eq!(t.verification, std::time::Duration::from_secs(70));
        assert_eq!(t.other, std::time::Duration::from_secs(30));
        assert_eq!(t.session, std::time::Duration::from_secs(4 * 3600));
    }

    #[test]
    fn session_lease_without_a_deadline_is_its_timeout_and_a_deadline_clips_it() {
        let now = 1_000_000;
        let session = MAX_SESSION_TIMEOUT_MS;
        assert_eq!(lease_expiry_ms(now, session, 0), now + session);
        // Half an hour left of a two-hour deadline: the lease is the remaining time.
        let deadline = now + 7_200_000;
        assert_eq!(lease_expiry_ms(now, 1_800_000, deadline), now + 1_800_000);
        assert_eq!(lease_expiry_ms(now, session, deadline), deadline);
    }
}
