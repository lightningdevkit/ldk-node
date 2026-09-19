# Explanation: `claim_for_id` amount validation fix

Branch: `fix/claim-for-id-amount-validation`
Commit: `91ac482` — "fix: validate claim_for_id amount against the observed PaymentClaimable event"

## The bug

`Bolt11Payment::claim_for_id(payment_id, claimable_amount_msat, preimage)` lets an
application manually claim a Lightning payment after receiving a `PaymentClaimable`
event. The caller is supposed to pass the `claimable_amount_msat` that the event
reported, and the code was supposed to check that value made sense before actually
claiming the funds (calling `claim_funds` is irreversible-ish — you're telling LDK
"yes, release the preimage, accept this payment").

The old check looked like this (simplified):

```rust
if let Some(invoice_amount_msat) = details.amount_msat {
    if claimable_amount_msat < invoice_amount_msat.saturating_sub(skimmed_fee_msat) {
        return Err(Error::InvalidAmount);
    }
}
```

It compared the caller's argument against `details.amount_msat` — the amount stored
on the `PaymentDetails` record. The problem: since the "payment-ID refactor" landed
in v0.8-development, `details.amount_msat` for an inbound payment is *itself* set
from the very same `PaymentClaimable` event's amount. So this check had degenerated
into:

```rust
claimable_amount_msat < claimable_amount_msat.saturating_sub(skimmed_fee_msat)
```

i.e. comparing the argument against essentially itself. Concretely:
- A well-behaved caller who passed the *correct* amount would trivially pass.
- A caller who mixed up arguments — e.g. swapped the amount from a *different*,
  concurrently-claimable payment — could also pass, because the check no longer
  had any independent value to compare against. There was nothing actually
  verifying "does this argument match what LDK told us for *this* payment."

This matters when an application is juggling multiple concurrent manual claims
(multiple `PaymentClaimable` events outstanding at once) — it's easy to accidentally
pass payment A's amount while claiming payment B, and the old code wouldn't catch it.

## The fix, piece by piece

### 1. `src/payment/store.rs` — a new field: `claimable_amount_msat`

Added a new field to `PaymentKind::Bolt11`:

```rust
PaymentKind::Bolt11 {
    hash: PaymentHash,
    preimage: Option<PaymentPreimage>,
    secret: Option<PaymentSecret>,
    counterparty_skimmed_fee_msat: Option<u64>,
    claimable_amount_msat: Option<u64>,   // <-- new
}
```

This stores **the amount reported by the most recent `PaymentClaimable` event**
for that payment — independent of `amount_msat`, which can be overwritten/derived
elsewhere. Think of it as "the last thing LDK actually told us was claimable,"
kept separately so it can later be used as a *ground truth* to check the caller's
argument against.

Supporting changes:
- `PaymentDetailsUpdate` gained a matching `claimable_amount_msat: Option<Option<u64>>`
  field (the usual double-`Option` pattern: outer `None` = "don't touch this field",
  inner `None` = "set it to None").
- `UpdatableObject::update()` for `PaymentDetails` now applies this update, with a
  `debug_assert!` that it's only ever set for `Bolt11` payments (a spontaneous or
  BOLT12 payment shouldn't have one).
- TLV (de)serialization: added as field `8` (a new optional field, so existing
  serialized records without it just decode to `None` — backwards compatible).
  This is why the `Readable for PaymentDetails` migration path (for pre-v0.8
  serialized records) also explicitly sets `claimable_amount_msat: None`.
- All the other places across the codebase that construct a `PaymentKind::Bolt11`
  literal (there are several, in `bolt11.rs`, `event.rs`, tests, etc.) needed a
  `claimable_amount_msat: None` (or `Some(...)` in test data) added, since it's a
  new required struct field. That's most of the "noise" diff you see repeated in
  many places — it's mechanical, not logic-bearing.

### 2. `src/event.rs` — actually recording the observed amount

When LDK fires `Event::PaymentClaimable` and ldk-node is about to forward it to
the user as `crate::Event::PaymentClaimable`, it now writes the event's amount
into the new field *before* emitting the event:

```rust
let claimable_update = PaymentDetailsUpdate {
    claimable_amount_msat: Some(Some(amount_msat)),
    ..PaymentDetailsUpdate::new(payment_id)
};
self.payment_store.update(claimable_update).await?;
```

So by the time your application code sees the `PaymentClaimable` event and later
calls `claim_for_id`, the payment store already has a durable record of "this is
the amount LDK told us was claimable for this payment." Every other place that
constructs a fresh `PaymentDetails`/update for a Bolt11 payment (initial invoice
creation, spontaneous payment received, etc.) sets `claimable_amount_msat: None`,
since no claimable event has fired yet for those.

### 3. `src/payment/bolt11.rs` — the actual validation logic

New free function `validate_claimable_amount`:

