# Changelog

Notable changes to the Soroban contracts in this workspace. Entries are
newest first and reference the issue and PR they came from.

This file starts at the emergency circuit breaker (#42). Earlier work is
summarised under [Before this changelog](#before-this-changelog) from the
commit history rather than reconstructed in detail, so nothing here claims
more precision than it has.

The workspace is pre-release: nothing is deployed, every contract is on
storage version 1, and no migration has been needed yet. Versioned release
sections start once something ships.

## [Unreleased]

### Added

- **Cross-contract settlement router with auth-chained escrow → reputation →
  loyalty atomicity** ([#38](https://github.com/workman-labs/guildworkman-core/issues/38),
  [PR #50](https://github.com/workman-labs/guildworkman-core/pull/50)).
  A new `contracts/settlement-router` crate that atomically drives escrow
  release, reputation attestation, and loyalty emission from a single
  `settle(appointment_id, rating, attestation_hash)` call, so a completed
  appointment settles as one indivisible unit instead of three independently
  callable — and independently spoofable — entrypoints:
  - `settle` proves the appointment is `Funded` in `escrow` before doing
    anything else, then calls `escrow.confirm_completion`,
    `reputation.submit_attestation`, and `loyalty-token.mint` in one
    transaction. Any `Err` from a sub-contract, or a pause on its side,
    panics the whole invocation — nothing partially commits.
  - **Idempotent per `appointment_id`**, checked before any cross-contract
    call and enforced twice over: this router's own `Settled` marker, and
    `escrow.confirm_completion` independently refusing a second call once
    the appointment is no longer `Funded`.
  - `reputation` gains an opt-in `set_router`/`get_router` (admin-only).
    Once set, `submit_attestation` additionally requires that router's own
    authorization alongside the client's — closing the previous gap where
    any `appointment_id` could be attested with no proof it was ever funded
    or completed. A contract address can only satisfy that requirement by
    directly executing the call, so this cannot be forged by an
    externally-owned account.
  - `loyalty-token.mint` needs no code change: pointing its existing
    `minter` role at this router (`set_minter`) is what gates it, the same
    migration `loyalty-emissions` already models.
  - Reward amounts are a fixed, admin-configured `RewardConfig` — never
    taken from a `settle` caller's own arguments, so a caller cannot name
    their own mint amount.
  - Guarded by the existing shared `SCOPE_SETTLEMENT` (no new scope
    introduced); see [Emergency circuit breaker](README.md#emergency-circuit-breaker).
- **Emergency circuit breaker across `escrow`, `reputation`, `loyalty-token`
  and `loyalty-emissions`** ([#42](https://github.com/workman-labs/guildworkman-core/issues/42),
  [PR #46](https://github.com/workman-labs/guildworkman-core/pull/46)). A shared pausability primitive in
  `contracts/governance-guard`'s new `pausable` module, built so that a halt
  can never become a fund trap:
  - **Scoped guards** rather than one global flag — `SCOPE_INTAKE`,
    `SCOPE_SETTLEMENT`, `SCOPE_ATTESTATION` — so paths that let value *in*
    can be halted independently of paths that let it *out*.
  - **Fund-recovery paths are never guarded**, enforced by omission at the
    call sites. `escrow`'s `cancel_appointment` and every dispute
    entrypoint, `loyalty-token`'s `transfer`/`transfer_from`/`approve`/
    `burn` of already-held balances, `loyalty-emissions`' `reclaim`, and all
    read-only views carry no pause check. No scope value, `ALL_SCOPES`
    included, can reach them.
  - **Ledger-time auto-expiry** capped at `MAX_PAUSE_DURATION` (7 days),
    evaluated on every guard consultation, so an unattended pause clears
    itself with no transaction, no live admin and no working key.
  - **Authorization by any single governance signer**, not the M-of-N
    threshold and not each contract's `admin` — mirroring `cancel_upgrade`.
    Unilateral in both directions, so a responder who pauses and then goes
    offline cannot wedge it in place.
  - New entrypoints on all four contracts: `pause`, `unpause` (masked, and
    deliberately does not touch the deadline), `get_pause_state`,
    `paused_scopes`, `is_paused`.
  - New events `Paused { caller, scopes, expires_at, reason }` and
    `Unpaused { caller, scopes, remaining_scopes }` for off-chain
    monitoring. Auto-expiry emits nothing — it has no transaction behind it
    — so `Paused.expires_at` is the authoritative end of a window unless an
    `Unpaused` arrives sooner.
  - A length-capped operator `reason` (≤ 64 UTF-8 **bytes**, not characters;
    may be empty) stored with the pause and emitted with the event, so "why
    is this halted?" is answerable from chain state. Client-side validation
    must count bytes.
  - Event wire format is pinned by assertion, not just documented: topics are
    `[Symbol("gov_pause"), Symbol("paused"|"unpaused"), Address(caller)]` and
    data is a `Map<Symbol, Val>` keyed by field name.
  - New errors per contract: `OperationPaused` (deliberately distinct from
    any status error), `InvalidPauseScope`, `InvalidPauseDuration`,
    `NotPaused`, `InvalidPauseReason` — appended so no existing code moved.
  - New storage key `GovernanceDataKey::PauseState`, appended after
    `PendingRotation` so deployed contracts' key encodings stay put.
  - `scripts/broadcast-pause.sh` for sweeping a mask across all four
    contracts during an incident.

### Changed

- CI (`.github/workflows/soroban-ci.yml`) now caches Cargo artifacts on
  failed runs too (`cache-on-failure`), so a run that fails on `fmt` or
  `clippy` doesn't make the retry rebuild `soroban-sdk` from scratch.

### Notes

- Measured guard overhead on the hot path: ~168 CPU instructions when no
  pause has been set (against ~336k for a full `create_appointment`), rising
  to ~25k only while a pause record actually exists. Full table in
  [README.md](README.md#hot-path-cost); tests in
  `contracts/escrow/src/test.rs` under "hot-path cost". Numbers are
  SDK-test-metered and underestimate compiled Wasm — they are a relative
  comparison, not a fee estimate.

## Before this changelog

Reconstructed from commit history; see each PR for detail.

- Governance signer rotation for the upgrade guard — timelocked
  propose/approve/execute, no unilateral veto (#27, PR #32).
- `dispute-resolution` v2 (PR #31), and the original decentralized dispute
  resolution with staked jury commit-reveal voting and slashing (PR #28).
- Multi-sig governance guard for contract upgrades and storage migration
  (#16, PR #26).
- `reputation`: sybil-resistant weighted scoring from signed attestations,
  with time decay and rate limiting (PR #20).
- `loyalty-emissions`: streaming emission engine with per-account and global
  rate limiting, plus admin reclaim (PR #19).
- Monorepo split into `backend-api/` and `soroban-contracts/` (c405b31).
