//! Ed25519 sign/verify benchmark for `SignedSessionEnvelope`.
//!
//! Targets per 16-RESEARCH §6:
//!   `SignedSessionEnvelope::seal` (sign path):             < 50µs mean
//!   `SignedSessionEnvelope::open` (verify + cache lookup): < 100µs mean
//!
//! These labels are CALIBRATION baselines for D-03 (informational-only on push;
//! hard regression gate lives in plan 16-09 via critcmp). Do not rename the
//! `bench_function` labels without also updating the critcmp baseline matcher.
//!
//! Run:  `cargo bench -p roz-bench --bench signed_envelope`
//! Save: `cargo bench -p roz-bench --bench signed_envelope -- --save-baseline <name>`

use std::time::Duration;

use chrono::DateTime;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_zenoh::envelope::{PeerKeyCache, SignedSessionEnvelope, sign_envelope, verify_envelope};

/// Canonical shared fixture — byte-identical to the one in:
///   - crates/roz-core/src/transport.rs tests
///   - crates/roz-zenoh/src/envelope.rs tests
///   - crates/roz-zenoh/tests/signed_session_relay_integration.rs
///
/// Drift here would change measured bytes-signed and invalidate historical
/// baselines, so keep it locked.
fn fixture_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-15-fixture".into()),
        correlation_id: CorrelationId("corr-15-fixture".into()),
        parent_event_id: None,
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).expect("valid"), // 2026-01-01T00:00:00Z
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

fn fixture() -> (SigningKey, EventEnvelope) {
    let key = SigningKey::generate(&mut OsRng);
    (key, fixture_envelope())
}

fn bench_seal(c: &mut Criterion) {
    let (key, envelope) = fixture();
    c.bench_function("SignedSessionEnvelope::seal", |b| {
        b.iter(|| {
            let signed = sign_envelope(black_box(&key), black_box(&envelope)).expect("sign ok");
            let _ = black_box(signed);
        });
    });
}

fn bench_open(c: &mut Criterion) {
    let (key, envelope) = fixture();
    // Pre-seal once outside the measured region so the bench measures open-only cost.
    let sealed: SignedSessionEnvelope = sign_envelope(&key, &envelope).expect("sign ok");

    // Build a PeerKeyCache with the verifying key inserted so the lookup
    // portion is exercised end-to-end (HashMap read + signature verify + JSON decode).
    let cache = PeerKeyCache::new();
    cache.insert(hex::encode(key.verifying_key().to_bytes()), key.verifying_key());

    c.bench_function("SignedSessionEnvelope::open", |b| {
        b.iter(|| {
            let opened = verify_envelope(black_box(&cache), black_box(&sealed)).expect("verify ok");
            let _ = black_box(opened);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(5))
        .measurement_time(Duration::from_secs(15))
        .sample_size(200);
    targets = bench_seal, bench_open
}
criterion_main!(benches);
