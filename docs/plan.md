# Plan contract

`contracts/plan` — reusable billing terms published by merchants.

A plan is a template: token, amount per period, interval, and a suggested
cap. Subscribers subscribe to a plan through the subscription contract, which
checks that the subscription's terms match the plan exactly. The subscriber
still picks their own cap.

A plan **never moves funds** and has **no authority over subscriptions**.
Deactivating one only stops new subscriptions to it.

## Data

```rust
pub struct Plan {
    pub id: u64,               // starts at 1; 0 means "no plan" to subscriptions
    pub merchant: Address,
    pub name: String,          // 1..=64 bytes
    pub token: Address,
    pub amount_per_period: i128,
    pub interval_ledgers: u32,
    pub default_cap: i128,     // suggestion, >= amount_per_period
    pub active: bool,
    pub created_at: u32,       // ledger sequence
    pub description: Option<String>, // None or 1..=MAX_DESCRIPTION_LEN bytes
    pub subscriber_count: u64, // incremented on each subscribe naming this plan
}
```

The subscription contract reads `Plan` across contracts and decodes it by
field name, so **field names must not change**.

`description` is optional free-form text (e.g. a human-readable summary of the
plan). If provided it must be non-empty and at most `MAX_DESCRIPTION_LEN`
(256) bytes; longer descriptions are rejected with `InvalidDescription`.
`subscriber_count` starts at 0 and is incremented by the subscription contract
on every successful `subscribe` that names this plan. It is a convenience
counter only — it is not decremented on cancellation or expiry.

## Functions

| Function | Signature required | Notes |
|---|---|---|
| `initialize(admin)` | admin | Once. |
| `set_registry(admin, registry)` | admin | Once, before the first plan. |
| `create_plan(merchant, name, token, amount_per_period, interval_ledgers, default_cap) -> u64` | **merchant** | Active immediately. Counted in the registry if linked. |
| `deactivate_plan(merchant, plan_id)` | **the plan's merchant** | Stops new subscriptions; reversible via `reactivate_plan`. |
| `reactivate_plan(merchant, plan_id)` | **the plan's merchant** | Re-enables a deactivated plan, keeping its ID. |
| `create_plan(merchant, name, token, amount_per_period, interval_ledgers, default_cap, description) -> u64` | **merchant** | Active immediately. `description` is `Option<String>`; if `Some`, it must be 1..=`MAX_DESCRIPTION_LEN` bytes. Counted in the registry if linked. |
| `set_plan_description(merchant, plan_id, description)` | **the plan's merchant** | Replaces the description (`None` clears it). Emits `plan_description_updated`. Same byte limit as `create_plan`. |
| `deactivate_plan(merchant, plan_id)` | **the plan's merchant** | Irreversible. |
| `get_plan(plan_id) -> Plan` | none | |
| `get_plan_subscriber_count(plan_id) -> u64` | none | Returns `Plan.subscriber_count`; 0 for unknown plans. |
| `get_plans_by_merchant(merchant) -> Vec<Plan>` | none | Active and inactive, oldest first. |
| `get_plans_by_merchant_page(merchant, start, limit) -> Vec<Plan>` | none | Paginated variant of `get_plans_by_merchant`. |
| `get_active_plans_by_merchant(merchant) -> Vec<Plan>` | none | Only active plans, oldest first. |
| `get_active_plans() -> Vec<Plan>` | none | Oldest first. **Deprecated** in favour of `get_active_plans_page`. |
| `get_active_plans_page(start, limit) -> Vec<Plan>` | none | Paginated active plans, oldest first. |
| `get_plans_by_token(token, offset, limit) -> Vec<Plan>` | none | Oldest first. If `offset >= len`, returns an **empty vec** (not an error); see below. |
| `get_active_plans() -> Vec<Plan>` | none | Oldest first. |
| `get_registry() -> Option<Address>` | none | |
| `get_admin() -> Address` | none | |

### `get_plans_by_token` pagination

`get_plans_by_token` returns at most `limit` plans starting at `offset`. An
`offset` greater than or equal to the number of plans for that token is **not**
an error: the call returns an empty `Vec`. Callers that need to distinguish
"no plans exist for this token" from "offset is past the end" should first
call `get_plans_by_merchant` (or an indexer) to learn the total count, or
simply stop paging when an empty page is returned.

