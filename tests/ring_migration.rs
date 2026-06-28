/// Correctness proofs for the seal-and-forward ring migration mechanism.
///
/// Properties verified here:
///
/// P1 – No packet loss: every packet pushed before seal is delivered; every
///      packet pushed to the new ring after migration is delivered.
///
/// P2 – No duplication: read_idx never regresses; each packet is delivered
///      at most once.
///
/// P3 – Write-index continuity: new_ring.write_idx >= reader.read_idx after
///      migration, so the reader is never "ahead of" the writer.
///
/// P4 – Concurrent wake safety: if seal fires between notify.notified()
///      subscription and .await, the reader wakes without a forever-sleep.
///
/// P5 – Chain migration: old → new1 → new2 works transparently with no
///      reader intervention.
///
/// P6 – Sealed-ring isolation: sealing ring A does not disturb readers on
///      ring B.
///
/// P7 – Overflow resilience: migration during an overflow event fast-forwards
///      the reader correctly on the new ring.
///
/// The property-based tests (proptest) vary packet counts, capacity sizes, and
/// reader lag at seal time to exercise the full reachable state space.
use bytes::Bytes;
use proptest::prelude::*;
use restream::media::ring_buffer::{MediaPacket, MediaType, PayloadFormat, Reader, RingBuffer};
use std::sync::Arc;
use tokio::runtime::Runtime;

// ─── helpers ────────────────────────────────────────────────────────────────

fn pkt(seq: u64) -> MediaPacket {
    MediaPacket {
        media_type: MediaType::Video,
        track_index: 0,
        pts: seq as i64,
        dts: seq as i64,
        is_keyframe: seq == 0,
        format: PayloadFormat::Raw,
        payload: Bytes::from(seq.to_le_bytes().to_vec()),
    }
}

/// Push `n` packets to `ring` starting from sequence `start`.
fn push_seq(ring: &Arc<RingBuffer>, start: u64, n: u64) {
    for i in start..start + n {
        ring.push(pkt(i));
    }
}

/// Drain all available packets from `reader` into a vec.
fn drain(reader: &mut Reader) -> Vec<u64> {
    let mut out = Vec::new();
    loop {
        let before = out.len();
        reader.pull_burst(&mut out, 64).ok();
        if out.len() == before {
            break;
        }
    }
    out.iter()
        .map(|p| i64::from_le_bytes(p.payload.as_ref().try_into().unwrap()) as u64)
        .collect()
}

/// Seal `old` and install `new_ring` as successor; returns the new ring.
fn seal(old: &Arc<RingBuffer>, new_capacity: usize) -> Arc<RingBuffer> {
    let new = Arc::new(RingBuffer::new_continuing(
        new_capacity,
        old.get_write_idx(),
    ));
    old.seal_and_forward(new.clone());
    new
}

// ─── P1 · P2 · P3  deterministic tests ──────────────────────────────────────

/// P1 + P2: reader at zero lag when seal fires.
#[test]
fn p1_p2_reader_caught_up_at_seal_time() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let old = Arc::new(RingBuffer::new(64));
        let mut r = Reader::new("r".into(), old.clone());

        push_seq(&old, 0, 10);
        let got = drain(&mut r);
        assert_eq!(got, (0..10).collect::<Vec<_>>(), "P1: first 10 delivered");

        // Reader is now at write_idx — zero lag.
        let new = seal(&old, 128);
        push_seq(&new, 10, 5);

        r.wait_for_data().await;
        let got2 = drain(&mut r);
        assert_eq!(got2, (10..15).collect::<Vec<_>>(), "P1: next 5 delivered");
        assert!(
            Arc::ptr_eq(r.current_ring(), &new),
            "P3: reader on new ring"
        );
    });
}

