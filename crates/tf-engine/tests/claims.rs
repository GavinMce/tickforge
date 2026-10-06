//! Strategies sharing Tier 1: holds counted per strategy, requests with a priority rule, denials counted.

use tf_core::{
    Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, TierAction, TierChange, Trade, TradeFlags,
};
use tf_engine::{
    Denied, Grant, Promoter, PromoterConfig, ScannerConfig, Tier0, TierError, tier_reason,
};

const SEC: Nanos = NANOS_PER_SEC;
const T0: Nanos = 1_767_571_200 * SEC;

const A: u16 = 1;
const B: u16 = 2;
const C: u16 = 3;
const D: u16 = 4;
const E: u16 = 5;

fn promoter(max: usize, dwell: u64) -> Promoter {
    let cfg = PromoterConfig {
        max_tier1: max,
        min_dwell_secs: dwell,
        ..PromoterConfig::default()
    };
    Promoter::new(
        cfg,
        ScannerConfig {
            min_volume: 1_000,
            ..ScannerConfig::default()
        },
        64,
    )
    .unwrap()
}

/// What the scanner would have done: a promotion nobody asked for.
fn scanner_promote(p: &mut Promoter, id: u32, at_sec: u64) {
    let c = TierChange {
        hdr: Header {
            ts_event: T0 + at_sec * SEC,
            ts_recv: T0 + at_sec * SEC,
            seq: 0,
            instrument: id,
            provider: ProviderId::Internal,
        },
        action: TierAction::Promote,
        reason: tier_reason::SCANNER_HIT,
        score: 9_000,
    };
    p.apply(&c).unwrap();
}

fn ask(p: &mut Promoter, who: u16, id: u32, at_sec: u64) -> (Grant, Vec<TierChange>) {
    let mut out = Vec::new();
    let g = p.request(who, id, T0 + at_sec * SEC, &mut out);
    (g, out)
}

#[test]
fn a_request_with_room_promotes_and_says_so_on_the_tape() {
    let mut p = promoter(3, 0);
    let (g, out) = ask(&mut p, A, 7, 100);
    assert_eq!(g, Grant::Promoted);
    assert_eq!(out.len(), 1);
    let c = out[0];
    assert_eq!(
        (
            c.hdr.instrument,
            c.action,
            c.reason,
            c.hdr.ts_recv,
            c.hdr.provider
        ),
        (
            7,
            TierAction::Promote,
            tier_reason::STRATEGY_REQUEST,
            T0 + 100 * SEC,
            ProviderId::Internal
        )
    );
    assert!(p.is_promoted(7) && p.is_wanted_by(A, 7) && !p.is_wanted_by(B, 7));
    assert!(p.symbol(7).is_some());
    // Again, by the same and by another: already there, interest recorded, nothing on the tape.
    for who in [A, B] {
        let (g, out) = ask(&mut p, who, 7, 101);
        assert_eq!((g, out.len()), (Grant::Already, 0));
    }
    assert!(p.is_wanted_by(B, 7));
    let st = p.owner_stats();
    assert_eq!(
        (st[0].0, st[0].1.requests, st[0].1.promoted, st[0].1.already),
        (A, 2, 1, 1)
    );
    assert_eq!(
        (st[1].0, st[1].1.requests, st[1].1.promoted, st[1].1.already),
        (B, 1, 0, 1)
    );
    // Sequence numbers keep rising across requests.
    let (_, o2) = ask(&mut p, A, 8, 102);
    assert!(o2[0].hdr.seq > c.hdr.seq);
}

#[test]
fn an_unknown_symbol_is_denied_and_counted() {
    let mut p = promoter(3, 0);
    let (g, out) = ask(&mut p, A, 64, 1);
    assert_eq!((g, out.len()), (Grant::Denied(Denied::Unknown), 0));
    assert_eq!(p.owner_stats()[0].1.denied_other, 1);
    assert!(!g.is_granted());
}

