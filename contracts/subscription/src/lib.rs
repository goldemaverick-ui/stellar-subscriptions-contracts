#![no_std]
//! # Subscription Contract
//!
//! Recurring pull payments on Soroban. A subscriber grants a bounded,
//! revocable authorization once; the merchant then pulls a fixed amount per
//! billing interval.

mod allowance;
mod errors;
mod events;
mod interfaces;
mod storage;
mod types;

pub use errors::Error;
pub use types::{SubStatus, Subscription, SubscriptionSummary};
pub use types::{ChargeStatus, SubStatus, Subscription};

use events::{Cancelled, Charged, Paused, PlanAssociated, Resumed, Subscribed};
use events::{Cancelled, Charged, MerchantEnded, Paused, Resumed, Subscribed};
use interfaces::{PlanClient, RegistryClient};
use soroban_sdk::{contract, contractimpl, token::TokenClient, Address, Env, Vec};
use storage::DataKey;
use types::SubscriptionSummary;

/// Most subscriptions one `batch_charge` call may charge, so a batch stays
/// within the per-transaction write budget.
pub const MAX_BATCH: u32 = 20;

/// Most entries `get_by_subscriber` / `get_by_merchant` may return in one
/// call, so a large history cannot exceed the transaction budget.
pub const MAX_LIST_SIZE: u32 = 200;

#[contract]
pub struct SubscriptionContract;

#[contractimpl]
impl SubscriptionContract {
    /// Set the admin. Callable once.
    ///
    /// The admin can only link the plan and registry contracts, once each,
    /// before any subscription exists. It has no power to charge, cancel,
    /// pause or resume anything.
    pub fn initialize(env: Env, admin: Address) -> Result<(), Error> {
        if storage::has_admin(&env) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        storage::write_admin(&env, &admin);
        storage::bump_instance(&env);
        Ok(())
    }