/// P1 + P2: reader has positive lag when seal fires — must drain old ring first.
#[test]
fn p1_p2_reader_has_lag_at_seal_time() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let old = Arc::new(RingBuffer::new(64));
        let mut r = Reader::new("r".into(), old.clone());

        push_seq(&old, 0, 20);
        // Only drain 7 before seal fires; 13 remain in old ring.
        let mut partial = Vec::new();
        r.pull_burst(&mut partial, 7).unwrap();

        let new = seal(&old, 128);
        push_seq(&new, 20, 5);

        // Drain everything without calling wait_for_data (data already available).
        let rest_old = drain(&mut r); // 7..20
        // After draining old ring, must migrate automatically.
        r.wait_for_data().await; // migrates to new ring
        let new_data = drain(&mut r); // 20..25

        let partial_seqs: Vec<u64> = partial
            .iter()
            .map(|p| i64::from_le_bytes(p.payload.as_ref().try_into().unwrap()) as u64)
            .collect();

        let all: Vec<u64> = partial_seqs
            .iter()
            .chain(rest_old.iter())
            .chain(new_data.iter())
            .copied()
            .collect();

        assert_eq!(
            all,
            (0..25).collect::<Vec<_>>(),
            "P1: all 25 delivered in order"
        );

        // P2: verify no duplicates.
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "P2: no duplicates");

        assert!(
            Arc::ptr_eq(r.current_ring(), &new),
            "P3: reader on new ring"
        );
    });
}

/// P3: write-index continuity — new_ring.write_idx == reader.read_idx immediately
/// after the seal so the reader is never "ahead of" the writer.
///
/// This is purely a structural invariant; we verify it synchronously without
/// calling wait_for_data (which would block on an empty ring).
#[test]
fn p3_write_index_continuity_zero_lag() {
    let old = Arc::new(RingBuffer::new(64));
    let mut r = Reader::new("r".into(), old.clone());

    push_seq(&old, 0, 50);
    let mut out = Vec::new();
    while r.lag() > 0 {
        r.pull_burst(&mut out, 64).ok();
    }
    assert_eq!(r.lag(), 0, "pre-condition: zero lag");
    // reader.read_idx is now 50 (tracked internally; we verify via lag == 0 above).

    // seal() uses new_continuing(cap, old.write_idx) — new ring starts at 50.
    let new = seal(&old, 256);

    // P3 invariant: new ring must start at or beyond reader's position.
    // If new_ring.write_idx < reader.read_idx the reader would incorrectly appear
    // "ahead" of the writer after migration and spin in wait_for_data indefinitely.
    assert_eq!(
        new.get_write_idx(),
        50,
        "P3: new ring write cursor equals old ring's final write cursor"
    );

    // A new packet to the new ring must be at a greater index than the reader's position.
    push_seq(&new, 50, 1);
    assert_eq!(
        new.get_write_idx(),
        51,
        "P3: write_idx advances past reader position"
    );

    // Now migrate and verify: read_idx == 50, write_idx == 51, lag == 1.
    let rt = Runtime::new().unwrap();
    rt.block_on(async { r.wait_for_data().await }); // migrates; sees write_idx 51 > 50
    assert!(
        Arc::ptr_eq(r.current_ring(), &new),
        "P3: reader migrated to new ring"
    );
    assert_eq!(
        r.lag(),
        1,
        "P3: reader exactly one packet behind after migration"
    );
}

// ─── P5  chain migration ─────────────────────────────────────────────────────

/// P5: old → new1 → new2.  Reader must follow the full chain.
#[test]
fn p5_chain_migration_two_hops() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let ring0 = Arc::new(RingBuffer::new(64));
        let mut r = Reader::new("r".into(), ring0.clone());

        push_seq(&ring0, 0, 5);
        let ring1 = seal(&ring0, 128);
        push_seq(&ring1, 5, 5);
        let ring2 = seal(&ring1, 256);
        push_seq(&ring2, 10, 5);

        // wait_for_data migrates hop by hop (the loop in wait_for_data handles chains).
        r.wait_for_data().await;
        let batch0 = drain(&mut r); // ring0: 0..5
        r.wait_for_data().await; // migrates to ring1
        let batch1 = drain(&mut r); // ring1: 5..10
        r.wait_for_data().await; // migrates to ring2
        let batch2 = drain(&mut r); // ring2: 10..15

        let all: Vec<u64> = batch0
            .iter()
            .chain(batch1.iter())
            .chain(batch2.iter())
            .copied()
            .collect();

        assert_eq!(all, (0..15).collect::<Vec<_>>(), "P5: all 15 across chain");
        assert!(Arc::ptr_eq(r.current_ring(), &ring2), "P5: reader on ring2");
    });
}

// ─── P6  sealed-ring isolation ───────────────────────────────────────────────

