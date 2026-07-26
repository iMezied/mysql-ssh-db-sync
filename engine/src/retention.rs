//! Backup retention policy.
//!
//! The bash predecessor never deleted anything, so backup directories grew
//! without bound. Deletion is destructive, so the policy is computed as a plan
//! first and always reported in the job log before anything is removed.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
pub struct RetentionPolicy {
    /// Keep at most this many of the most recent artifacts.
    pub keep_last: Option<u32>,
    /// Delete artifacts older than this many days.
    pub max_age_days: Option<u32>,
}

impl RetentionPolicy {
    pub const fn is_enabled(&self) -> bool {
        self.keep_last.is_some() || self.max_age_days.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RetentionCandidate {
    pub path: String,
    pub created_at: DateTime<Utc>,
    #[specta(type = f64)]
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RetentionPlan {
    pub keep: Vec<RetentionCandidate>,
    pub delete: Vec<RetentionCandidate>,
    #[specta(type = f64)]
    pub bytes_reclaimed: u64,
}

/// Decide which artifacts to remove.
///
/// Both rules are applied, and an artifact is deleted if *either* says so. The
/// newest artifact is never deleted, whatever the policy says: a retention
/// setting must not be able to leave a user with no backup at all.
pub fn plan_retention(
    mut candidates: Vec<RetentionCandidate>,
    policy: RetentionPolicy,
    now: DateTime<Utc>,
) -> RetentionPlan {
    // Newest first.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.created_at));

    if !policy.is_enabled() || candidates.is_empty() {
        return RetentionPlan {
            keep: candidates,
            delete: Vec::new(),
            bytes_reclaimed: 0,
        };
    }

    let age_cutoff = policy
        .max_age_days
        .map(|d| now - Duration::days(i64::from(d)));

    let mut keep = Vec::new();
    let mut delete = Vec::new();

    for (index, candidate) in candidates.into_iter().enumerate() {
        // Index 0 is the most recent artifact and is always retained.
        let is_newest = index == 0;

        let over_count = policy.keep_last.is_some_and(|n| index >= n.max(1) as usize);
        let too_old = age_cutoff.is_some_and(|cutoff| candidate.created_at < cutoff);

        if !is_newest && (over_count || too_old) {
            delete.push(candidate);
        } else {
            keep.push(candidate);
        }
    }

    let bytes_reclaimed = delete.iter().map(|c| c.size_bytes).sum();

    RetentionPlan {
        keep,
        delete,
        bytes_reclaimed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(days_ago: i64, size: u64) -> RetentionCandidate {
        RetentionCandidate {
            path: format!("/backups/b{days_ago}.sql.gz"),
            created_at: Utc::now() - Duration::days(days_ago),
            size_bytes: size,
        }
    }

    fn paths(items: &[RetentionCandidate]) -> Vec<&str> {
        items.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn disabled_policy_deletes_nothing() {
        let plan = plan_retention(
            vec![candidate(0, 1), candidate(100, 1)],
            RetentionPolicy::default(),
            Utc::now(),
        );
        assert!(plan.delete.is_empty());
        assert_eq!(plan.keep.len(), 2);
    }

    #[test]
    fn keep_last_retains_the_n_newest() {
        let plan = plan_retention(
            vec![
                candidate(0, 1),
                candidate(1, 1),
                candidate(2, 1),
                candidate(3, 1),
            ],
            RetentionPolicy {
                keep_last: Some(2),
                max_age_days: None,
            },
            Utc::now(),
        );
        assert_eq!(
            paths(&plan.keep),
            vec!["/backups/b0.sql.gz", "/backups/b1.sql.gz"]
        );
        assert_eq!(plan.delete.len(), 2);
    }

    #[test]
    fn max_age_deletes_older_artifacts() {
        let plan = plan_retention(
            vec![candidate(0, 1), candidate(10, 1), candidate(40, 1)],
            RetentionPolicy {
                keep_last: None,
                max_age_days: Some(30),
            },
            Utc::now(),
        );
        assert_eq!(paths(&plan.delete), vec!["/backups/b40.sql.gz"]);
    }

    #[test]
    fn rules_combine_with_or_not_and() {
        // b5 survives the age rule but not the count rule.
        let plan = plan_retention(
            vec![candidate(0, 1), candidate(5, 1), candidate(90, 1)],
            RetentionPolicy {
                keep_last: Some(1),
                max_age_days: Some(30),
            },
            Utc::now(),
        );
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.delete.len(), 2);
    }

    #[test]
    fn newest_artifact_is_never_deleted_even_when_ancient() {
        // A user who backs up rarely must not be left with nothing.
        let plan = plan_retention(
            vec![candidate(365, 1)],
            RetentionPolicy {
                keep_last: Some(5),
                max_age_days: Some(7),
            },
            Utc::now(),
        );
        assert!(plan.delete.is_empty());
        assert_eq!(plan.keep.len(), 1);
    }

    #[test]
    fn keep_last_zero_still_retains_the_newest() {
        let plan = plan_retention(
            vec![candidate(0, 1), candidate(1, 1)],
            RetentionPolicy {
                keep_last: Some(0),
                max_age_days: None,
            },
            Utc::now(),
        );
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.delete.len(), 1);
    }

    #[test]
    fn reclaimed_bytes_are_summed() {
        let plan = plan_retention(
            vec![candidate(0, 100), candidate(1, 200), candidate(2, 300)],
            RetentionPolicy {
                keep_last: Some(1),
                max_age_days: None,
            },
            Utc::now(),
        );
        assert_eq!(plan.bytes_reclaimed, 500);
    }

    #[test]
    fn empty_input_is_handled() {
        let plan = plan_retention(
            Vec::new(),
            RetentionPolicy {
                keep_last: Some(3),
                max_age_days: Some(30),
            },
            Utc::now(),
        );
        assert!(plan.keep.is_empty());
        assert!(plan.delete.is_empty());
        assert_eq!(plan.bytes_reclaimed, 0);
    }

    #[test]
    fn unordered_input_is_sorted_before_planning() {
        let plan = plan_retention(
            vec![candidate(5, 1), candidate(0, 1), candidate(2, 1)],
            RetentionPolicy {
                keep_last: Some(1),
                max_age_days: None,
            },
            Utc::now(),
        );
        assert_eq!(paths(&plan.keep), vec!["/backups/b0.sql.gz"]);
    }
}