```rust
fn validate_claimable_amount(
    claimable_amount_msat: u64,
    requested_amount_msat: Option<u64>,
    observed_claimable_amount_msat: Option<u64>,
    counterparty_skimmed_fee_msat: u64,
) -> Result<(), Error> {
    if let Some(observed_amount_msat) = observed_claimable_amount_msat {
        if claimable_amount_msat != observed_amount_msat {
            return Err(Error::InvalidAmount);
        }
    }

    if let Some(requested_amount_msat) = requested_amount_msat {
        if claimable_amount_msat < requested_amount_msat.saturating_sub(counterparty_skimmed_fee_msat) {
            return Err(Error::InvalidAmount);
        }
    }

    Ok(())
}
```

Two independent checks, both must pass:

1. **Equality against the observed event amount** (new, the actual fix): if we
   have a recorded `claimable_amount_msat` from a real `PaymentClaimable` event
   (i.e. any payment received since v0.8), the caller's argument must match it
   *exactly*. This is what catches the argument mix-up bug. Since the event
   amount already accounts for LSP fee-skimming etc., an exact match is the
   right bar — no under/over slack needed here.

2. **Historic underpayment guard** (preserved from before, for backwards
   compatibility): if `details.amount_msat` (the originally requested invoice
   amount) is known — which for *pre-v0.8 serialized* payments is independent
   of the event — the claimable amount must be at least the requested amount
   minus any JIT-channel LSP skimmed fee. For payments received since v0.8,
   `amount_msat` is itself event-derived, so this check degenerates to a no-op
   for them (which is fine — check #1 already protects them).

`claim_for_id` was updated to pull `counterparty_skimmed_fee_msat` and the new
`claimable_amount_msat` out of `details.kind` alongside `hash`, and call
`validate_claimable_amount(...)` instead of the old inline check.

Doc comments on `claim_for_id` and on the new `PaymentKind::Bolt11::claimable_amount_msat`
field were expanded to explain this two-tier behavior for future readers.

### 4. Tests

**Unit tests** in `bolt11.rs` (all against `validate_claimable_amount` directly,
no node/network needed):
- `migrated_record_rejects_underpayment` / `_allows_overpayment` / `_accounts_for_jit_fee`
  — cover the pre-v0.8 "no observed amount" path, confirming the historic guard
  still works (including the fee-adjusted JIT-channel case).
- `current_record_rejects_mismatched_argument` / `_accepts_the_observed_amount`
  / `_accepts_fee_adjusted_jit_amount` — cover the new post-v0.8 "observed amount
  present" path, confirming mismatches are now rejected and the correct
  (possibly fee-adjusted) amount is accepted.
- `unregistered_record_relies_on_observed_amount_only` — a payment with no prior
  registration, showing the observed-amount check alone is sufficient protection
  even when `amount_msat` is self-referential.

**Integration test** in `tests/integration_tests_rust.rs`, inside the LSPS2
JIT-channel test (`do_lsps2_client_service_integration`): after a JIT payment
becomes claimable, it now asserts that calling `claim_for_id` with (a) the full
invoice amount (forgetting to subtract the LSP's skimmed fee) and (b) an amount
one msat off from the true claimable amount, both return `Err(NodeError::InvalidAmount)`
— i.e., the bug this PR fixes is now actually exercised end-to-end before the
real, successful claim proceeds.

### 5. `CHANGELOG.md`

A new "Fixed"-style entry under the unreleased section summarizing the above for
downstream consumers of the crate/changelog.

## What you need to know / watch out for

- **This is a backwards-compatible storage change.** The new TLV field (`8`) is
  optional on read, so old serialized `PaymentDetails` records deserialize fine
  with `claimable_amount_msat: None` — no migration script needed, no breaking
  change to on-disk format.
- **Behavior change for API consumers:** if any external caller of `claim_for_id`
  was previously (accidentally or not) passing an amount that *didn't* exactly
  match the `PaymentClaimable` event's amount but happened to still satisfy the
  old (broken) invoice-amount check, that call will now correctly fail with
  `Error::InvalidAmount`. This is the intended fix, but it's worth flagging in
  the PR description as a behavior change, not just a "pure bug fix with zero
  observable difference" — well-behaved callers are unaffected, sloppy ones will
  now see errors they should have been seeing all along.
- **`cargo fmt` was not run** and the build/tests were not compiled or executed
  for this commit, per your explicit instruction. Before merging, you'll want to
  run `cargo fmt --all` and the full test suite (per the repo's own CLAUDE.md
  rules) since that wasn't done as part of this commit.
- **AI tooling disclosure** was added to the commit message body (per CLAUDE.md's
  requirement to disclose AI tool use in commit messages/PR descriptions), noting
  Claude Code was used to help implement and test the change. No co-author trailer
  was added, per your request.
- The branch has already been pushed to your fork
  (`fix/claim-for-id-amount-validation`); GitHub returned a compare/PR link:
  `https://github.com/yahia008/ldk-node/pull/new/fix/claim-for-id-amount-validation`
  if you want to open a PR.
