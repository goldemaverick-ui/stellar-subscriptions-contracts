//! subscribe, charge, cancel, pause and resume each emit exactly one event.
//! Token events (approve, transfer) are filtered out by contract address.

use soroban_sdk::{
    testutils::{Address as _, Events as _},
    token::{StellarAssetClient, TokenClient},
    Address, Event,
};

use super::setup::{expect_err, Setup, AMOUNT, CAP, INTERVAL};
use crate::{
    events::{AllowanceRefreshed, Cancelled, Charged, Paused, Resumed, Subscribed},
    Error,
};

/// The last invocation emitted exactly `expected` from this contract.
fn assert_only_event<E: Event>(s: &Setup, expected: E) {
    assert_eq!(
        s.env.events().all().filter_by_contract(&s.client.address),
        [expected.to_xdr(&s.env, &s.client.address)]
    );
}

#[test]
fn subscribe_emits_subscribed() {
    let s = Setup::new();
    let id = s.subscribe();
    assert_only_event(
        &s,
        Subscribed {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            merchant: s.merchant.clone(),
            token: s.token.address.clone(),
            amount_per_period: AMOUNT,
            interval_ledgers: INTERVAL,
            total_cap: CAP,
            plan_id: 0,
            trial_ledgers: 0,
            expires_at: 0,
        },
    );
}

#[test]
fn subscribe_with_trial_emits_trial_and_expiry() {
    let s = Setup::new();
    let trial = 10u32;
    let now = s.ledger();
    let id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &trial,
        &0,
    );
    assert_only_event(
        &s,
        Subscribed {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            merchant: s.merchant.clone(),
            token: s.token.address.clone(),
            amount_per_period: AMOUNT,
            interval_ledgers: INTERVAL,
            total_cap: CAP,
            plan_id: 0,
            trial_ledgers: trial,
            expires_at: now + trial,
            expires_at: 0,
        },
    );
}

#[test]
fn charge_emits_charged() {
    let s = Setup::new();
    let id = s.subscribe();
    let now = s.ledger();
    s.client.charge(&s.merchant, &id);
    assert_only_event(
        &s,
        Charged {
            subscription_id: id,
            merchant: s.merchant.clone(),
            token: s.token.address.clone(),
            amount: AMOUNT,
            total_charged: AMOUNT,
            next_charge_ledger: now + INTERVAL,
            exhausted: false,
        },
    );
}

#[test]
fn the_charge_that_reaches_the_cap_reports_exhaustion() {
    let s = Setup::new();
    let id = s.subscribe_with(AMOUNT, INTERVAL, AMOUNT);
    let now = s.ledger();
    s.client.charge(&s.merchant, &id);
    assert_only_event(
        &s,
        Charged {
            subscription_id: id,
            merchant: s.merchant.clone(),
            token: s.token.address.clone(),
            amount: AMOUNT,
            total_charged: AMOUNT,
            next_charge_ledger: now + INTERVAL,
            exhausted: true,
        },
    );
}

#[test]
fn charged_event_token_matches_the_subscriptions_token() {
    let s = Setup::new();
    let id = s.subscribe();
    let now = s.ledger();
    s.client.charge(&s.merchant, &id);

    let sub = s.sub(id);
    assert_eq!(sub.token, s.token.address);

fn exhausted_is_false_until_the_final_charge() {
    let s = Setup::new();
    // Cap = 3 periods, so the third charge exactly exhausts it.
    let id = s.subscribe_with(AMOUNT, INTERVAL, 3 * AMOUNT);

    for _ in 0..2 {
        let now = s.ledger();
        s.client.charge(&s.merchant, &id);
        assert_only_event(
            &s,
            Charged {
                subscription_id: id,
                merchant: s.merchant.clone(),
                token: s.token.address.clone(),
                amount: AMOUNT,
                total_charged: s.sub(id).total_charged,
                next_charge_ledger: now + INTERVAL,
                exhausted: false,
            },
        );
        s.advance(INTERVAL);
    }

    let now = s.ledger();
    s.client.charge(&s.merchant, &id);
    assert_only_event(
        &s,
        Charged {
            subscription_id: id,
            merchant: s.merchant.clone(),
            token: sub.token.clone(),
            amount: AMOUNT,
            total_charged: AMOUNT,
            next_charge_ledger: now + INTERVAL,
            exhausted: false,
        },
    );
            token: s.token.address.clone(),
            amount: AMOUNT,
            total_charged: 3 * AMOUNT,
            next_charge_ledger: now + INTERVAL,
            exhausted: true,
        },
    );
    assert_eq!(s.sub(id).status, crate::SubStatus::Exhausted);
}

#[test]
fn cancel_emits_cancelled_with_the_unused_cap() {
    let s = Setup::new();
    let id = s.subscribe();
    s.client.charge(&s.merchant, &id);
    s.client.cancel(&s.subscriber, &id);
    assert_only_event(
        &s,
        Cancelled {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            total_charged: AMOUNT,
            unused_cap: CAP - AMOUNT,
            allowance_updated: true,
            registry_updated: true,
        },
    );
}

