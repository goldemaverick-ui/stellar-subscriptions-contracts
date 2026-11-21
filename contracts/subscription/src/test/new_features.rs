//! Tests for issue #53 (free trial), #54 (merchant end), and new view functions.

use soroban_sdk::{testutils::{Address as _, Events as _}, Address, Event};

use super::setup::{expect_err, Setup, AMOUNT, CAP, INTERVAL};
use crate::{events::MerchantEnded, Error, SubStatus};

#[test]
fn merchant_end_emits_merchant_ended_with_correct_fields() {
    let s = Setup::new();
    let id = s.subscribe();

    // Charge once so total_charged is non-zero and distinguishable from 0.
    s.client.charge(&s.merchant, &id);
    let total_charged = s.client.get_subscription(&id).total_charged;
    assert_eq!(total_charged, AMOUNT);

    s.client.merchant_end(&s.merchant, &id);

    let expected = MerchantEnded {
        subscription_id: id,
        merchant: s.merchant.clone(),
        total_charged,
    };
    assert_eq!(
        s.env.events().all().filter_by_contract(&s.client.address),
        [expected.to_xdr(&s.env, &s.client.address)]
    );
}

#[test]
fn trial_delays_first_charge() {
    let s = Setup::new();
    let trial_ledgers = 500u32;

    // Subscribe with trial.
    let id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &trial_ledgers,
    );

    // Check subscription records the trial.
    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.trial_ledgers, trial_ledgers);
    assert_eq!(sub.next_charge_ledger, s.ledger() + trial_ledgers);

    // First charge is not available immediately.
    assert!(!s.client.is_chargeable(&id));

    // Advancing partway through trial: still not chargeable.
    s.advance(trial_ledgers - 100);
    assert!(!s.client.is_chargeable(&id));

    // Advancing past trial: first charge is available.
    s.advance(100);
    assert!(s.client.is_chargeable(&id));

    // Charge succeeds.
    s.client.charge(&s.merchant, &id);
    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.total_charged, AMOUNT);
    assert_eq!(sub.last_charge_ledger, s.ledger());
}

#[test]
fn trial_zero_allows_immediate_charge() {
    let s = Setup::new();

    // Subscribe with no trial (0 ledgers).
    let id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &0,
    );

    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.trial_ledgers, 0);
    assert_eq!(sub.next_charge_ledger, s.ledger());

    // First charge is available immediately.
    assert!(s.client.is_chargeable(&id));
}

#[test]
fn trial_affects_only_first_charge() {
    let s = Setup::new();
    let trial_ledgers = 200u32;

    let id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &trial_ledgers,
    );

    // Wait until trial ends.
    s.advance(trial_ledgers);

    // Charge succeeds.
    s.client.charge(&s.merchant, &id);

    // Next charge follows normal interval, not trial+interval.
    s.advance(INTERVAL / 2);
    assert!(!s.client.is_chargeable(&id));
    s.advance(INTERVAL / 2);
    assert!(s.client.is_chargeable(&id));
}

#[test]
fn merchant_end_prevents_future_charges() {
    let s = Setup::new();
    let id = s.subscribe();

    // Charge once.
    s.client.charge(&s.merchant, &id);
    assert_eq!(s.client.get_subscription(&id).total_charged, AMOUNT);

    // Merchant ends the subscription.
    s.client.merchant_end(&s.merchant, &id);

    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.status, SubStatus::MerchantEnded);

    // Advance to when the next charge would normally be due.
    s.advance(INTERVAL);

    // Charge now fails.
    expect_err(
        s.client.try_charge(&s.merchant, &id),
        Error::SubscriptionMerchantEnded,
    );
}

#[test]
fn merchant_end_can_only_be_called_by_merchant() {
    let s = Setup::new();
    let id = s.subscribe();

    let other = Address::generate(&s.env);
    expect_err(
        s.client.try_merchant_end(&other, &id),
        Error::NotMerchant,
    );
}

#[test]
fn merchant_end_fails_on_cancelled_subscription() {
    let s = Setup::new();
    let id = s.subscribe();

    // Cancel the subscription.
    s.client.cancel(&s.subscriber, &id);
    assert_eq!(s.client.get_subscription(&id).status, SubStatus::Cancelled);

    // Merchant cannot end a cancelled subscription.
    expect_err(
        s.client.try_merchant_end(&s.merchant, &id),
        Error::SubscriptionCancelled,
    );
}

#[test]
fn merchant_end_fails_on_exhausted_subscription() {
    let s = Setup::new();
    let id = s.subscribe();

    // Charge until the cap is reached, exhausting the subscription.
    while s.client.is_chargeable(&id) {
        s.client.charge(&s.merchant, &id);
        s.advance(INTERVAL);
    }
    assert_eq!(s.client.get_subscription(&id).status, SubStatus::Exhausted);

    // Merchant cannot end an exhausted subscription.
    expect_err(
        s.client.try_merchant_end(&s.merchant, &id),
        Error::SubscriptionExhausted,
    );
}

#[test]
fn merchant_end_fails_on_already_merchant_ended_subscription() {
    let s = Setup::new();
    let id = s.subscribe();

    // Merchant ends the subscription.
    s.client.merchant_end(&s.merchant, &id);
    assert_eq!(
        s.client.get_subscription(&id).status,
        SubStatus::MerchantEnded
    );

    // Merchant cannot end an already merchant-ended subscription.
    expect_err(
        s.client.try_merchant_end(&s.merchant, &id),
        Error::SubscriptionMerchantEnded,
    );
}