## What deactivation does and does not do

- New `subscribe` calls naming the plan fail with `PlanInactive`.
- `reactivate_plan` restores `active = true` on the same plan ID, so
  subscriptions can resume without creating a new plan.
- Existing subscriptions keep charging exactly as authorized. The subscriber
  consented to specific terms and a cap; the merchant retiring a plan does not
  change that consent. A merchant who wants to stop billing simply stops
  calling `charge`.

This is covered by `deactivating_a_plan_leaves_existing_subscriptions_chargeable`
in the subscription crate's integration tests.

## `default_cap` and whole periods

`default_cap` is a suggestion that subscribers copy into their own
`total_cap`. It only has to be at least one period's charge (`InvalidCap`
otherwise). It does **not** have to be a whole multiple of
`amount_per_period`, and the plan contract does not reject a cap that is not.

If it is not a multiple, the remainder can never be charged. Worked example,
with `amount_per_period = 10` and `default_cap = 15`:

| Charge | `total_charged` after | Result |
|---|---|---|
| 1st | 10 | Accepted (10 <= 15). |
| 2nd | 20 | Rejected with `CapExceeded` (20 > 15). |

The subscription stays `Active` with 5 unchargeable. A subscriber who copies
the suggested cap gets exactly one period's charge from it. Merchants who want
the whole cap used should publish a `default_cap` that is a multiple of
`amount_per_period` (for example 20 or 30). The subscription contract's rules
are described in `docs/subscription.md`, in the paragraph that begins "A cap
that is not a whole number of periods is allowed".

This is covered by `a_default_cap_that_is_not_a_whole_multiple_is_accepted`
in the plan crate's tests.

## Events

| Event | Topics | Data |
|---|---|---|
| `plan_created` | plan_id, merchant | name, token, amount_per_period, interval_ledgers, default_cap, description |
| `plan_description_updated` | plan_id, merchant | description |
| `plan_deactivated` | plan_id, merchant | — |
| `plan_reactivated` | plan_id, merchant | — |

## Errors

| Code | Name | Meaning |
|---|---|---|
| 1 | `AlreadyInitialized` | |
| 2 | `NotInitialized` | |
| 3 | `Unauthorized` | Caller is not the admin. |
| 4 | `InvalidName` | Empty or longer than 64 bytes. |
| 5 | `InvalidAmount` | `amount_per_period <= 0`. |
| 6 | `InvalidInterval` | `interval_ledgers == 0`. |
| 7 | `InvalidCap` | `default_cap < amount_per_period`. |
| 8 | `PlanNotFound` | |
| 9 | `NotPlanMerchant` | Only the creating merchant may deactivate or reactivate. |
| 10 | `PlanAlreadyInactive` | |
| 11 | `Overflow` | |
| 12 | `AlreadyConfigured` | Registry already linked, or a plan already exists. |
| 13 | `RegistryUpdateFailed` | The linked registry refused to count the plan. |
| 14 | `PlanAlreadyActive` | `reactivate_plan` called on an active plan. |
| 15 | `InvalidPageSize` | `limit` is 0 or greater than `MAX_PAGE_SIZE`. |
| 15 | `InvalidDescription` | Description is empty or longer than `MAX_DESCRIPTION_LEN` (256) bytes. |

Codes are append-only. (14 is reserved.)

## Limits

`get_active_plans` walks every plan ever created and will exceed the
per-transaction instruction budget for large catalogues. Prefer the paginated
queries (`get_active_plans_page`, `get_plans_by_merchant_page`) with a `limit`
of at most `MAX_PAGE_SIZE`. For bulk listings, serve them from an indexer
reading `plan_created` / `plan_deactivated` / `plan_reactivated` events.
`get_active_plans` walks every plan ever created. For large catalogues, serve
listings from an indexer reading `plan_created` / `plan_deactivated` events.

`MAX_DESCRIPTION_LEN` is **256 bytes**. Descriptions are stored as UTF-8 and
the limit is measured in bytes, not characters.