    /// Admin links the plan contract that non-zero `plan_id`s are checked
    /// against. Once only, and only before the first subscription, so the
    /// terms a subscriber was checked against can never be swapped later.
    pub fn set_plan_contract(
        env: Env,
        admin: Address,
        plan_contract: Address,
    ) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;
        if storage::read_plan_contract(&env).is_some() || storage::has_subscriptions(&env) {
            return Err(Error::AlreadyConfigured);
        }
        storage::write_plan_contract(&env, &plan_contract);
        storage::bump_instance(&env);
        Ok(())
    }

    /// Admin links the registry every lifecycle change is reported to. Once
    /// only, and only before the first subscription, so the registry never
    /// misses one.
    pub fn set_registry(env: Env, admin: Address, registry: Address) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;
        if storage::read_registry(&env).is_some() || storage::has_subscriptions(&env) {
            return Err(Error::AlreadyConfigured);
        }
        storage::write_registry(&env, &registry);
        storage::bump_instance(&env);
        Ok(())
    }

    pub fn get_plan_contract(env: Env) -> Option<Address> {
        storage::read_plan_contract(&env)
    }

    pub fn get_registry(env: Env) -> Option<Address> {
        storage::read_registry(&env)
    }

    /// Get the admin address.
    pub fn get_admin(env: Env) -> Result<Address, Error> {
        storage::read_admin(&env)
    /// Backward-compatible entry point for callers that do not need trials.
    ///
    /// Delegates to [`subscribe`](Self::subscribe) with `trial_ledgers = 0`,
    /// so the first charge is available immediately. Use this from scripts or
    /// clients compiled against the pre-trial ABI.
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_simple(
        env: Env,
        subscriber: Address,
        merchant: Address,
        token: Address,
        amount_per_period: i128,
        interval_ledgers: u32,
        total_cap: i128,
        plan_id: u64,
    ) -> Result<u64, Error> {
        Self::subscribe(
            env,
            subscriber,
            merchant,
            token,
            amount_per_period,
            interval_ledgers,
            total_cap,
            plan_id,
            0,
        )
    }

    /// Subscriber authorizes a subscription. This is where consent is granted
    /// and bounded: the merchant may later pull `amount_per_period` once per
    /// `interval_ledgers`, never more than `total_cap` in total.
    ///
    /// A non-zero `plan_id` must name an active plan whose merchant, token,
    /// amount and interval match exactly; 0 creates an ad-hoc subscription.
    /// The first charge is available immediately. Also approves this contract
    /// to spend the subscriber's outstanding cap in `token`.
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe(
        env: Env,
        subscriber: Address,
        merchant: Address,
        token: Address,
        amount_per_period: i128,
        interval_ledgers: u32,
        total_cap: i128,
        plan_id: u64,
        trial_ledgers: u32,
    ) -> Result<u64, Error> {
        Self::subscribe_inner(
            &env,
            subscriber,
            merchant,
            token,
            amount_per_period,
            interval_ledgers,
            total_cap,
            plan_id,
            0,
        )
    }

    /// Same as `subscribe`, but the subscription expires at `expires_at`
    /// (a ledger sequence number). Once the current ledger is past
    /// `expires_at`, `charge` fails with `SubscriptionExpired`.
    ///
    /// `expires_at` must be strictly in the future; 0 means no expiry.
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_until(
        env: Env,
        subscriber: Address,
        merchant: Address,
        token: Address,
        amount_per_period: i128,
        interval_ledgers: u32,
        total_cap: i128,
        plan_id: u64,
        expires_at: u32,
    ) -> Result<u64, Error> {
        if expires_at != 0 && expires_at <= env.ledger().sequence() {
            return Err(Error::InvalidExpiry);
        }
        Self::subscribe_inner(
            &env,
            subscriber,
            merchant,
            token,
            amount_per_period,
            interval_ledgers,
            total_cap,
            plan_id,
            expires_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn subscribe_inner(
        env: &Env,
        subscriber: Address,
        merchant: Address,
        token: Address,
        amount_per_period: i128,
        interval_ledgers: u32,
        total_cap: i128,
        plan_id: u64,
        expires_at: u32,
    ) -> Result<u64, Error> {
        storage::read_admin(env)?;
        subscriber.require_auth();

        if subscriber == merchant {
            return Err(Error::SameParty);
        }
        if amount_per_period <= 0 {
            return Err(Error::InvalidAmount);
        }
        if interval_ledgers == 0 {
            return Err(Error::InvalidInterval);
        }
        if total_cap < amount_per_period {
            return Err(Error::InvalidCap);
        }
        if plan_id != 0 {
            Self::check_plan(
                env,
                plan_id,
                &merchant,
                &token,
                amount_per_period,
                interval_ledgers,
            )?;
        }

        let id = storage::take_next_id(env)?;
        let now = env.ledger().sequence();
        let next_charge_ledger = now
            .checked_add(trial_ledgers)
            .ok_or(Error::Overflow)?;
        let trial_ledgers = 0u32;
        let expires_at = now.checked_add(trial_ledgers).ok_or(Error::Overflow)?;

        let id = storage::take_next_id(&env)?;
        let sub = Subscription {
            id,
            subscriber,
            merchant,
            token,
            amount_per_period,
            interval_ledgers,
            total_cap,
            total_charged: 0,
            start_ledger: now,
            last_charge_ledger: 0,
            next_charge_ledger: now + trial_ledgers,
            // A trial delays the first charge; without one, the first charge
            // is available immediately.
            next_charge_ledger: now.saturating_add(trial_ledgers),
            next_charge_ledger: now.saturating_add(trial_ledgers),
            next_charge_ledger,
            status: SubStatus::Active,
            plan_id,
            trial_ledgers,
        };
        storage::write_sub(&env, &sub);
        storage::push_index(&env, DataKey::BySubscriber(sub.subscriber.clone()), id);
        storage::push_index(&env, DataKey::ByMerchant(sub.merchant.clone()), id);
        if plan_id != 0 {
            storage::push_index(&env, DataKey::ByPlan(plan_id), id);
            next_charge_ledger: expires_at,
            status: SubStatus::Active,
            plan_id,
            expires_at,
        };
        storage::write_sub(env, &sub);
        storage::push_index(env, DataKey::BySubscriber(sub.subscriber.clone()), id);
        storage::push_index(env, DataKey::ByMerchant(sub.merchant.clone()), id);
        if plan_id != 0 {
            storage::push_index(env, DataKey::ByPlan(plan_id), id);
        }

        // After the write, so the new subscription's cap is included.
        allowance::sync(env, &sub.subscriber, &sub.token)?;

        if let Some(registry) = storage::read_registry(env) {
            let registered = RegistryClient::new(env, &registry).try_register_subscription(
                &id,
                &sub.subscriber,
                &sub.merchant,
                &sub.token,
            );
            if !matches!(registered, Ok(Ok(()))) {
                return Err(Error::RegistryUpdateFailed);
            }
        }

        // After all local state is written: bump the plan's subscriber count.
        // A failure rolls back the whole call, so no half-created
        // subscription is ever left behind.
        if plan_id != 0 {
            let plan_contract =
                storage::read_plan_contract(&env).ok_or(Error::PlanContractNotSet)?;
            let incremented =
                PlanClient::new(&env, &plan_contract).try_increment_subscriber_count(&plan_id);
            if !matches!(incremented, Ok(Ok(()))) {
                return Err(Error::PlanNotFound);
            }
        }

        storage::bump_instance(&env);
        storage::bump_instance(env);
        Subscribed {
            subscription_id: id,
            subscriber: sub.subscriber,
            merchant: sub.merchant,
            token: sub.token,
            amount_per_period,
            interval_ledgers,
            total_cap,
            plan_id,
            trial_ledgers,
            expires_at,
        }
        .publish(env);
        Ok(id)
    }

    /// Merchant pulls one period's charge. This is where all three
    /// invariants are enforced, in this order, before any funds move:
    ///
    /// - REVOCATION: only an `Active` subscription can be charged.
    /// - INTERVAL: the current ledger must be at or past
    ///   `next_charge_ledger`.
    /// - CAP: `total_charged + amount_per_period`, computed with
    ///   `checked_add`, must not exceed `total_cap`.
    ///
    /// On success `amount_per_period` moves subscriber -> merchant and the
    /// next charge is scheduled one full interval from now, so a late charge
    /// never unlocks a burst of catch-up charges.
    pub fn charge(env: Env, merchant: Address, subscription_id: u64) -> Result<(), Error> {
        merchant.require_auth();
        let mut sub = storage::read_sub(&env, subscription_id)?;

        // REVOCATION first, so an unrelated caller cannot learn whether a
        // subscription exists by comparing NotMerchant against a status error.
        match sub.status {
            SubStatus::Active => {}
            SubStatus::Paused => return Err(Error::SubscriptionPaused),
            SubStatus::Cancelled => return Err(Error::SubscriptionCancelled),
            SubStatus::Exhausted => return Err(Error::SubscriptionExhausted),
            SubStatus::MerchantEnded => return Err(Error::SubscriptionMerchantEnded),
        }

        // EXPIRY. Checked before the interval so an expired subscription
        // reports `SubscriptionExpired` even when a trial pushed
        // `next_charge_ledger` past `expires_at`, making the first charge
        // impossible. `expires_at == 0` means no expiry.
        if sub.merchant != merchant {
            return Err(Error::NotMerchant);
        }

        // EXPIRY.
        let now = env.ledger().sequence();
        if sub.expires_at != 0 && now > sub.expires_at {
            return Err(Error::SubscriptionExpired);
        }

        // INTERVAL.
        if now < sub.next_charge_ledger {
            return Err(Error::IntervalNotElapsed);
        }

        // CAP.
        let new_total = sub
            .total_charged
            .checked_add(sub.amount_per_period)
            .ok_or(Error::Overflow)?;
        if new_total > sub.total_cap {
            return Err(Error::CapExceeded);
        }
        let next_charge_ledger = now
            .checked_add(sub.interval_ledgers)
            .ok_or(Error::Overflow)?;

        sub.total_charged = new_total;
        sub.last_charge_ledger = now;
        sub.next_charge_ledger = next_charge_ledger;
        if new_total == sub.total_cap {
            sub.status = SubStatus::Exhausted;
        }
        storage::write_sub(&env, &sub);

        // Every check has passed. If the token refuses the transfer, the
        // error return rolls back the write above, so a failed pull never
        // counts against the cap or the schedule.
        let spender = env.current_contract_address();
        let transfer = TokenClient::new(&env, &sub.token).try_transfer_from(
            &spender,
            &sub.subscriber,
            &sub.merchant,
            &sub.amount_per_period,
        );
        if !matches!(transfer, Ok(Ok(()))) {
            return Err(Error::TransferFailed);
        }

        Self::report(&env, &sub)?;

        storage::bump_instance(&env);
        Charged {
            subscription_id,
            merchant: sub.merchant,
            token: sub.token.clone(),
            token: sub.token,
            amount: sub.amount_per_period,
            total_charged: sub.total_charged,
            next_charge_ledger: sub.next_charge_ledger,
            exhausted: sub.status == SubStatus::Exhausted,
        }
        .publish(&env);
        Ok(())
    }

    /// Charge several of the merchant's subscriptions in one transaction.
    ///
    /// Every id goes through the full `charge` checks (REVOCATION, INTERVAL,
    /// and CAP) in order. The batch is **all or nothing**: if any id fails,
    /// the error is returned immediately and the entire transaction is rolled
    /// back — no subscription is charged and no funds move.
    ///
    /// A repeated id demonstrates this clearly: the first occurrence charges
    /// and advances `next_charge_ledger`; the second then fails with
    /// [`Error::IntervalNotElapsed`], rolling back both. At most [`MAX_BATCH`]
    /// ids are accepted; passing more returns [`Error::BatchTooLarge`] before
    /// any charge is attempted. Exactly [`MAX_BATCH`] ids is valid.
    pub fn batch_charge(env: Env, merchant: Address, ids: Vec<u64>) -> Result<(), Error> {
        if ids.len() > MAX_BATCH {
            return Err(Error::BatchTooLarge);
        }
        for id in ids.iter() {
            Self::charge(env.clone(), merchant.clone(), id)?;
        }
        Ok(())
    }

    /// Charge as many of the merchant's subscriptions as possible, collecting
    /// per-id results instead of aborting on the first failure.
    ///
    /// Each id is charged independently through the full `charge` checks; a
    /// failed charge records its error in the returned vector and does not
    /// roll back the others. The result vector is positionally aligned with
    /// `ids`. At most `MAX_BATCH` ids are accepted; a larger batch returns a
    /// single `Err(BatchTooLarge)` and charges nothing.
    pub fn batch_charge_soft(
        env: Env,
        merchant: Address,
        ids: Vec<u64>,
    ) -> Vec<Result<(), Error>> {
        let mut results = Vec::new(&env);
        if ids.len() > MAX_BATCH {
            results.push_back(Err(Error::BatchTooLarge));
            return results;
        }
        for id in ids.iter() {
            results.push_back(Self::charge(env.clone(), merchant.clone(), id));
        }
        results
    }

    /// Subscriber cancels. Final: no further charge will ever succeed.
    ///
    /// Allowed from Active, Paused or MerchantEnded. The status change is what
    /// enforces REVOCATION — `charge` refuses anything that is not Active, and
    /// nothing moves a subscription out of Cancelled. A subscription the
    /// merchant already ended can still be cancelled so the subscriber can
    /// trigger the allowance clean-up. Lowering the token allowance and
    /// updating the registry happen afterwards as clean-up, and neither is
    /// ever allowed to block the cancellation itself.
    pub fn cancel(env: Env, subscriber: Address, subscription_id: u64) -> Result<(), Error> {
        let mut sub = Self::load_for_subscriber(&env, &subscriber, subscription_id)?;
        match sub.status {
            SubStatus::Active | SubStatus::Paused | SubStatus::MerchantEnded => {}
            SubStatus::Cancelled => return Err(Error::SubscriptionCancelled),
            SubStatus::Exhausted => return Err(Error::SubscriptionExhausted),
            SubStatus::MerchantEnded => return Err(Error::SubscriptionMerchantEnded),
        }

        sub.status = SubStatus::Cancelled;
        storage::write_sub(&env, &sub);

        // Drop the id from the plan index so `get_by_plan` and friends never
        // return a cancelled subscription. Best-effort: the status write above
        // already enforces REVOCATION, so a stale index entry can never allow
        // a charge.
        if sub.plan_id != 0 {
            storage::remove_index_value(&env, DataKey::ByPlan(sub.plan_id), sub.id);
        }

        // Cancellation must succeed even if the token or the registry refuses
        // the follow-up; each outcome is reported in the event instead.
        let allowance_updated = allowance::sync(&env, &sub.subscriber, &sub.token).is_ok();
        let registry_updated = Self::report(&env, &sub).is_ok();

        storage::bump_instance(&env);
        Cancelled {
            subscription_id,
            subscriber: sub.subscriber,
            total_charged: sub.total_charged,
            // Reporting only. total_charged <= total_cap always holds, and
            // saturating keeps this line from ever blocking a cancel.
            unused_cap: sub.total_cap.saturating_sub(sub.total_charged),
            allowance_updated,
            registry_updated,
        }
        .publish(&env);
        Ok(())
    }

    /// Convenience wrapper: cancel every one of `subscriber`'s subscriptions
    /// in `token` that is not already final (Cancelled or Exhausted).
    ///
    /// This was requested in issue #60 and closed without landing in
    /// `lib.rs`; it is implemented here for real rather than left as a
    /// client-side loop, since each `cancel` already re-authenticates and
    /// re-reads storage, so looping client-side would spend the same gas
    /// with no safety benefit and no single combined auth check.
    ///
    /// Final subscriptions in `token` are simply skipped, not errored, so
    /// callers do not need to filter their own list first. Returns the ids
    /// that were actually cancelled.
    pub fn cancel_all(env: Env, subscriber: Address, token: Address) -> Result<Vec<u64>, Error> {
        subscriber.require_auth();
        let ids = storage::read_index(&env, &storage::DataKey::BySubscriber(subscriber.clone()));
        let mut cancelled = Vec::new(&env);
        for id in ids.iter() {
            let sub = storage::read_sub(&env, id)?;
            if sub.token != token || sub.status.is_final() {
                continue;
            }
            Self::cancel(env.clone(), subscriber.clone(), id)?;
            cancelled.push_back(id);
        }
        Ok(cancelled)
    }

    /// Subscriber pauses charging. Only an Active subscription can be paused.
    /// The cap, the charges so far and the token allowance are untouched.
    pub fn pause(env: Env, subscriber: Address, subscription_id: u64) -> Result<(), Error> {
        let mut sub = Self::load_for_subscriber(&env, &subscriber, subscription_id)?;
        match sub.status {
            SubStatus::Active => {}
            SubStatus::Paused => return Err(Error::AlreadyPaused),
            SubStatus::Cancelled => return Err(Error::SubscriptionCancelled),
            SubStatus::Exhausted => return Err(Error::SubscriptionExhausted),
            SubStatus::MerchantEnded => return Err(Error::SubscriptionMerchantEnded),
        }

        sub.status = SubStatus::Paused;
        storage::write_sub(&env, &sub);
        Self::report(&env, &sub)?;
        storage::bump_instance(&env);
        Paused {
            subscription_id,
            subscriber,
            ledger: env.ledger().sequence(),
        }
        .publish(&env);
        Ok(())
    }

    /// Subscriber resumes a paused subscription.
    ///
    /// The schedule never moves earlier and paused periods are never billed:
    /// if the next charge was not yet due it stays where it was; if it fell
    /// due while paused, exactly one charge becomes available now.
    pub fn resume(env: Env, subscriber: Address, subscription_id: u64) -> Result<(), Error> {
        let mut sub = Self::load_for_subscriber(&env, &subscriber, subscription_id)?;
        match sub.status {
            SubStatus::Paused => {}
            SubStatus::Active => return Err(Error::NotPaused),
            SubStatus::Cancelled => return Err(Error::SubscriptionCancelled),
            SubStatus::Exhausted => return Err(Error::SubscriptionExhausted),
            SubStatus::MerchantEnded => return Err(Error::SubscriptionMerchantEnded),
        }

        let now = env.ledger().sequence();
        sub.status = SubStatus::Active;
        sub.next_charge_ledger = sub.next_charge_ledger.max(now);
        storage::write_sub(&env, &sub);
        Self::report(&env, &sub)?;
        storage::bump_instance(&env);
        Resumed {
            subscription_id,
            subscriber,
            next_charge_ledger: sub.next_charge_ledger,
        }
        .publish(&env);
        Ok(())
    }

    /// Subscriber retroactively links an ad-hoc subscription (`plan_id == 0`)
    /// to an existing plan, provided the plan's terms match the
    /// subscription exactly and the plan is still active.
    ///
    /// This only updates the subscription's bookkeeping (`plan_id` and the
    /// `ByPlan` index) — it changes none of the subscription's actual terms,
    /// since `check_plan` requires them to already match the plan.
    pub fn associate_plan(
        env: Env,
        subscriber: Address,
        subscription_id: u64,
        plan_id: u64,
    ) -> Result<(), Error> {
        let mut sub = Self::load_for_subscriber(&env, &subscriber, subscription_id)?;
        if plan_id == 0 {
            return Err(Error::PlanNotFound);
        }
        Self::check_plan(
            &env,
            plan_id,
            &sub.merchant,
            &sub.token,
            sub.amount_per_period,
            sub.interval_ledgers,
        )?;

        sub.plan_id = plan_id;
        storage::write_sub(&env, &sub);
        storage::push_index(&env, DataKey::ByPlan(plan_id), subscription_id);
        storage::bump_instance(&env);

        PlanAssociated {
            subscription_id,
            subscriber,
            plan_id,
    /// Merchant ends a subscription. Final: no further charges, pauses,
    /// resumes or subscriber cancellations, ever.
    ///
    /// Allowed from Active or Paused. Mirrors `cancel`, but is authorized
    /// and initiated by the merchant instead of the subscriber.
    pub fn merchant_end(env: Env, merchant: Address, subscription_id: u64) -> Result<(), Error> {
        merchant.require_auth();
        let mut sub = storage::read_sub(&env, subscription_id)?;
        if sub.merchant != merchant {
            return Err(Error::NotMerchant);
        }
        match sub.status {
            SubStatus::Active | SubStatus::Paused => {}
            SubStatus::Cancelled => return Err(Error::SubscriptionCancelled),
            SubStatus::Exhausted => return Err(Error::SubscriptionExhausted),
            SubStatus::MerchantEnded => return Err(Error::SubscriptionMerchantEnded),
        }

        sub.status = SubStatus::MerchantEnded;
        storage::write_sub(&env, &sub);

        // Ending must succeed even if the registry refuses the follow-up
        // update; that outcome is reporting-only and never blocks it.
        let _ = Self::report(&env, &sub);

        storage::bump_instance(&env);
        MerchantEnded {
            subscription_id,
            merchant: sub.merchant,
            total_charged: sub.total_charged,
        }
        .publish(&env);
        Ok(())
    }

    /// Subscriber re-approves this contract for the cap still outstanding
    /// across their open subscriptions in `token`, with a fresh expiry.
    /// Returns the approved amount.
    ///
    /// Token allowances expire. A subscription that outlives its allowance
    /// fails to charge with `TransferFailed` — changing no state — until its
    /// subscriber calls this. It cannot raise any subscription's cap: the
    /// amount is recomputed from contract state, and `charge` enforces each
    /// cap itself regardless of the allowance.
    pub fn refresh_allowance(env: Env, subscriber: Address, token: Address) -> Result<i128, Error> {
        subscriber.require_auth();
        let amount = allowance::sync(&env, &subscriber, &token)?;
        storage::bump_instance(&env);
        events::AllowanceRefreshed {
            subscriber,
            token,
            amount,
        }
        .publish(&env);
        Ok(amount)
    }

    /// Deprecated: use [`Self::get_subscription_by_id`] instead. Kept for
    /// backwards compatibility.
    #[deprecated(since = "0.0.0", note = "use get_subscription_by_id instead")]
    pub fn get_subscription(env: Env, subscription_id: u64) -> Result<Subscription, Error> {
        storage::read_sub(&env, subscription_id)
    }

    /// Alias of `get_subscription`, named consistently with
    /// `get_by_subscriber` and `get_by_merchant`.
    pub fn get_subscription_by_id(env: Env, subscription_id: u64) -> Result<Subscription, Error> {
        storage::read_sub(&env, subscription_id)
    }

    /// Whether `charge` would pass every contract check right now: Active,
    /// not expired, interval elapsed, and one more period fits under the cap.
    ///
    /// The token is not consulted, so a charge can still fail with
    /// `TransferFailed` if the subscriber's balance or allowance is short.
    pub fn is_chargeable(env: Env, subscription_id: u64) -> Result<bool, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        if sub.status != SubStatus::Active {
            return Ok(false);
        }
        let now = env.ledger().sequence();
        // `expires_at == 0` means no expiry. A charge is still allowed at
        // exactly `expires_at`; only strictly past it is expired.
        if sub.expires_at != 0 && now > sub.expires_at {
            return Ok(false);
        }
        if now < sub.next_charge_ledger || now.checked_add(sub.interval_ledgers).is_none() {
            return Ok(false);
        }
        Ok(match sub.total_charged.checked_add(sub.amount_per_period) {
            Some(total) => total <= sub.total_cap,
            None => false,
        })
    }

    /// Why `charge` would or would not succeed right now, as a single status
    /// code. Consistent with `is_chargeable`: `ChargeStatus::Chargeable` is
    /// returned exactly when `is_chargeable` is true.
    pub fn charge_status(env: Env, subscription_id: u64) -> Result<ChargeStatus, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        let now = env.ledger().sequence();
        match sub.status {
            SubStatus::Cancelled => return Ok(ChargeStatus::Cancelled),
            SubStatus::Exhausted => return Ok(ChargeStatus::Exhausted),
            SubStatus::MerchantEnded => return Ok(ChargeStatus::MerchantEnded),
            SubStatus::Paused => return Ok(ChargeStatus::Paused),
            SubStatus::Active => {}
        }
        // `expires_at == 0` means no expiry; still chargeable at exactly
        // `expires_at`.
        if sub.expires_at != 0 && now > sub.expires_at {
            return Ok(ChargeStatus::Expired);
        }
        if now < sub.next_charge_ledger {
            return Ok(ChargeStatus::IntervalNotElapsed);
        }
        match sub.total_charged.checked_add(sub.amount_per_period) {
            Some(total) if total <= sub.total_cap => Ok(ChargeStatus::Chargeable),
            _ => Ok(ChargeStatus::CapExceeded),
    /// Names the reason `charge` would succeed or fail right now, covering
    /// every `SubStatus` plus the interval and cap checks `is_chargeable`
    /// applies while Active.
    pub fn charge_status(env: Env, subscription_id: u64) -> Result<ChargeStatus, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        let status = match sub.status {
            SubStatus::Paused => ChargeStatus::Paused,
            SubStatus::Cancelled => ChargeStatus::Cancelled,
            SubStatus::Exhausted => ChargeStatus::Exhausted,
            SubStatus::MerchantEnded => ChargeStatus::MerchantEnded,
            SubStatus::Active => {
                let now = env.ledger().sequence();
                if now < sub.next_charge_ledger {
                    ChargeStatus::TooEarly
                } else {
                    match sub.total_charged.checked_add(sub.amount_per_period) {
                        Some(total) if total <= sub.total_cap => ChargeStatus::Chargeable,
                        _ => ChargeStatus::CapWouldBeExceeded,
                    }
                }
            }
        };
        Ok(status)
    /// Why `charge` would succeed or fail right now. Agrees with
    /// `is_chargeable`: `Chargeable` exactly when that returns true.
    pub fn charge_status(env: Env, subscription_id: u64) -> Result<ChargeStatus, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        match sub.status {
            SubStatus::Paused => return Ok(ChargeStatus::Paused),
            SubStatus::Cancelled => return Ok(ChargeStatus::Cancelled),
            SubStatus::Exhausted => return Ok(ChargeStatus::Exhausted),
            SubStatus::MerchantEnded => return Ok(ChargeStatus::MerchantEnded),
            SubStatus::Active => {}
        }
        let now = env.ledger().sequence();
        if now < sub.next_charge_ledger {
            return Ok(ChargeStatus::TooEarly);
        }
        match sub.total_charged.checked_add(sub.amount_per_period) {
            Some(total) if total <= sub.total_cap => Ok(ChargeStatus::Chargeable),
            _ => Ok(ChargeStatus::CapWouldBeExceeded),
        }
    }

    /// Cap that can still ever be charged: `total_cap - total_charged` while
    /// Active or Paused, and 0 once Cancelled or Exhausted.
    pub fn remaining_cap(env: Env, subscription_id: u64) -> Result<i128, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        if sub.status.is_final() {
            return Ok(0);
        }
        sub.total_cap
            .checked_sub(sub.total_charged)
            .ok_or(Error::Overflow)
    }

    /// Every subscription a subscriber has created, oldest first.
    ///
    /// Fails with `ListTooLarge` instead of loading more than
    /// `MAX_LIST_SIZE` subscriptions, so a subscriber with a very large
    /// history cannot push a caller over the transaction budget.
    pub fn get_by_subscriber(env: Env, subscriber: Address) -> Result<Vec<Subscription>, Error> {
        Self::load_indexed(&env, &DataKey::BySubscriber(subscriber))
    }

    /// Every subscription payable to a merchant, oldest first.
    ///
    /// Fails with `ListTooLarge` instead of loading more than
    /// `MAX_LIST_SIZE` subscriptions, so a merchant with a very large
    /// history cannot push a caller over the transaction budget.
    pub fn get_by_merchant(env: Env, merchant: Address) -> Result<Vec<Subscription>, Error> {
        Self::load_indexed(&env, &DataKey::ByMerchant(merchant))
    }

    /// Total number of subscriptions ever created. O(1).
    pub fn get_subscription_count(env: Env) -> u64 {
        storage::read_next_id(&env).saturating_sub(1)
    }

    /// Number of subscriptions a subscriber has created. O(1).
    pub fn get_subscription_count_by_subscriber(env: Env, subscriber: Address) -> u32 {
        storage::read_index(&env, &DataKey::BySubscriber(subscriber)).len()
    }

    /// Number of subscriptions payable to a merchant. O(1).
    pub fn get_subscription_count_by_merchant(env: Env, merchant: Address) -> u32 {
        storage::read_index(&env, &DataKey::ByMerchant(merchant)).len()
    /// Lightweight analytics view of a subscription: its status and running
    /// totals, without the cost of returning the full `Subscription`.
    pub fn get_subscription_summary(
        env: Env,
        subscription_id: u64,
    ) -> Result<SubscriptionSummary, Error> {
        let sub = storage::read_sub(&env, subscription_id)?;
        Ok(SubscriptionSummary {
            id: sub.id,
            status: sub.status,
            total_charged: sub.total_charged,
            next_charge_ledger: sub.next_charge_ledger,
        })
    /// Page of a subscriber's subscriptions, oldest first.
    ///
    /// A `limit` of 0 returns an empty vector.
    pub fn get_by_subscriber_page(
        env: Env,
        subscriber: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        Self::load_indexed_page(&env, &DataKey::BySubscriber(subscriber), offset, limit)
    }

    /// Page of a merchant's subscriptions, oldest first.
    ///
    /// A `limit` of 0 returns an empty vector.
    pub fn get_by_merchant_page(
        env: Env,
        merchant: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        Self::load_indexed_page(&env, &DataKey::ByMerchant(merchant), offset, limit)
    /// Number of subscriptions created from a plan. Returns 0 for plan_id=0
    /// (ad-hoc subscriptions are never indexed by plan).
    pub fn count_by_plan(env: Env, plan_id: u64) -> u32 {
        storage::read_index(&env, &DataKey::ByPlan(plan_id)).len()
    }

    /// Subscriptions from a plan, oldest first. Returns up to `limit` entries
    /// starting at `offset`. Ad-hoc subscriptions (plan_id=0) are never indexed
    /// here, so this always returns an empty vec for plan_id=0.
    pub fn get_by_plan_page(
        env: Env,
        plan_id: u64,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        if limit == 0 {
            return Ok(Vec::new(&env));
        }
        let ids = storage::read_index(&env, &DataKey::ByPlan(plan_id));
        let end = offset.checked_add(limit).ok_or(Error::Overflow)?;
        let len = ids.len();
        let mut out = Vec::new(&env);
        for i in offset..end.min(len) {
            out.push_back(storage::read_sub(&env, ids.get(i).unwrap())?);
        }
        Ok(out)
    /// Every subscription created against a plan, oldest first. Ad-hoc
    /// subscriptions (`plan_id` 0) are never indexed, so 0 returns empty.
    pub fn get_by_plan(env: Env, plan_id: u64) -> Result<Vec<Subscription>, Error> {
        if plan_id == 0 {
            return Ok(Vec::new(&env));
        }
        Self::load_indexed(&env, &DataKey::ByPlan(plan_id))
    }

    /// One page of a plan's subscriptions, oldest first. `start` is the
    /// number of entries to skip; `limit` caps the page size.
    pub fn get_by_plan_page(
        env: Env,
        plan_id: u64,
        start: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        if plan_id == 0 {
            return Ok(Vec::new(&env));
        }
        Self::load_indexed_page(&env, &DataKey::ByPlan(plan_id), start, limit)
    }

    /// Number of subscriptions created against a plan. 0 for ad-hoc.
    pub fn count_by_plan(env: Env, plan_id: u64) -> Result<u32, Error> {
        if plan_id == 0 {
            return Ok(0);
        }
        Ok(storage::read_index(&env, &DataKey::ByPlan(plan_id)).len())
    }

    // ---- internal helpers ----

    fn require_admin(env: &Env, admin: &Address) -> Result<(), Error> {
        let stored = storage::read_admin(env)?;
        admin.require_auth();
        if stored != *admin {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }

    /// Require the subscriber's signature and that they own the
    /// subscription.
    fn load_for_subscriber(
        env: &Env,
        subscriber: &Address,
        subscription_id: u64,
    ) -> Result<Subscription, Error> {
        subscriber.require_auth();
        let sub = storage::read_sub(env, subscription_id)?;
        if sub.subscriber != *subscriber {
            return Err(Error::NotSubscriber);
        }
        Ok(sub)
    }

    /// A subscription to a plan must be for exactly that plan's terms, and
    /// the plan must still be accepting subscribers.
    fn check_plan(
        env: &Env,
        plan_id: u64,
        merchant: &Address,
        token: &Address,
        amount_per_period: i128,
        interval_ledgers: u32,
    ) -> Result<(), Error> {
        let plan_contract = storage::read_plan_contract(env).ok_or(Error::PlanContractNotSet)?;
        let plan = match PlanClient::new(env, &plan_contract).try_get_plan(&plan_id) {
            Ok(Ok(plan)) => plan,
            _ => return Err(Error::PlanNotFound),
        };
        if !plan.active {
            return Err(Error::PlanInactive);
        }
        if plan.merchant != *merchant
            || plan.token != *token
            || plan.amount_per_period != amount_per_period
            || plan.interval_ledgers != interval_ledgers
        {
            return Err(Error::PlanMismatch);
        }
        Ok(())
    }

    /// Mirror a subscription's total and status into the linked registry.
    /// A no-op when no registry is linked.
    fn report(env: &Env, sub: &Subscription) -> Result<(), Error> {
        let Some(registry) = storage::read_registry(env) else {
            return Ok(());
        };
        let updated = RegistryClient::new(env, &registry).try_update_subscription(
            &sub.id,
            &sub.total_charged,
            &(sub.status as u32),
        );
        if !matches!(updated, Ok(Ok(()))) {
            return Err(Error::RegistryUpdateFailed);
        }
        Ok(())
    }

    fn load_indexed(env: &Env, key: &DataKey) -> Result<Vec<Subscription>, Error> {
        let ids = storage::read_index(env, key);
        if ids.len() > MAX_LIST_SIZE {
            return Err(Error::ListTooLarge);
        }
        let mut out = Vec::new(env);
        for id in ids.iter() {
            out.push_back(storage::read_sub(env, id)?);
        }
        Ok(out)
    }

    /// Paginated variant of `load_indexed`. A `limit` of 0 returns an empty
    /// vector, consistently across every paginated view.
    fn load_indexed_page(
        env: &Env,
        key: &DataKey,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        let mut out = Vec::new(env);
        if limit == 0 {
            return Ok(out);
        }
        for id in storage::read_index(env, key).iter().skip(offset).take(limit) {
            out.push_back(storage::read_sub(env, id)?);
    fn load_indexed_page(
        env: &Env,
        key: &DataKey,
        start: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, Error> {
        let mut out = Vec::new(env);
        let ids = storage::read_index(env, key);
        let len = ids.len();
        if start >= len || limit == 0 {
            return Ok(out);
        }
        let end = start.saturating_add(limit).min(len);
        for i in start..end {
            out.push_back(storage::read_sub(env, ids.get_unchecked(i))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod test;