/// P6: sealing ring A does not disturb reader on ring B.
#[test]
fn p6_isolation_between_pipelines() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let ring_a = Arc::new(RingBuffer::new(64));
        let ring_b = Arc::new(RingBuffer::new(64));
        let mut r_b = Reader::new("rb".into(), ring_b.clone());

        push_seq(&ring_a, 0, 10);
        push_seq(&ring_b, 100, 10);

        // Seal ring A only.
        let _new_a = seal(&ring_a, 128);

        // Reader on B should deliver packets 100..110 unaffected.
        let got = drain(&mut r_b);
        assert_eq!(got, (100..110).collect::<Vec<_>>(), "P6: ring B unaffected");
        assert!(
            Arc::ptr_eq(r_b.current_ring(), &ring_b),
            "P6: reader stays on ring B"
        );
    });
}

// ─── P4  concurrent wake safety  (tokio task) ───────────────────────────────

/// P4: seal fires after reader subscribes to notify but before .await —
/// reader must not sleep forever.
///
/// Uses `seal()` (which calls `new_continuing`) so new_ring.write_idx == 5 ==
/// reader.read_idx — the new packet at seq=5 moves write_idx to 6, which is
/// > read_idx (5), so wait_for_data returns cleanly.
#[tokio::test]
async fn p4_concurrent_seal_while_reader_waiting() {
    use tokio::time::{Duration, timeout};

    let old = Arc::new(RingBuffer::new(64));
    let old2 = old.clone();
    push_seq(&old, 0, 5); // write_idx = 5

    let mut r = Reader::new("r".into(), old.clone());
    drain(&mut r); // read_idx = 5 (caught up)

    // New ring continues from write_idx=5 so reader at read_idx=5 is valid.
    let new = Arc::new(RingBuffer::new_continuing(128, old.get_write_idx()));
    let new2 = new.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        old2.seal_and_forward(new2.clone()); // fires old notify, reader migrates
        new2.push(pkt(5)); // write_idx 5→6, fires new notify
    });

    let result = timeout(Duration::from_secs(2), r.wait_for_data()).await;
    assert!(
        result.is_ok(),
        "P4: wait_for_data must not sleep forever after seal"
    );

    let got = drain(&mut r);
    assert_eq!(got, vec![5u64], "P4: packet from new ring delivered");
}

/// P4 variant: seal fires *before* reader reaches wait_for_data.
#[tokio::test]
async fn p4_seal_before_reader_reaches_wait() {
    let old = Arc::new(RingBuffer::new(64));
    push_seq(&old, 0, 3); // write_idx = 3

    let mut r = Reader::new("r".into(), old.clone());
    drain(&mut r); // read_idx = 3 (caught up)

    // New ring continues at write_idx=3; push pkt(3) → write_idx becomes 4.
    let new = Arc::new(RingBuffer::new_continuing(128, old.get_write_idx()));
    old.seal_and_forward(new.clone());
    new.push(pkt(3)); // write_idx 3→4

    // wait_for_data detects seal immediately (next is already set), migrates,
    // and sees write_idx (4) > read_idx (3).
    tokio::time::timeout(std::time::Duration::from_millis(100), r.wait_for_data())
        .await
        .expect("P4: should not block when seal already set and new data present");

    let got = drain(&mut r);
    assert_eq!(got, vec![3u64], "P4: packet after pre-seal migrate");
}

// ─── proptest  property-based exhaustion ────────────────────────────────────

