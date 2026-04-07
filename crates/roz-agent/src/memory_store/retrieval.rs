//! Memory retrieval and ranking for the `MemoryStore`.
//!
//! [`rank_and_budget`] ranks memory entries by class priority × verified × confidence × recency,
//! then caps the result at an approximate token budget.

use chrono::Utc;
use roz_core::memory::{Confidence, MemoryClass, MemoryEntry};

/// Class priority order: highest number = highest priority.
/// Safety > Procedure > Environment > Operator > Task > `WorkingWorld`
const fn class_priority(class: MemoryClass) -> u8 {
    match class {
        MemoryClass::Safety => 6,
        MemoryClass::Procedure => 5,
        MemoryClass::Environment => 4,
        MemoryClass::Operator => 3,
        MemoryClass::Task => 2,
        MemoryClass::WorkingWorld => 1,
    }
}

/// Numerical weight for confidence level.
const fn confidence_weight(confidence: Confidence) -> u8 {
    match confidence {
        Confidence::High => 3,
        Confidence::Medium => 2,
        Confidence::Low => 1,
    }
}

/// Approximate token count for a memory entry.
/// Uses a conservative estimate of 1 token ≈ 4 bytes of UTF-8.
fn approx_tokens(entry: &MemoryEntry) -> u32 {
    let char_count = entry.fact.len()
        + entry.memory_id.len()
        + entry.scope_key.len()
        + entry.source_ref.as_deref().map_or(0, str::len)
        + 64; // overhead for other fields
    u32::try_from(char_count / 4 + 1).unwrap_or(u32::MAX)
}

/// Compute a composite score for ranking.
/// Higher is better. Verified entries score double.
fn score(entry: &MemoryEntry) -> f64 {
    let priority = f64::from(class_priority(entry.class));
    let confidence = f64::from(confidence_weight(entry.confidence));
    let verified_mult = if entry.verified { 2.0 } else { 1.0 };

    // Recency: 1.0 for brand new, decays toward 0 over ~24h.
    // Entries without a stale_after get recency 1.0 (durable).
    // Precision loss from i64→f64 is acceptable here: second-level timestamps
    // are well within f64's exact integer range (< 2^53).
    #[allow(clippy::cast_precision_loss)]
    let recency = entry.stale_after.map_or(1.0, |deadline| {
        let now = Utc::now();
        let total_secs = (deadline - entry.created_at).num_seconds().max(1) as f64;
        let remaining_secs = (deadline - now).num_seconds().max(0) as f64;
        (remaining_secs / total_secs).min(1.0)
    });

    priority * verified_mult * confidence * (recency + 0.1)
}

/// Rank memory entries by: class priority × verified × confidence × recency.
/// Budget-capped by approximate token count.
///
/// Non-stale entries are sorted highest-score first; stale entries are
/// excluded entirely. The output is truncated once the cumulative token
/// estimate would exceed `budget_tokens`.
pub fn rank_and_budget(entries: &[MemoryEntry], budget_tokens: u32) -> Vec<MemoryEntry> {
    let now = Utc::now();

    // Filter stale entries, then score the rest.
    let mut scored: Vec<(f64, &MemoryEntry)> = entries
        .iter()
        .filter(|e| !e.is_stale(now))
        .map(|e| (score(e), e))
        .collect();

    // Sort descending by score; use memory_id as tiebreak for determinism.
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.partial_cmp(sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });

    // Apply token budget.
    let mut result = Vec::new();
    let mut used_tokens: u32 = 0;
    for (_, entry) in scored {
        let tokens = approx_tokens(entry);
        if used_tokens.saturating_add(tokens) > budget_tokens {
            break;
        }
        used_tokens = used_tokens.saturating_add(tokens);
        result.push(entry.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn make_entry(id: &str, class: MemoryClass, confidence: Confidence, verified: bool) -> MemoryEntry {
        MemoryEntry {
            memory_id: id.to_string(),
            class,
            scope_key: "test-scope".to_string(),
            fact: format!("Fact about {id}"),
            source_kind: roz_core::memory::MemorySourceKind::Observation,
            source_ref: None,
            confidence,
            verified,
            stale_after: Some(Utc::now() + Duration::hours(8)),
            created_at: Utc::now() - Duration::hours(4),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn safety_ranked_above_working_world() {
        // Safety (priority=6, High, unverified) vs WorkingWorld (priority=1, High, verified).
        // Safety priority 6 vs WW priority 1 — Safety wins even though WW is verified.
        // Score Safety: 6 × 1 × 3 × r = 18r
        // Score WW:     1 × 2 × 3 × r = 6r
        // Safety clearly wins.
        let entries = vec![
            make_entry("ww-1", MemoryClass::WorkingWorld, Confidence::High, true),
            make_entry("sf-1", MemoryClass::Safety, Confidence::High, false),
        ];
        let ranked = rank_and_budget(&entries, 1000);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].memory_id, "sf-1", "Safety should rank above WorkingWorld");
    }

    #[test]
    fn class_priority_order() {
        assert!(class_priority(MemoryClass::Safety) > class_priority(MemoryClass::Procedure));
        assert!(class_priority(MemoryClass::Procedure) > class_priority(MemoryClass::Environment));
        assert!(class_priority(MemoryClass::Environment) > class_priority(MemoryClass::Operator));
        assert!(class_priority(MemoryClass::Operator) > class_priority(MemoryClass::Task));
        assert!(class_priority(MemoryClass::Task) > class_priority(MemoryClass::WorkingWorld));
    }

    #[test]
    fn stale_entries_excluded() {
        let mut stale = make_entry("stale-1", MemoryClass::Safety, Confidence::High, true);
        stale.stale_after = Some(Utc::now() - Duration::hours(1));
        let fresh = make_entry("fresh-1", MemoryClass::WorkingWorld, Confidence::Low, false);

        let ranked = rank_and_budget(&[stale, fresh], 1000);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].memory_id, "fresh-1");
    }

    #[test]
    fn budget_cap_limits_results() {
        // Each entry has a fact ~18 chars + overhead ~64 = ~82 chars = ~21 tokens.
        // Set budget to just enough for 1 entry.
        let entries = vec![
            make_entry("e1", MemoryClass::Safety, Confidence::High, true),
            make_entry("e2", MemoryClass::Procedure, Confidence::High, true),
            make_entry("e3", MemoryClass::Environment, Confidence::High, true),
        ];
        // Each entry needs roughly 25 tokens; budget of 30 allows only 1.
        let ranked = rank_and_budget(&entries, 30);
        assert_eq!(ranked.len(), 1, "budget should cap results");
    }

    #[test]
    fn verified_boosts_rank() {
        let unverified = make_entry("uv-1", MemoryClass::Procedure, Confidence::High, false);
        let verified = make_entry("v-1", MemoryClass::Procedure, Confidence::High, true);

        let ranked = rank_and_budget(&[unverified, verified], 1000);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].memory_id, "v-1", "verified entry should rank higher");
    }

    #[test]
    fn empty_input_returns_empty() {
        let ranked = rank_and_budget(&[], 1000);
        assert!(ranked.is_empty());
    }

    #[test]
    fn zero_budget_returns_empty() {
        let entries = vec![make_entry("e1", MemoryClass::Safety, Confidence::High, true)];
        let ranked = rank_and_budget(&entries, 0);
        assert!(ranked.is_empty());
    }
}