#[test]
fn cancel_reports_the_exact_unused_cap_after_a_known_number_of_charges() {
    let s = Setup::new();
    let id = s.subscribe_with(60, INTERVAL, 300);
    s.client.charge(&s.merchant, &id);
    s.advance(INTERVAL);
    s.client.charge(&s.merchant, &id);
    s.client.cancel(&s.subscriber, &id);
    assert_only_event(
        &s,
        Cancelled {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            total_charged: 120,
            unused_cap: 180,
            allowance_updated: true,
            registry_updated: true,
        },
    );
}

#[test]
fn cancel_with_no_charges_reports_the_full_cap_as_unused() {
    let s = Setup::new();
    let id = s.subscribe_with(AMOUNT, INTERVAL, 100);
    s.client.cancel(&s.subscriber, &id);
    assert_only_event(
        &s,
        Cancelled {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            total_charged: 0,
            unused_cap: 100,
            allowance_updated: true,
            registry_updated: true,
        },
    );
}

#[test]
fn resumed_event_reports_now_when_resumed_after_the_due_date_has_passed() {
    let s = Setup::new();
    let id = s.subscribe();
    s.client.pause(&s.subscriber, &id);
    // Original next_charge_ledger is start + INTERVAL. Resume well after it.
    s.set_ledger(s.sub(id).next_charge_ledger + 10);
    let resumed_at = s.ledger();
    s.client.resume(&s.subscriber, &id);
    assert_only_event(
        &s,
        Resumed {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            next_charge_ledger: resumed_at,
        },
    );
}

#[test]
fn resumed_event_keeps_the_original_due_date_when_resumed_early() {
    let s = Setup::new();
    let id = s.subscribe();
    let original_next = s.sub(id).next_charge_ledger;
    s.client.pause(&s.subscriber, &id);
    // Resume before the original due date.
    s.set_ledger(original_next - 10);
    s.client.resume(&s.subscriber, &id);
    assert_only_event(
        &s,
        Resumed {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            next_charge_ledger: original_next,
        },
    );
}

#[test]
fn pause_emits_paused() {
    let s = Setup::new();
    let id = s.subscribe();
    let now = s.ledger();
    s.client.pause(&s.subscriber, &id);
    assert_only_event(
        &s,
        Paused {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            ledger: now,
        },
    );
}

#[test]
fn resume_emits_resumed_with_the_next_charge_ledger() {
    let s = Setup::new();
    let id = s.subscribe();
    s.client.pause(&s.subscriber, &id);
    s.advance(10);
    let now = s.ledger();
    s.client.resume(&s.subscriber, &id);
    // The first charge fell due while paused, so it is available now.
    assert_only_event(
        &s,
        Resumed {
            subscription_id: id,
            subscriber: s.subscriber.clone(),
            next_charge_ledger: now,
        },
    );
}

#[test]
fn refresh_allowance_emits_allowance_refreshed_with_the_outstanding_cap() {
    let s = Setup::new();
    s.subscribe();
    s.client.refresh_allowance(&s.subscriber, &s.token.address);
    assert_only_event(
        &s,
        AllowanceRefreshed {
            subscriber: s.subscriber.clone(),
            token: s.token.address.clone(),
            amount: CAP,
        },
    );
}

#[test]
fn refresh_allowance_amount_is_the_sum_of_remaining_caps_of_non_final_subscriptions() {
    let s = Setup::new();

    // Exhausted: one charge of the full cap leaves nothing remaining.
    let exhausted = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &100,
        &INTERVAL,
        &100,
        &0,
        &0,
    );
    s.client.charge(&s.merchant, &exhausted);

    // Cancelled before any charge: its remaining cap must not count either.
    let cancelled = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &200,
        &INTERVAL,
        &200,
        &0,
        &0,
    );
    s.client.cancel(&s.subscriber, &cancelled);

    // Active, untouched: its full cap is still outstanding.
    let active = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &300,
        &INTERVAL,
        &300,
        &0,
        &0,
    );
    let _ = active;

    s.client.refresh_allowance(&s.subscriber, &s.token.address);
    assert_only_event(
        &s,
        AllowanceRefreshed {
            subscriber: s.subscriber.clone(),
            token: s.token.address.clone(),
            amount: 300,
        },
    );
}

#[test]
fn refresh_allowance_amount_only_counts_subscriptions_in_the_requested_token() {
    let s = Setup::new();

    // Active subscription in the primary token: its cap is outstanding.
    s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &0,
    );

    // A second token, with its own active subscription for the same
    // subscriber. Its cap must be excluded when refreshing the first token.
    let admin2 = Address::generate(&s.env);
    let sac2 = s.env.register_stellar_asset_contract_v2(admin2.clone());
    let token2 = TokenClient::new(&s.env, &sac2.address());
    StellarAssetClient::new(&s.env, &token2.address).mint(&s.subscriber, &1_000_000);
    s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &token2.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &0,
    );

    s.client.refresh_allowance(&s.subscriber, &s.token.address);
    assert_only_event(
        &s,
        AllowanceRefreshed {
            subscriber: s.subscriber.clone(),
            token: s.token.address.clone(),
            amount: CAP,
        },
    );
}

#[test]
fn a_rejected_charge_emits_nothing() {
    let s = Setup::new();
    let id = s.subscribe();
    s.client.charge(&s.merchant, &id);
    expect_err(
        s.client.try_charge(&s.merchant, &id),
        Error::IntervalNotElapsed,
    );
    assert!(s
        .env
        .events()
        .all()
        .filter_by_contract(&s.client.address)
        .events()
        .is_empty());
}