#[test]
fn merchant_end_prevents_subscriber_pause_and_resume() {
    let s = Setup::new();
    let id = s.subscribe();

    // Merchant ends the subscription.
    s.client.merchant_end(&s.merchant, &id);

    // Subscriber cannot pause a merchant-ended subscription.
    expect_err(
        s.client.try_pause(&s.subscriber, &id),
        Error::SubscriptionMerchantEnded,
    );

    // Subscriber cannot resume either (and it's not paused anyway).
    expect_err(
        s.client.try_resume(&s.subscriber, &id),
        Error::SubscriptionMerchantEnded,
    );
}

#[test]
fn merchant_end_prevents_subscriber_cancel() {
    let s = Setup::new();
    let id = s.subscribe();

    // Merchant ends the subscription.
    s.client.merchant_end(&s.merchant, &id);

    // Subscriber cannot cancel a merchant-ended subscription.
    expect_err(
        s.client.try_cancel(&s.subscriber, &id),
        Error::SubscriptionMerchantEnded,
    );
}

#[test]
fn merchant_end_updates_registry() {
    let s = Setup::new();
    let id = s.subscribe();

    // Charge to have some total_charged.
    s.client.charge(&s.merchant, &id);

    // Merchant ends.
    s.client.merchant_end(&s.merchant, &id);

    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.status, SubStatus::MerchantEnded);
}

#[test]
fn paused_subscription_can_be_merchant_ended() {
    let s = Setup::new();
    let id = s.subscribe();

    // Subscriber pauses.
    s.client.pause(&s.subscriber, &id);
    assert_eq!(s.client.get_subscription(&id).status, SubStatus::Paused);

    // Merchant can still end a paused subscription.
    s.client.merchant_end(&s.merchant, &id);

    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.status, SubStatus::MerchantEnded);

    // Advance and verify charge still fails.
    s.advance(INTERVAL);
    expect_err(
        s.client.try_charge(&s.merchant, &id),
        Error::SubscriptionMerchantEnded,
    );
}

#[test]
fn invariant_trial_with_cap() {
    // Verify CAP invariant holds with trials.
    let s = Setup::new();
    let id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &500,
    );

    // Wait for trial to end.
    s.advance(500);

    // Charge the full cap.
    while s.client.is_chargeable(&id) {
        s.client.charge(&s.merchant, &id);
        s.advance(INTERVAL);
    }

    let sub = s.client.get_subscription(&id);
    assert_eq!(sub.total_charged, CAP);
    assert_eq!(sub.status, SubStatus::Exhausted);
}

#[test]
fn subscribe_simple_matches_subscribe_with_zero_trial() {
    let s = Setup::new();

    // Subscribe via the backward-compatible overload (no trial parameter).
    let simple_id = s.client.subscribe_simple(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
    );

    // Subscribe via the full signature with trial_ledgers = 0.
    let full_id = s.client.subscribe(
        &s.subscriber,
        &s.merchant,
        &s.token.address,
        &AMOUNT,
        &INTERVAL,
        &CAP,
        &0,
        &0,
    );

    let simple_sub = s.client.get_subscription(&simple_id);
    let full_sub = s.client.get_subscription(&full_id);

    // Both records must be identical.
    assert_eq!(simple_sub.trial_ledgers, 0);
    assert_eq!(simple_sub.trial_ledgers, full_sub.trial_ledgers);
    assert_eq!(simple_sub.next_charge_ledger, s.ledger());
    assert_eq!(simple_sub.next_charge_ledger, full_sub.next_charge_ledger);
    assert_eq!(simple_sub.status, SubStatus::Active);
    assert_eq!(simple_sub.status, full_sub.status);
    assert_eq!(simple_sub.amount, full_sub.amount);
    assert_eq!(simple_sub.interval_ledgers, full_sub.interval_ledgers);
    assert_eq!(simple_sub.total_cap, full_sub.total_cap);
    assert_eq!(simple_sub.total_charged, full_sub.total_charged);
    assert_eq!(simple_sub.plan_id, full_sub.plan_id);

    // Both subscriptions are immediately chargeable.
    assert!(s.client.is_chargeable(&simple_id));
    assert!(s.client.is_chargeable(&full_id));

    // Charging the simple subscription behaves like the full one.
    s.client.charge(&s.merchant, &simple_id);
    s.client.charge(&s.merchant, &full_id);
    let simple_sub = s.client.get_subscription(&simple_id);
    let full_sub = s.client.get_subscription(&full_id);
    assert_eq!(simple_sub.total_charged, AMOUNT);
    assert_eq!(simple_sub.total_charged, full_sub.total_charged);
    assert_eq!(simple_sub.next_charge_ledger, full_sub.next_charge_ledger);
}

#[test]
fn invariant_merchant_end_with_multiple_subscriptions() {
    // Verify merchant_end affects only one subscription.
    let s = Setup::new();
    let id1 = s.subscribe();
    let id2 = s.subscribe();

    // Merchant auth is already mocked, so just call merchant_end
    s.client.merchant_end(&s.merchant, &id1);

    // id1 is ended, id2 is still active.
    assert_eq!(
        s.client.get_subscription(&id1).status,
        SubStatus::MerchantEnded
    );
    assert_eq!(s.client.get_subscription(&id2).status, SubStatus::Active);

    // id2 can still be charged after its interval.
    let before_charge = s.ledger();
    assert!(s.client.is_chargeable(&id2));
    s.client.charge(&s.merchant, &id2);
    // After charge, next charge should be at before_charge + INTERVAL
    assert_eq!(s.client.get_subscription(&id2).next_charge_ledger, before_charge + INTERVAL);
}