#[test]
fn equal_priority_never_evicts_and_the_denial_is_counted_against_the_asker() {
    let mut p = promoter(3, 0);
    for id in 0..3 {
        assert_eq!(ask(&mut p, A, id, 10).0, Grant::Promoted);
    }
    let (g, out) = ask(&mut p, B, 3, 20);
    assert_eq!((g, out.len()), (Grant::Denied(Denied::Full), 0));
    assert!(!p.is_promoted(3) && p.promoted().len() == 3);
    let stats = p.owner_stats();
    let b = stats.iter().find(|s| s.0 == B).unwrap().1;
    let a = stats.iter().find(|s| s.0 == A).unwrap().1;
    assert_eq!((b.requests, b.denied_full, b.promoted), (1, 1, 0));
    assert_eq!((a.denied_full, a.lost), (0, 0));
    // A strategy given a lower priority than the default cannot evict either, and one with the same
    // as the holder cannot.
    p.set_priority(B, 1);
    assert_eq!(ask(&mut p, B, 3, 21).0, Grant::Denied(Denied::Full));
    p.set_priority(B, 0);
    assert_eq!(ask(&mut p, B, 3, 22).0, Grant::Denied(Denied::Full));
}

#[test]
fn a_higher_priority_evicts_and_the_loser_is_told() {
    let mut p = promoter(3, 0);
    for id in 0..3 {
        ask(&mut p, A, id, 10);
    }
    p.set_priority(C, 5);
    let (g, out) = ask(&mut p, C, 3, 20);
    // All three are equal (level 1, one claimant, promoted at the same second): the lowest id goes.
    assert_eq!(g, Grant::PromotedByEviction { evicted: 0 });
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[0].hdr.instrument, out[0].action, out[0].reason),
        (0, TierAction::Demote, tier_reason::EVICTED)
    );
    assert_eq!(
        (out[1].hdr.instrument, out[1].action, out[1].reason),
        (3, TierAction::Promote, tier_reason::STRATEGY_REQUEST)
    );
    assert_eq!(
        (out[0].hdr.ts_recv, out[1].hdr.ts_recv),
        (T0 + 20 * SEC, T0 + 20 * SEC)
    );
    assert_eq!(out[1].hdr.seq, out[0].hdr.seq + 1);
    assert_eq!(p.promoted(), [1, 2, 3]);
    assert!(!p.is_wanted_by(A, 0) && p.is_wanted_by(C, 3));
    assert_eq!(p.drain_revoked(), [(A, 0)]);
    assert!(p.drain_revoked().is_empty());
    let stats = p.owner_stats();
    let a = stats.iter().find(|s| s.0 == A).unwrap().1;
    let c = stats.iter().find(|s| s.0 == C).unwrap().1;
    assert_eq!((a.lost, c.promoted, c.evicted_for), (1, 1, 1));
}

