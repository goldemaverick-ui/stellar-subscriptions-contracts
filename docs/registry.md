# Registry contract

`contracts/registry` — an index of plans and subscriptions with aggregate
stats, for dashboards and indexers.

The registry is **bookkeeping only**. It never moves funds and has no say in
whether a charge is allowed. It exists so a UI can show totals and lists
without replaying every event.

## Writers

Only two contracts may write, and they are fixed once:

- the **subscription contract** may register and update subscriptions;
- the **plan contract** may register plans.

`set_writers` is admin-only and callable exactly once, so the admin cannot
later point the registry at a contract that rewrites history. Anyone else
calling a write function fails authorization.

## Data

```rust
pub struct RegistryStats {
    pub total_plans: u64,
    pub total_subscriptions: u64,
    pub active_subscriptions: u64,   // last reported status is Active
    pub total_charged_volume: i128,  // sum of charges, raw units, all tokens
}

pub struct SubscriptionRecord {
    pub id: u64,
    pub subscriber: Address,
    pub merchant: Address,
    pub token: Address,
    pub total_charged: i128,
    pub status: u32,                 // 0 Active, 1 Paused, 2 Cancelled, 3 Exhausted, 4 MerchantEnded
}
```

## Per-merchant statistics

```rust
pub struct MerchantStats {
    pub total_subscriptions: u64,    // every subscription ever registered for this merchant
    pub active_subscriptions: u64,   // last reported status is Active, for this merchant
    pub total_charged_volume: i128,  // sum of charges for this merchant, raw units, all tokens
}
```

Each field counts the same way as its `RegistryStats` counterpart, but
scoped to one merchant's `Address` instead of the whole registry.
`total_subscriptions` and `total_charged_volume` only increase; they are
`u64`/`i128` counters and will panic on overflow (checked arithmetic), which
in practice requires an astronomically large number of subscriptions or
raw-unit charges. `active_subscriptions` moves up and down as subscriptions
transition into and out of Active.

`get_merchant_stats(merchant: Address) -> MerchantStats` is callable by
anyone and returns a zero-valued `MerchantStats` for a merchant that has
never had a subscription registered against it — it never errors.

## Events

Each successful write emits exactly one event. A rejected call emits nothing.

| Event | Emitted by | Data |
|---|---|---|
| `subscription_registered` | `register_subscription` | topics: `subscription_id`, `subscriber`, `merchant` |
| `subscription_updated` | `update_subscription` | topic: `subscription_id`; data: `total_charged`, `status` (new values) |

See [events.md](events.md#registry-contract) for the full field layout.

Indexers can follow these events instead of polling `get_stats`.

## Functions

| Function | Caller | Notes |
|---|---|---|
| `initialize(admin)` | admin | Once. |
| `set_writers(admin, subscription_contract, plan_contract)` | admin | Once. |
| `register_plan(plan_id)` | plan contract | Each plan counted once. |
| `register_subscription(subscription_id, subscriber, merchant, token)` | subscription contract | Starts Active. `token` is stored on the record. |
| `update_subscription(subscription_id, total_charged, status)` | subscription contract | See rules below. |
| `get_stats() -> RegistryStats` | anyone | |
| `get_merchant_stats(merchant) -> MerchantStats` | anyone | Zero-valued if never indexed. |
| `get_subscription_record(subscription_id)` | anyone | |
| `get_all_subscriptions() -> Vec<u64>` | anyone | Oldest first. |
| `get_all_subscriptions_page(page, limit) -> Result<Vec<u64>, Error>` | anyone | Paginated, oldest first. |
| `get_sub_records_by_merchant(merchant, start, limit) -> Result<Vec<SubscriptionRecord>, Error>` | anyone | One page of a merchant's records, oldest first. Empty if none or `start` is past the end. |
| `get_all_subscriptions() -> Vec<u64>` | anyone | **Deprecated.** Oldest first. O(n): reads every id in one call. Prefer the paged form. |
| `get_all_subscriptions_page(start, limit) -> Result<Vec<u64>, Error>` | anyone | Up to `limit` ids from index `start`, oldest first. `limit` ≤ `MAX_PAGE_SIZE` (100) or `PageSizeTooLarge`. |
| `get_active_subscriptions() -> Vec<u64>` | anyone | Oldest first. |
| `get_active_subscriptions_page(page, limit) -> Result<Vec<u64>, Error>` | anyone | Paginated, oldest first. |
| `get_writers() -> Writers` | anyone | |

## Update rules

`update_subscription` mirrors the subscription contract's own rules so the
index can never reach a state the subscription contract could not:

- `status` must be 0–4 (`InvalidStatus`);
- `total_charged` never decreases (`ChargedDecreased`);
- a Cancelled, Exhausted or MerchantEnded record never changes again (`AlreadyFinal`);
- `total_charged_volume` grows by exactly the increase, with checked
  arithmetic;
- `active_subscriptions` moves only on a real transition into or out of
  Active, so cancelling a paused subscription is not double-counted.

## Caveats

- **Warning: `total_charged_volume` is not a currency amount.** It adds raw
  token units across **all tokens** with no decimal or price normalization.
  1 XLM (7 decimals) and 1 USDC (7 decimals) each add their raw units, so
  mixed-token totals are meaningless as value. Use it only as an activity
  signal, and take per-token volume from an indexer.
- `get_active_subscriptions` walks every registered subscription.
- Plans and subscriptions created before the registry was linked would be
  missing, which is why both the plan and subscription contracts only accept
  a registry link before their first record.

## Errors

| Code | Name |
|---|---|
| 1 | `AlreadyInitialized` |
| 2 | `NotInitialized` |
| 3 | `Unauthorized` |
| 4 | `NotConfigured` |
| 5 | `AlreadyConfigured` |
| 6 | `SubscriptionAlreadyRegistered` |
| 7 | `SubscriptionNotFound` |
| 8 | `InvalidStatus` |
| 9 | `ChargedDecreased` |
| 10 | `AlreadyFinal` |
| 11 | `Overflow` |
| 12 | `PlanAlreadyRegistered` |
| 13 | `PageSizeTooLarge` |

Codes are append-only.