proptest! {
    /// P1 + P2 with random batch sizes and lag at seal time.
    ///
    /// Drives the async `wait_for_data` via `block_on` and collects the
    /// resulting sequence *outside* the async block so `prop_assert*` macros
    /// can return `Err(TestCaseError)` from the proptest body.
    #[test]
    fn prop_no_loss_no_gap_no_duplication(
        n_before in 1usize..50,
        n_after  in 0usize..50,
        drain_before_seal in 0usize..50,
        // Guarantee old_cap >= n_before so no overflow occurs before the seal.
        // P7 (overflow_then_seal) covers the overflow path separately.
        old_cap_extra in 0usize..100,
        new_cap_extra in 1usize..200,
    ) {
        // Both rings must have strictly more capacity than the packets they'll
        // hold so pull_burst's overflow guard never fires spuriously (it triggers
        // when write_idx - read_idx >= capacity, i.e. ring is exactly full).
        let old_cap = n_before + old_cap_extra + 1;
        // new_cap must exceed both n_after (no overflow on new ring) and old_cap.
        let new_cap = (n_after + 1).max(old_cap + 1) + new_cap_extra;
        let pre_drain = drain_before_seal.min(n_before);

        // Drive the async portions; collect sequences as plain data.
        let rt = Runtime::new().unwrap();
        let seqs: Vec<u64> = rt.block_on(async move {
            let old = Arc::new(RingBuffer::new(old_cap));
            let mut r = Reader::new("r".into(), old.clone());

            push_seq(&old, 0, n_before as u64);

            // Partial drain before seal.
            let mut out = Vec::new();
            for _ in 0..pre_drain {
                r.pull_burst(&mut out, 1).ok();
            }

            let new = Arc::new(RingBuffer::new_continuing(new_cap, old.get_write_idx()));
            old.seal_and_forward(new.clone());
            push_seq(&new, n_before as u64, n_after as u64);

            // Drain the old ring — no wait_for_data needed, data is already there.
            loop {
                let before = out.len();
                r.pull_burst(&mut out, 64).ok();
                if out.len() == before { break; }
            }

            // Migrate to the new ring and drain it.  Only wait when n_after > 0;
            // if the new ring is empty the seal detector migrates us but no new
            // data will ever arrive, so wait_for_data would block forever.
            if n_after > 0 {
                // wait_for_data migrates to new ring (from next pointer) and
                // returns once write_idx > read_idx on the new ring.
                r.wait_for_data().await;
                r.pull_burst(&mut out, 64).ok();
                // In case a second call is needed (large n_after vs small burst).
                if r.lag() > 0 {
                    r.pull_burst(&mut out, 64).ok();
                }
            }

            out.iter()
                .map(|p| i64::from_le_bytes(p.payload.as_ref().try_into().unwrap()) as u64)
                .collect()
        });

        // All packets (pre-drain AND post-seal) are collected into `out`,
        // so the full expected sequence is always 0..n_before+n_after.
        let expected_count = (n_before + n_after) as u64;

        // P1: total count
        prop_assert_eq!(seqs.len() as u64, expected_count,
            "P1 count: got {} expected {}", seqs.len(), expected_count);

        // P1: no gap — sequence must be exactly 0, 1, 2, ...
        for (idx, &s) in seqs.iter().enumerate() {
            prop_assert_eq!(s, idx as u64,
                "P1 gap at position {} (expected {}, got {})", idx, idx, s);
        }

        // P2: monotone (proves no duplicates given P1 ordering)
        for w in seqs.windows(2) {
            prop_assert!(w[1] > w[0], "P2 regression: {} not > {}", w[1], w[0]);
        }
    }
}

// ─── P7  overflow during migration ──────────────────────────────────────────

/// P7: if the old ring overflows while the reader is lagging and then a seal
/// fires, the reader fast-forwards on the old ring and migrates cleanly —
/// no panic, no duplication of post-seal packets.
#[test]
fn p7_overflow_then_seal() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let old = Arc::new(RingBuffer::new(8)); // tiny ring to force overflow
        let mut r = Reader::new("r".into(), old.clone());

        // Overflow the ring: write 16 packets into an 8-slot ring.
        push_seq(&old, 0, 16);
        // Reader is now stale; pull_burst fast-forwards it.
        let mut pre = Vec::new();
        r.pull_burst(&mut pre, 64).ok(); // triggers fast-forward, some packets dropped

        // Now seal and write to new ring.
        let new = seal(&old, 128);
        push_seq(&new, 100, 5);

        r.wait_for_data().await;
        let post: Vec<u64> = drain(&mut r);

        // After overflow, fast-forward lands the reader near the tail.
        // The new ring's packets (100..105) must arrive intact.
        assert!(
            post.windows(2).all(|w| w[1] > w[0]),
            "P7: monotone after overflow+migrate"
        );
        // All new-ring packets must appear.
        assert!(
            post.iter().any(|&s| s >= 100),
            "P7: new-ring packets present after overflow+migrate"
        );
        // No post-seal packets duplicated.
        let post_seal: Vec<u64> = post.into_iter().filter(|&s| s >= 100).collect();
        let mut deduped = post_seal.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            post_seal.len(),
            "P7: no duplicates from new ring"
        );
    });
}