#[test]
fn the_victim_is_chosen_by_level_then_claimants_then_age_then_id() {
    // Level 0 (nobody asked) before anything claimed, however old the claimed one is; oldest first.
    let mut p = promoter(4, 0);
    p.set_priority(C, 9);
    ask(&mut p, A, 1, 10); // claimed, level 1
    scanner_promote(&mut p, 7, 200); // unclaimed, newer
    scanner_promote(&mut p, 6, 300); // unclaimed, newest
    ask(&mut p, A, 2, 400);
    let (g, _) = ask(&mut p, C, 20, 500);
    assert_eq!(
        g,
        Grant::PromotedByEviction { evicted: 7 },
        "oldest unclaimed first, though id 6 is lower"
    );
    let (g, _) = ask(&mut p, C, 21, 501);
    assert_eq!(g, Grant::PromotedByEviction { evicted: 6 });
    // Now 1, 2 (A, level 1) and 20, 21 (C, level 9): the lowest level goes, lowest id first.
    let (g, _) = ask(&mut p, C, 22, 502);
    assert_eq!(g, Grant::PromotedByEviction { evicted: 1 });

    // Same level: fewer interested strategies goes first.
    let mut p = promoter(2, 0);
    p.set_priority(B, 3);
    p.set_priority(D, 3);
    p.set_priority(E, 9);
    ask(&mut p, B, 10, 10); // level 3, one claimant
    ask(&mut p, B, 11, 10);
    ask(&mut p, D, 11, 11); // level 3, two claimants
    assert_eq!(
        ask(&mut p, E, 12, 20).0,
        Grant::PromotedByEviction { evicted: 10 }
    );
    // Level beats the number of claimants: a symbol one strategy of priority 9 wants outlasts one two
    // of priority 3 want. And the number beats age: of two at the same level, the one with fewer
    // claimants goes even though it is the newer.
    let mut q = promoter(3, 0);
    q.set_priority(B, 3);
    q.set_priority(D, 3);
    q.set_priority(E, 9);
    q.set_priority(C, 20);
    ask(&mut q, E, 20, 10); // level 9, one claimant
    ask(&mut q, B, 21, 11);
    ask(&mut q, D, 21, 11); // level 3, two claimants, older
    ask(&mut q, B, 22, 50); // level 3, one claimant, newer
    assert_eq!(
        ask(&mut q, C, 23, 60).0,
        Grant::PromotedByEviction { evicted: 22 }
    );
    assert_eq!(
        ask(&mut q, C, 24, 61).0,
        Grant::PromotedByEviction { evicted: 21 }
    );
    assert_eq!(
        ask(&mut q, C, 25, 62).0,
        Grant::PromotedByEviction { evicted: 20 }
    );
    // A request of priority exactly the level does not evict, one above does.
    assert_eq!(ask(&mut p, B, 13, 21).0, Grant::Denied(Denied::Full));
    p.set_priority(A, 4);
    assert_eq!(
        ask(&mut p, A, 13, 22).0,
        Grant::PromotedByEviction { evicted: 11 }
    );
    assert_eq!(p.drain_revoked(), [(B, 10), (B, 11), (D, 11)]);
}

#[test]
fn a_symbol_dwells_before_it_can_be_evicted() {
    let mut p = promoter(1, 60);
    p.set_priority(C, 9);
    ask(&mut p, A, 1, 100);
    assert_eq!(ask(&mut p, C, 2, 159).0, Grant::Denied(Denied::Full));
    assert_eq!(
        ask(&mut p, C, 2, 160).0,
        Grant::PromotedByEviction { evicted: 1 }
    );
}

#[test]
fn a_held_symbol_is_never_evicted_whatever_the_priority() {
    let mut p = promoter(2, 0);
    p.set_priority(C, 255);
    ask(&mut p, A, 1, 10);
    ask(&mut p, A, 2, 10);
    p.pin(A, 1);
    p.pin(B, 2);
    assert_eq!(ask(&mut p, C, 3, 20).0, Grant::Denied(Denied::Full));
    assert_eq!(p.promoted(), [1, 2]);
    // One holder letting go does not free a symbol another still holds.
    p.unpin(A, 2);
    assert!(p.is_pinned(2) && p.is_pinned_by(B, 2) && !p.is_pinned_by(A, 2));
    assert_eq!(ask(&mut p, C, 3, 21).0, Grant::Denied(Denied::Full));
    p.unpin(B, 2);
    assert!(!p.is_pinned(2));
    assert_eq!(
        ask(&mut p, C, 3, 22).0,
        Grant::PromotedByEviction { evicted: 2 }
    );
    // A hold on a symbol that is not in Tier 1 yet still counts, and an unknown id is ignored.
    p.pin(A, 40);
    p.pin(A, 9999);
    assert!(p.is_pinned(40) && !p.is_pinned(9999));
    // Unpinning something never pinned does nothing.
    p.unpin(D, 40);
    assert!(p.is_pinned(40));
}

#[test]
fn claimed_symbols_survive_the_cooldown_and_go_when_released() {
    let mut p = promoter(5, 0);
    let mut t0 = Tier0::new(64);
    ask(&mut p, A, 1, 0);
    ask(&mut p, A, 2, 0);
    ask(&mut p, B, 3, 0);
    p.pin(B, 3);
    p.release(B, 3);
    // Feed an unrelated symbol every second so the sweep runs; nothing here makes 1, 2 or 3 hot.
    let mut demoted: Vec<(u32, u64)> = Vec::new();
    for s in 1..=400u64 {
        if s == 200 {
            p.release(A, 1);
            p.unpin(B, 3);
        }
        let ts = T0 + s * SEC;
        let ev = Event::Trade(Trade {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: s,
                instrument: 30,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(1000),
            size: 1,
            flags: TradeFlags::NONE,
        });
        t0.on_event(&ev);
        let mut out = Vec::new();
        p.on_event(&t0, &ev, &mut out);
        for c in out {
            assert_eq!(
                (c.action, c.reason),
                (TierAction::Demote, tier_reason::COOLED_OFF)
            );
            demoted.push((c.hdr.instrument, s));
        }
    }
    // 1 (released) and 3 (hold and interest both gone) cool off at once; 2 is still wanted by A.
    assert_eq!(demoted.iter().map(|d| d.0).collect::<Vec<_>>(), [1, 3]);
    assert!(
        demoted.iter().all(|d| (200..=202).contains(&d.1)),
        "{demoted:?}"
    );
    assert!(p.is_promoted(2) && p.is_wanted_by(A, 2));
    assert!(
        p.drain_revoked().is_empty(),
        "released interests are not revocations"
    );
}

#[test]
fn a_follower_decides_nothing_but_keeps_the_books() {
    let mut f = Promoter::follower(3, 64);
    assert_eq!(ask(&mut f, A, 5, 10).0, Grant::Denied(Denied::Following));
    assert_eq!(f.owner_stats()[0].1.denied_other, 1);
    // The tape promotes it; now the request is granted without a change of its own.
    let tape = TierChange {
        hdr: Header {
            ts_event: T0,
            ts_recv: T0,
            seq: 0,
            instrument: 5,
            provider: ProviderId::Internal,
        },
        action: TierAction::Promote,
        reason: tier_reason::STRATEGY_REQUEST,
        score: 0,
    };
    f.apply(&tape).unwrap();
    assert_eq!(ask(&mut f, A, 5, 11), (Grant::Already, vec![]));
    f.pin(A, 5);
    // The tape demotes it: the interest is revoked, the hold is kept for the strategy to release.
    let down = TierChange {
        action: TierAction::Demote,
        reason: tier_reason::EVICTED,
        hdr: Header { seq: 1, ..tape.hdr },
        ..tape
    };
    f.apply(&down).unwrap();
    assert_eq!(f.drain_revoked(), [(A, 5)]);
    assert!(!f.is_wanted_by(A, 5) && f.is_pinned_by(A, 5));
    assert_eq!(f.apply(&down), Err(TierError::NotPromoted));
}

#[test]
fn a_follower_fed_the_tape_of_requests_ends_in_the_same_membership() {
    let mut live = promoter(3, 0);
    live.set_priority(C, 9);
    let mut tape = Vec::new();
    let script: [(u16, u32, u64); 8] = [
        (A, 1, 10),
        (A, 2, 11),
        (B, 3, 12),
        (B, 4, 13),
        (C, 4, 14),
        (C, 5, 15),
        (A, 6, 16),
        (C, 7, 17),
    ];
    for (who, id, s) in script {
        let (_, out) = ask(&mut live, who, id, s);
        tape.extend(out);
    }
    assert!(tape.len() >= 5);
    let mut follower = Promoter::follower(3, 64);
    for c in &tape {
        follower.apply(c).unwrap();
    }
    assert_eq!(follower.promoted(), live.promoted());
}

#[test]
fn the_metrics_text_has_a_line_for_every_count() {
    let mut p = promoter(1, 0);
    ask(&mut p, A, 1, 10);
    ask(&mut p, B, 2, 11);
    let m = p.metrics_text();
    for line in [
        "tier1_symbols 1",
        "tier1_capacity 1",
        "tier1_scanner_refused_full 0",
        "tier1_requests{strategy=\"1\"} 1",
        "tier1_promoted{strategy=\"1\"} 1",
        "tier1_denied_full{strategy=\"2\"} 1",
        "tier1_requests{strategy=\"2\"} 1",
        "tier1_lost{strategy=\"1\"} 0",
    ] {
        assert!(m.lines().any(|l| l == line), "{line}\n{m}");
    }
}
