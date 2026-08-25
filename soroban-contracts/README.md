# guildworkman-contracts

![CI](https://github.com/workman-labs/guildworkman-contracts/actions/workflows/ci.yml/badge.svg)

Soroban (Stellar) smart contracts for GuildWorkman, the skilled-worker booking
marketplace. This workspace has six independent contracts:

| Contract | Path | Purpose |
|---|---|---|
| `escrow` | `contracts/escrow` | Holds a client's payment for a booked appointment until the client confirms the job is done; releases funds to the skilled worker, refunds on cancellation, and supports admin-arbitrated disputes. |
| `reputation` | `contracts/reputation` | Stores one immutable review per completed appointment and keeps a running rating aggregate per skilled worker. |
| `loyalty-token` | `contracts/loyalty-token` | A SEP-41-style fungible token used to reward clients/workers with points on completed appointments. Only a designated `minter` (the backend's service account) can mint. |
| `loyalty-emissions` | `contracts/loyalty-emissions` | An emission engine that owns the `loyalty-token`'s `minter` role. Instead of minting rewards in a lump sum, it streams them out of per-account linear vesting schedules, throttled by per-account and global rate limits, and lets the admin reclaim allocations left unclaimed past a deadline. |
| `settlement-router` | `contracts/settlement-router` | Orchestrates `escrow`, `reputation` and `loyalty-token` atomically: a single `settle` call releases escrowed funds, records the client's attestation, and mints loyalty points, or none of it happens. Becomes the sole trust root `reputation`/`loyalty-token` accept a settlement from once wired in — see [settlement-router](#settlement-router). |
| `dispute-resolution` | `contracts/dispute-resolution` | Decentralized alternative to `escrow`'s single-admin arbitration: resolves a dispute via a **staked jury** using **commit-reveal** voting, then pays the majority out of the slashed stakes of the minority and no-shows. |
| `governance-guard` | `contracts/governance-guard` | Not a deployed contract — a shared library that five of the contracts above (all except `dispute-resolution`) depend on, providing the multi-sig upgrade/migration pattern described in [Upgrade governance](#upgrade-governance). |

These mirror the domain already implemented server-side in the backend
([`../backend-api`](../backend-api): `AppointmentService`, `ReviewService`,
`TransactionService`), moving the trust-sensitive parts of that flow —
holding money, recording reviews, issuing rewards — on-chain.

## Table of contents

- [Project ecosystem](#project-ecosystem)
- [Architecture](#architecture)
- [Upgrade governance](#upgrade-governance)
- [Emergency circuit breaker](#emergency-circuit-breaker)
- [Contract Events](#contract-events)
- [Prerequisites](#prerequisites)
- [Build](#build)
- [Test](#test)
- [Deploy (testnet example)](#deploy-testnet-example)
- [Contract interfaces](#contract-interfaces)
- [Security considerations / known limitations](#security-considerations--known-limitations)
- [Notes / follow-ups](#notes--follow-ups)
- [License](#license)

Notable changes are recorded in [CHANGELOG.md](CHANGELOG.md).

## Project ecosystem

GuildWorkman lives in two repositories:

| Repo | Role |
|---|---|
| **`guildworkman-core`** (this repo) | These Soroban contracts (`soroban-contracts/`, here) **and** the Spring Boot backend (`backend-api/`) — which implements the same domain server-side today (`AppointmentService`, `ReviewService`, `TransactionService`) and is the intended caller of these contracts once integrated (see [Suggested backend integration](#suggested-backend-integration-not-yet-wired-in)). |
| [`guildworkman-web`](https://github.com/workman-labs/guildworkman-web) | Next.js frontend. Already has a real Freighter wallet-connect button and a "Trust, backed by smart contracts" section describing these contracts — but makes no contract calls yet. |

## Architecture

These are three separate contracts rather than one monolith:

- **Independent storage and upgrade paths.** `escrow` handles money,
  `reputation` handles reviews, `loyalty-token` handles a token balance
  ledger. Each has a different risk profile and a different reason to change;
  bugs or upgrades in one shouldn't force redeploying the others.
- **Independent trust boundaries.** `escrow` and `loyalty-token` each have
  their own `admin`/`minter` role — they don't need to trust each other's
  internal state, only the caller's authenticated identity.
- **Composability over coupling.** A future contract (or the backend) can
  call into any of the three via their public functions without depending on
  their private storage layout.

### Suggested backend integration (not yet wired in)

None of this is currently called from the backend (`../backend-api`). The
intended flow, if/when integrated, looks like:

1. Client books a worker in the existing Java backend → backend (or the
   client's wallet, if going non-custodial) calls `escrow.create_appointment`
   with the agreed amount and a token address (e.g. a Stellar Asset Contract
   wrapping USDC), instead of only charging via Paystack.
2. Client marks the job done in the app → backend calls
   `escrow.confirm_completion`, which pays the worker.
3. Backend calls `reputation.submit_review` with the same `appointment_id`
   right after the client submits their in-app review, so on-chain reviews
   stay 1:1 with completed, paid appointments.
4. Backend (as `minter`) calls `loyalty-token.mint` to reward the client
   and/or worker some points for the completed appointment — or, for rewards
   that should vest over time rather than land instantly, registers a
   `loyalty-emissions.create_schedule` and lets the recipient `claim` the
   stream as it vests (see [loyalty-emissions](#loyalty-emissions)).

Steps 2-4 can instead collapse into a single call once `settlement-router` is
deployed and wired in (see [settlement-router](#settlement-router)): the
backend (or the client's own wallet) calls `settlement-router.settle` once,
which atomically drives `escrow.confirm_completion`,
`reputation.submit_attestation` and `loyalty-token.mint` — funds, review and
reward land together or not at all, and neither `reputation` nor
`loyalty-token` will accept a write from anywhere else once that wiring is
in place.

This requires the backend to hold a Stellar keypair per role (or per user, if
going non-custodial) and a Soroban RPC client — none of that exists in
`backend-api/` today.

## Upgrade governance

All five contracts can have their code swapped in place via Soroban's
`update_current_contract_wasm`, gated behind an M-of-N multi-sig — set once
at `initialize` via a `signers: Vec<Address>` + `threshold: u32` — instead of
being controlled by a single key or left permanently immutable. The pattern
lives in `contracts/governance-guard`, a shared library each contract depends
on and calls from inside its own methods; it is not itself deployed.

This is deliberately narrow in scope: it only ever gates the ability to swap
a contract's code and run its post-upgrade migration. It does not replace or
extend any contract's existing `admin`/`minter` role, and a compromised or
colluding signer set still cannot resolve disputes, mint tokens, or reclaim
schedules directly — it can only ship new code that would need to be
malicious on its own terms to do any of that.

**Flow:**

1. Any signer uploads the new Wasm — `stellar contract upload --wasm ...` —
   and gets back a hash. (This step doesn't go through governance-guard; it
   just puts bytes on the ledger, uploading code executes nothing.)
2. A signer calls `propose_upgrade(proposer, wasm_hash)`. Their approval is
   recorded immediately.
3. Other signers call `approve_upgrade(approver, wasm_hash)` until approvals
   reach `threshold`. **Whichever call — propose or approve — is the one
   that reaches threshold performs the actual swap in that same
   transaction** (with a 1-of-N guard, that's `propose_upgrade` itself).
4. Because a Wasm swap only takes effect after its own invocation finishes,
   the new code can't run a migration in that same call. A signer calls
   `migrate(signer)` afterward, in a separate transaction, to run whatever
   storage transformation the new version needs and advance the recorded
   storage version. No such transformation exists yet in any contract —
   `migrate` currently just proves the version-gated path is real and
   returns `NothingToMigrate` on a fresh deploy, so it's a real place for a
   future version to land field-by-field changes rather than a promise.

Every contract that adopts this exposes the same six entrypoints —
`propose_upgrade`, `approve_upgrade`, `cancel_upgrade`, `migrate`, plus
read-only `get_signers`, `get_upgrade_threshold`, `get_pending_upgrade`,
`get_storage_version` — on top of whatever it already had. `cancel_upgrade`
can be called by any single signer, not the full threshold: a minority
should always be able to block an in-flight upgrade it didn't sign off on,
even though it takes the full threshold to push one through.

**Known limitation, on purpose:** the signer set and threshold are immutable
after `initialize` — there is no signer-rotation flow. Rotating signers
safely (without a majority being able to silently lock out a minority, or
vice versa) is its own governance design problem, and shipping it alongside
the upgrade mechanism itself would roughly double the surface being trusted
in one pass. Left as a deliberate follow-up rather than rushed in here.

Each contract's per-error-code table below lists the governance error
variants it inherited from `governance-guard`, at whatever numeric offset
came next in that contract's existing `Error` enum — the variant names are
identical across all five, only the numbers differ.

## Emergency circuit breaker

Jump to: [Pause authorization](#pause-authorization) ·
[Pause entrypoints](#pause-entrypoints) · [Events](#events) ·
[Pause errors](#pause-errors) · [Pause storage layout](#pause-storage-layout) ·
[Which clock](#which-clock) · [Hot-path cost](#hot-path-cost) ·
[Pausing from the CLI](#pausing-from-the-cli)

The same five contracts can be **paused** during an incident. The primitive
lives in `contracts/governance-guard`'s `pausable` module, next to the
upgrade guard and for the same reason: every contract that needs it already
depends on that crate.

A plain `bool paused` flag would be a rug pull waiting to happen, so this one
is built around three properties instead.

**1. Scoped, not global.** A pause names a bitmask of scopes rather than
freezing the contract:

| Scope | Bit | Meaning | Guarded entrypoints |
|---|---|---|---|
| `SCOPE_INTAKE` | `1` | New value or new obligations entering the system | `escrow`: `create_appointment`, `create_milestone_escrow`, `add_milestone` · `loyalty-token`: `mint` · `loyalty-emissions`: `create_schedule` |
| `SCOPE_SETTLEMENT` | `2` | Discretionary happy-path payouts | `escrow`: `confirm_completion`, `approve_milestone`, `release_milestone_funds` · `loyalty-emissions`: `claim` · `settlement-router`: `settle` |
| `SCOPE_ATTESTATION` | `4` | Reputation writes | `reputation`: `submit_attestation` |

`ALL_SCOPES` (`7`) is all three. A contract with no entrypoint in some scope
ignores a pause naming it — pausing `SCOPE_ATTESTATION` on `escrow` is a
well-formed no-op, not an error, so an operator can broadcast one mask to
every contract without special-casing.

**2. Fund-recovery paths are never guarded.** This is the constraint the
whole design exists to satisfy, and it is enforced by *omission* — the
following entrypoints carry no pause check at all, so no scope value,
`ALL_SCOPES` included, can reach them:

| Contract | Always callable, even while paused | Why |
|---|---|---|
| `escrow` | `cancel_appointment`, `raise_dispute`, `resolve_dispute`, `raise_milestone_dispute`, `resolve_milestone_dispute` | Every route by which an escrowed balance reaches whoever is entitled to it. Pausing intake stops new money entering; pausing a refund would create a hostage situation. |
| `loyalty-token` | `transfer`, `transfer_from`, `approve`, `burn` | These move *already-held* balances. A holder's points are their property; only `mint` — the sole path that creates supply that doesn't yet exist — is guarded. |
| `loyalty-emissions` | `reclaim` | Mints and burns nothing; it only marks an allocation as never-to-be-minted, so it can't take a balance anyone holds. |
| all | every read-only view | A consumer must always be able to see current state, including the state that prompted the halt. |

Pausing `SCOPE_SETTLEMENT` does withhold a payout, which deserves an explicit
argument rather than an assumption. It is a delay, not a seizure: it changes
no party's power relative to the others (a client could always cancel a
`Funded` appointment; either party could always force a dispute — both stay
open), and `release_milestone_funds` is *permissionless*, which is exactly
why it has to be haltable. On `loyalty-emissions`, vesting is a pure function
of the ledger clock and keeps accruing through a pause, so a halted `claim`
mints the identical amount afterwards, just later.

**3. Time-bound, enforced on read.** A pause carries an `expires_at` ledger
timestamp, capped at `MAX_PAUSE_DURATION` (7 days). Expiry is evaluated every
time a guard is consulted, so a pause lapses with **no unpause transaction,
no live admin and no working key** — an abandoned pause is indistinguishable
from no pause at all.

What this deliberately does *not* promise: an admin who is present and
hostile can re-pause each time the window lapses. That is not closable here —
the same signer set can already replace all of a contract's code through the
upgrade flow. The honest guarantee is narrower: *an unattended pause always
clears*, and *no pause of any duration can stop a user from recovering funds
they already own*. The second half is what keeps the residual risk a liveness
problem for new business rather than a custody problem for existing balances.

#### Pause authorization

`pause` and `unpause` require **any single governance
signer** — not the full M-of-N threshold, and deliberately not each
contract's own `admin`. Gathering a threshold takes time an incident doesn't
give you, and the action being authorized is bounded on every axis that
matters, so this mirrors `cancel_upgrade`, which is unilateral for the same
reason. Using the signer set rather than `admin` matters because the signer
set is M-of-N, is rotatable through the timelocked flow, and survives the
loss of any one key — an incident is exactly when a single non-rotatable key
is least trustworthy. `unpause` is unilateral in the same way, so a responder
who places a pause and then goes offline cannot wedge it in place.

#### Pause entrypoints

Added to all five contracts:

- `pause(caller: Address, scopes: u32, duration_secs: u64, reason: String) -> PauseState`
- `unpause(caller: Address, scopes: u32) -> u32` — clears only the named
  scopes and returns what's still halted, *without* touching the deadline.
  Re-calling `pause` with a narrower mask would also lift scopes, but restarts
  the clock on everything left; partial `unpause` is how you bring the system
  back a piece at a time.
- `get_pause_state() -> Option<PauseState>` — the active pause as
  `{ scopes, expires_at, paused_by, paused_at, reason }`, or `None` once
  expired. This is the one call an operator or watcher needs: mask and
  deadline in a single read.
- `paused_scopes() -> u32`, `is_paused(scope: u32) -> bool` — narrower
  convenience views over the same record.

`reason` is free-form operator context, may be empty, and is stored and
emitted verbatim — never parsed or compared — so "why is this halted?" is
answerable from chain state instead of from a chat log nobody can find at
3am. It is length-capped because it lives in instance storage that every
subsequent invocation pays to load: an incident ticket reference belongs
on-chain, the write-up does not.

> **The cap is 64 UTF-8 _bytes_, not characters.** `MAX_PAUSE_REASON_LEN`
> bounds what `String::len()` reports, which is the byte length. For ASCII
> the two are identical, which is exactly why this is worth stating: a
> 33-character reason made of two-byte code points is **66 bytes and is
> rejected**, even though a character-count check would have passed it.
> Client-side validation must count bytes — `Buffer.byteLength(s, 'utf8')`
> in JS, `len(s.encode('utf-8'))` in Python — or restrict input to ASCII.
> Both boundaries are pinned by tests
> (`a_multibyte_reason_is_measured_in_bytes_and_accepted_at_exactly_the_cap`
> and `…_over_the_byte_cap_is_rejected_despite_a_short_char_count` in
> `contracts/governance-guard/src/pausable_test.rs`).

#### Events

Two events let off-chain monitoring react. Both shapes are pinned by
assertion in `contracts/escrow/src/test.rs`
(`paused_event_has_the_documented_topics_and_data_shape` and its `unpaused`
counterpart), so this table cannot drift from what is emitted without a test
failing.

Topics are ordered: the two prefix symbols first, then each `#[topic]` field
in declaration order. Data is a **`Map<Symbol, Val>` keyed by field name**
(`data_format` defaults to `"map"`), so field *names* are part of the
contract but their order is not — index by key, not by position.

| Event | Topics (in order) | Data map |
|---|---|---|
| `Paused` | `Symbol("gov_pause")`, `Symbol("paused")`, `Address(caller)` | `expires_at: u64`, `reason: String`, `scopes: u32` |
| `Unpaused` | `Symbol("gov_pause")`, `Symbol("unpaused")`, `Address(caller)` | `scopes: u32`, `remaining_scopes: u32` |

`scopes` on `Paused` is the full set halted *after* the call — the new state,
not a delta. On `Unpaused` it is the set that call *cleared*, with
`remaining_scopes` the set still halted afterwards (`0` when fully lifted).

As an indexer would see them, after
`pause(signer, SCOPE_INTAKE, 7200, "INC-412")` at ledger timestamp `1000`,
then `unpause(signer, SCOPE_INTAKE)`:

```jsonc
// Paused
{
  "contract": "C…ESCROW",
  "topics": ["gov_pause", "paused", "G…SIGNER"],
  "data": { "scopes": 1, "expires_at": 8200, "reason": "INC-412" }
}
// Unpaused
{
  "contract": "C…ESCROW",
  "topics": ["gov_pause", "unpaused", "G…SIGNER"],
  "data": { "scopes": 1, "remaining_scopes": 0 }
}
```

**Auto-expiry emits nothing.** It is a read-time evaluation with no
transaction behind it, so there is no execution context to emit from — which
is the same property that makes expiry trustworthy in the first place.
Monitors should treat the `expires_at` carried by `Paused` as the
authoritative end of a window unless an `Unpaused` arrives sooner, and must
not wait for an event that will never come.

#### Pause errors

Five variants per contract, at whatever offset came next in that
contract's existing `Error` enum — identical names, different numbers:

| Variant | `escrow` | `reputation` | `loyalty-token` | `loyalty-emissions` | `settlement-router` | Meaning |
|---|---|---|---|---|---|---|
| `OperationPaused` | 37 | 29 | 24 | 30 | 23 | The entrypoint's scope is currently halted. A dedicated variant rather than a reused `InvalidStatus`: "the protocol is halted, retry later" and "this request was never valid" call for opposite reactions from a client. |
| `InvalidPauseScope` | 38 | 30 | 25 | 31 | 24 | The scope mask was empty or contained bits outside `ALL_SCOPES`. Empty is rejected rather than treated as a no-op — during an incident a mask that halts nothing is a mistake the operator wants to hear about. |
| `InvalidPauseDuration` | 39 | 31 | 26 | 32 | 25 | The duration was `0` or exceeded `MAX_PAUSE_DURATION`. |
| `NotPaused` | 40 | 32 | 27 | 33 | 26 | `unpause` with nothing in effect, including a record that already auto-expired. |
| `InvalidPauseReason` | 41 | 33 | 28 | 34 | 27 | The `reason` exceeded `MAX_PAUSE_REASON_LEN` (64 bytes). |

A non-signer calling `pause`/`unpause` gets the existing `NotASigner`.

#### Pause storage layout

One instance-storage entry, `GovernanceDataKey::PauseState`,
holding `PauseState { scopes, expires_at, paused_by, paused_at, reason }`.
Appended after `PendingRotation` so already-deployed contracts' key encodings
stay put. Only one record exists at a time — a second `pause` replaces the
first outright rather than layering — so the halted scopes and the deadline
are always readable from one place. `paused_by` is recorded for attribution
and grants no rights: any signer may lift a pause, not just the one who
placed it.

#### Which clock

`expires_at` is compared against `env.ledger().timestamp()`
— Stellar's ledger close time in seconds, agreed by SCP consensus and
required to be monotonic, not a value any single validator picks. The
manipulation surface is small and points the harmless way: nudging the clock
forward can only end a pause *sooner*, backward isn't possible, and no
fund-recovery path consults a clock at all. Wall-clock seconds rather than
ledger sequence (which the upgrade timelocks use) because a pause duration is
negotiated between humans mid-incident — "give us six hours" — and seconds say
that directly.

#### Hot-path cost

The guard runs on every guarded entrypoint, so its cost is measured rather
than assumed, using the SDK's budget metering. Tests live in
`contracts/escrow/src/test.rs` under "hot-path cost".

| Measurement | CPU instructions | Δ |
|---|---:|---:|
| Trivial instance-storage view (`get_storage_version`) — the floor | 51,549 | — |
| `paused_scopes()`, **no pause record** | 51,717 | **+168** |
| `paused_scopes()`, live pause record | 77,023 | +25,474 |
| `create_appointment`, **no pause record** | 336,299 | — |
| `create_appointment`, live record on another scope | 365,219 | +8.6% |
| `create_appointment` rejected while paused | 78,216 | 23% of the above |

> ⚠️ **These are SDK-test-metered numbers, not absolute Wasm costs or fees.**
> Per the SDK's own documentation, native Rust test execution underestimates
> both CPU and memory relative to the compiled Wasm, and this harness charges
> no Wasm instantiation. They are meaningful as a *relative* comparison of
> the same call with and without a pause record — which is the question being
> asked — and should not be used to size a transaction fee.

The shape that matters: **in normal operation — no pause ever set, which is
the state the contracts are in essentially always — the guard costs ~168
instructions against a ~336,000-instruction booking.** That is a rounding
error. The ~25,000 figure is deserializing the `PauseState` record, and is
paid only *while an incident is in progress*, which is precisely when
degraded throughput is the point. It is also why `reason` is length-capped:
the cap is what keeps the incident-time cost bounded too.

A rejected call costs about 23% of the work it replaces, because the guard is
the first statement of each guarded entrypoint, ahead of auth and every other
storage access. That makes a pause a usable response to an entrypoint being
hammered rather than an amplifier.

No micro-optimization is warranted at these numbers. The guard is one
instance-storage read plus two integer comparisons, and the instance entry is
already in the footprint of any invocation that touches admin or governance
state — so it is a hit on an entry the host has loaded regardless, not an
extra ledger read. The tests fence *relative* behaviour (a deliberately loose
"must not approach doubling") rather than absolute numbers, so an SDK bump
doesn't produce a brittle CI failure.

#### Pausing from the CLI

```sh
# Halt new bookings for 6 hours (any one governance signer)
stellar contract invoke --id $ESCROW --source signer1 --network testnet \
  -- pause --caller $SIGNER_1 --scopes 1 --duration_secs 21600 \
     --reason "INC-412 milestone accounting"

# Halt everything the breaker can reach, for the 7-day maximum
stellar contract invoke --id $ESCROW --source signer1 --network testnet \
  -- pause --caller $SIGNER_1 --scopes 7 --duration_secs 604800 --reason "INC-412"

# Refunds and disputes keep working throughout — no scope reaches them
stellar contract invoke --id $ESCROW --source client --network testnet \
  -- cancel_appointment --appointment_id 1

# Bring intake back early, leaving settlement halted on its original deadline
stellar contract invoke --id $ESCROW --source signer2 --network testnet \
  -- unpause --caller $SIGNER_2 --scopes 1

# What's halted right now, until when, and why
stellar contract invoke --id $ESCROW --source signer1 --network testnet \
  -- get_pause_state
```

**Broadcasting to every contract.** The scope vocabulary is shared, so one
mask goes to all five — including `reputation`, which has no intake
entrypoint, since a scope a contract doesn't use is a no-op rather than an
error. The five are separate deployments with separate storage, so this is
N transactions, not one: they needn't land in the same ledger, and a partial
sweep is a valid state rather than a corrupt one, because each contract's
guard reads only its own record. `scripts/broadcast-pause.sh` does the sweep:

```sh
export ESCROW=… REPUTATION=… LOYALTY_TOKEN=… LOYALTY_EMISSIONS=… SETTLEMENT_ROUTER=…
SIGNER=my-key ./scripts/broadcast-pause.sh pause 7 21600 "INC-412 triage"
SIGNER=my-key ./scripts/broadcast-pause.sh status
SIGNER=my-key ./scripts/broadcast-pause.sh unpause 1
```

Note each contract has its **own** governance signer set; if they differ, run
the script once per key with only the matching ids exported.

## Contract Events

Every state-changing entry point across the three core contracts (escrow,
reputation, loyalty-token) now emits exactly one event on success. A failed
operation that returns an `Error` emits **no event**. Topic tuples are chosen
so an off-chain indexer can filter by `appointment_id`, `worker`, or `client`
address without scanning every ledger.

Topics are ordered: the two prefix symbols first, then each `#[topic]` field
in declaration order. Data is a **`Map<Symbol, Val>` keyed by field name**
(`data_format` defaults to `"map"`), so field *names* are part of the
contract but their order is not — index by key, not by position.

### Escrow events

| Event | Topics (in order) | Data fields |
|---|---|---|
| `AppointmentCreated` | `"escrow"`, `"created"`, `appointment_id`, `client`, `worker` | `amount` |
| `AppointmentCompleted` | `"escrow"`, `"completed"`, `appointment_id`, `client` | `worker` |
| `AppointmentCancelled` | `"escrow"`, `"cancelled"`, `appointment_id`, `client` | `amount` |
| `AppointmentDisputed` | `"escrow"`, `"disputed"`, `appointment_id`, `caller` | `client`, `worker` |
| `AppointmentResolved` | `"escrow"`, `"resolved"`, `appointment_id`, `recipient` | `amount`, `refund_to_client` |
| `MilestoneCreated` | `"escrow"`, `"milestone_created"`, `escrow_id`, `client` | `index`, `amount`, `deadline` |
| `MilestoneApproved` | `"escrow"`, `"milestone_approved"`, `escrow_id`, `client` | `milestone_index` |
| `MilestoneReleased` | `"escrow"`, `"milestone_released"`, `escrow_id`, `worker` | `milestone_index`, `amount` |
| `MilestoneDisputed` | `"escrow"`, `"milestone_disputed"`, `escrow_id`, `caller` | `milestone_index` |
| `MilestoneResolved` | `"escrow"`, `"milestone_resolved"`, `escrow_id`, `recipient` | `milestone_index`, `amount` |
| `MilestoneEscrowCreated` | `"escrow"`, `"milestone_escrow_created"`, `escrow_id`, `client`, `worker` | `total_amount` |

### Reputation events

| Event | Topics (in order) | Data fields |
|---|---|---|
| `AttestationSubmitted` | `"rep"`, `"attested"`, `appointment_id`, `client`, `worker` | `rating` |

### Loyalty token events

| Event | Topics (in order) | Data fields |
|---|---|---|
| `Mint` | `"loyalty"`, `"mint"`, `to` | `amount` |
| `Transfer` | `"loyalty"`, `"transfer"`, `from`, `to` | `amount` |
| `Burn` | `"loyalty"`, `"burn"`, `from` | `amount` |
| `Approve` | `"loyalty"`, `"approve"`, `from`, `spender` | `amount`, `expiration_ledger` |
| `MinterRotated` | `"loyalty"`, `"minter_rotated"`, `old_minter`, `new_minter` | _(none)_ |

`Transfer`, `Burn` and `Approve` follow SEP-41 conventions so standard wallet and
indexer tooling recognizes them. `Mint` uses the same prefix pair for
consistency but is not defined by SEP-41.

### Indexer filtering examples

An off-chain indexer can filter by any topic position:

- **By appointment:** filter topics[2] == `appointment_id` on escrow/reputation
  events.
- **By worker:** filter topics[3] or topics[4] == `worker` address on escrow
  events; topics[3] on `AttestationSubmitted`.
- **By client:** filter topics[2] or topics[3] == `client` address on escrow
  events; topics[2] on `AttestationSubmitted`.
- **By token holder:** filter topics[2] == `to`/`from` on loyalty-token events.

All event shapes are pinned by assertion in each contract's test suite, so
the topic layout and data fields cannot drift from this documentation without
a test failing.

## Prerequisites

- Rust with the `wasm32v1-none` target: `rustup target add wasm32v1-none`
  (Soroban does not yet support `wasm32-unknown-unknown` on Rust 1.82+;
  use `wasm32v1-none` with Rust 1.84+, or Rust 1.81 or earlier for the old target.)
- [Stellar CLI](https://developers.stellar.org/docs/tools/cli/install-cli) (`stellar` binary), used for deploying/invoking contracts.

## Build

```sh
stellar contract build
# or, per-contract:
cargo build --target wasm32v1-none --release -p guildworkman-escrow
```

Wasm output lands in `target/wasm32v1-none/release/*.wasm`.

## Test

```sh
cargo test --workspace
```

Each contract has unit tests under `contracts/<name>/src/test.rs` using
`soroban-sdk`'s `testutils`. Current coverage:

- **escrow** (66 tests): happy-path completion pays the worker and drains the
  contract's balance; cancellation refunds the client; a raised dispute
  resolved in the worker's favor pays the worker; creating a duplicate
  `appointment_id` is rejected; confirming an already-completed appointment
  is rejected; milestone creation, approval, release, dispute, and resolution;
  milestone escrow creation and fund release; hot-path cost measurements;
  pause/unpause lifecycle and scoped guard behavior; and structured contract
  event assertions for every state-changing entry point.
- **reputation** (45 tests): submitting reviews updates the count/sum
  aggregate and average correctly; reviewing the same `appointment_id` twice
  is rejected; a rating outside 1-5 is rejected; stake weighting and time
  decay; rate limiting; admin stake management; pause/unpause lifecycle;
  structured contract event assertions; and adversarial edge cases.
- **loyalty-token** (27 tests): mint increases balance; transfer moves balance
  between accounts; transferring more than the balance fails; approve +
  transfer_from spends down the allowance correctly; burn reduces balance;
  the admin can rotate the minter and the new minter can mint; pause/unpause
  lifecycle; structured contract event assertions for mint, transfer, burn,
  and minter rotation; and adversarial edge cases.
- **loyalty-emissions** (43 tests): linear vesting reports the right amount at
  the start, midpoint, and end of a stream and stays capped afterwards; a
  cliff blocks vesting until it's reached; `claim` mints the vested delta and
  incremental claims only mint what's newly vested; per-account and global
  rate limits clamp a claim to the remaining window budget and open a fresh
  budget the next window; the `claimable` view reflects both vesting and rate
  limits; `reclaim` returns the unclaimed remainder only after the deadline and
  blocks further claims; pause/unpause lifecycle; structured contract event
  assertions; and adversarial paths — double-init, bad config, bad
  schedule params, reclaim-before-vesting-end, double-claim, double-reclaim,
  claiming a missing/reclaimed schedule, and looping claims across many windows
  never mints more than a schedule's `total`.
- **settlement-router** (14 tests): a `settle` call atomically releases
  escrowed funds, records the attestation, and mints loyalty to both parties
  in one deployment wiring real `escrow`/`reputation`/`loyalty-token`
  contracts together; a replay is rejected without a double release or mint;
  settling an appointment that isn't `Funded` (missing, already completed
  outside the router, cancelled, disputed) is rejected before any
  cross-contract call; a pause on either sub-contract — or on the router's
  own `SCOPE_SETTLEMENT` — reverts the whole invocation with nothing
  committed; a negative `RewardConfig` is rejected at `initialize` and at
  `set_reward_config`; a `0` reward skips that party's mint; and
  `reputation.submit_attestation` rejects a direct caller lacking the
  configured router's own authorization once `set_router` is wired in.
- **dispute-resolution** (27 tests): the full commit → reveal → resolve →
  withdraw lifecycle pays a plaintiff- or defendant-majority jury out of the
  losers' stakes; a no-show who committed but never revealed is slashed and
  their stake flows to the winners; a tie or a below-quorum turnout refunds
  every staker (including non-revealers) with no slashing; the state machine
  rejects committing/revealing/resolving/withdrawing out of phase; and
  adversarial paths — double-init, bad config, duplicate/same-party disputes,
  double commit/reveal/resolve/withdraw, revealing with the wrong vote or salt,
  a copycat replaying another juror's commitment being unable to reveal it, and
  a slashed loser never draining the pot via repeated withdrawals.

These are unit tests against the in-process Soroban test host
(`Env::default()` + `mock_all_auths()`), not integration tests against a real
network — they don't cover cross-contract calls, real fee/resource limits, or
multi-node consensus behavior.

## Deploy (testnet example)

```sh
stellar keys generate deployer --network testnet --fund

stellar contract deploy \
  --wasm target/wasm32v1-none/release/guildworkman_escrow.wasm \
  --source deployer --network testnet

stellar contract invoke \
  --id <ESCROW_CONTRACT_ID> --source deployer --network testnet \
  -- initialize --admin <ADMIN_ADDRESS>
```

Repeat `deploy` for `guildworkman_reputation.wasm`,
`guildworkman_loyalty_token.wasm`, `guildworkman_loyalty_emissions.wasm`,
`guildworkman_settlement_router.wasm`, and
`guildworkman_dispute_resolution.wasm`, then call each contract's `initialize`
once. For the emission engine to be
able to mint, point it at the token in its `initialize` and then hand it the
token's `minter` role:

```sh
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --token $LOYALTY_CONTRACT_ID \
     --config '{ "window": 17280, "account_cap": "1000", "global_cap": "100000" }'

# Hand the token's minter role to the emissions contract.
stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- set_minter --new_minter $EMISSIONS
```

To wire `settlement-router` in instead (see
[settlement-router](#settlement-router) for the full "Deploying under
partial rollout" order — this re-points `minter` away from
`loyalty-emissions` above, so pick one mint authority per deployment):

```sh
stellar contract invoke --id $ROUTER --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --escrow $ESCROW --reputation $REPUTATION \
     --loyalty_token $LOYALTY --reward_config '{ "client_reward": "50", "worker_reward": "100" }' \
     --governance_init '{"signers":["'$SIGNER_1'","'$SIGNER_2'","'$SIGNER_3'"],"threshold":2}'

# Hand the token's minter role to the router.
stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- set_minter --new_minter $ROUTER

# Reputation only accepts an attestation the router vouches for from here on.
stellar contract invoke --id $REPUTATION --source admin --network testnet \
  -- set_router --router $ROUTER
```

## Contract interfaces

### escrow

- `initialize(admin: Address, governance_init: GovernanceInit)`
- `create_appointment(appointment_id: u64, client: Address, worker: Address, token: Address, amount: i128)`
- `confirm_completion(appointment_id: u64)` — client-only, pays the worker
- `cancel_appointment(appointment_id: u64)` — client-only, refunds the client
- `raise_dispute(appointment_id: u64, caller: Address)` — client or worker
- `resolve_dispute(appointment_id: u64, refund_to_client: bool)` — admin-only
- `get_appointment(appointment_id: u64) -> Appointment`
- `propose_upgrade`, `approve_upgrade`, `cancel_upgrade`, `migrate`, `get_signers`, `get_upgrade_threshold`, `get_pending_upgrade`, `get_storage_version` — see [Upgrade governance](#upgrade-governance)
- `pause`, `unpause`, `get_pause_state`, `paused_scopes`, `is_paused` — see [Emergency circuit breaker](#emergency-circuit-breaker)

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Admin` | instance | The dispute arbiter's `Address`, set once in `initialize`. |
| `Appointment(u64)` | persistent | An `Appointment { client, worker, token, amount, status }` keyed by `appointment_id`. `status` is one of `Funded`, `Completed`, `Cancelled`, `Disputed`, `Resolved`. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `AlreadyInitialized` | 1 | `initialize` called more than once. |
| `NotInitialized` | 2 | `resolve_dispute` called before `initialize`. |
| `AppointmentExists` | 3 | `create_appointment` reused an existing `appointment_id`. |
| `AppointmentNotFound` | 4 | No appointment stored under that `appointment_id`. |
| `InvalidStatus` | 5 | The requested transition doesn't apply to the appointment's current status (e.g. confirming a non-`Funded` appointment). |
| `InvalidAmount` | 6 | `amount <= 0` in `create_appointment`. |
| `NotAParticipant` | 7 | `raise_dispute` called by an address that is neither the client nor the worker. |
| `GovernanceAlreadyInitialized` | 8 | `initialize` called more than once (surfaced via the governance guard). |
| `GovernanceNotInitialized` | 9 | A governance call before `initialize`. |
| `InvalidThreshold` | 10 | `threshold` is `0` or exceeds the number of signers. |
| `DuplicateSigner` | 11 | The same address appears twice in `signers`. |
| `NotASigner` | 12 | `propose_upgrade`/`approve_upgrade`/`cancel_upgrade`/`migrate` called by a non-signer. |
| `NoPendingUpgrade` | 13 | `approve_upgrade`/`cancel_upgrade` with nothing proposed. |
| `AlreadyApproved` | 14 | The same signer approving the same proposal twice. |
| `ProposalExpired` | 15 | `approve_upgrade` more than ~7 days after `propose_upgrade`. |
| `HashMismatch` | 16 | `approve_upgrade` with a hash that doesn't match the pending proposal. |
| `AlreadyMigrated` | 17 | `migrate` targeting a version already applied or behind the current one. |
| `NothingToMigrate` | 18 | `migrate` called when the stored version is already current. |

Codes 19-36 (milestone escrow and signer rotation) are documented in
`src/lib.rs`; codes 37-41 are the circuit breaker's, listed in
[Emergency circuit breaker](#emergency-circuit-breaker).

#### CLI usage

```sh
# One-time setup — a 2-of-3 signer governance guard on top of the admin
stellar contract invoke --id $ESCROW --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR \
     --governance_init '{"signers":["'$SIGNER_1'","'$SIGNER_2'","'$SIGNER_3'"],"threshold":2}'

# Client books worker WORKER_ADDR, depositing 10000 units of TOKEN_ADDR, appointment id 1
stellar contract invoke --id $ESCROW --source client --network testnet \
  -- create_appointment --appointment_id 1 --client $CLIENT_ADDR \
     --worker $WORKER_ADDR --token $TOKEN_ADDR --amount 10000

# Client confirms the job is done -> pays the worker
stellar contract invoke --id $ESCROW --source client --network testnet \
  -- confirm_completion --appointment_id 1

# Client cancels before completion -> refunds the client
stellar contract invoke --id $ESCROW --source client --network testnet \
  -- cancel_appointment --appointment_id 1

# Either party raises a dispute
stellar contract invoke --id $ESCROW --source client --network testnet \
  -- raise_dispute --appointment_id 1 --caller $CLIENT_ADDR

# Admin resolves the dispute in the worker's favor
stellar contract invoke --id $ESCROW --source admin --network testnet \
  -- resolve_dispute --appointment_id 1 --refund_to_client false

# Read appointment state
stellar contract invoke --id $ESCROW --source admin --network testnet \
  -- get_appointment --appointment_id 1
```

### reputation

> **This section is stale and predates this change** — it describes a
> simpler `submit_review`/`Rating{count,sum}` interface that doesn't match
> `contracts/reputation/src/lib.rs`, which actually has `Config`,
> `submit_attestation` with stake weighting and time decay, admin-managed
> stake, and rate limiting. That drift already existed before this PR and
> rewriting it is unrelated to #16, so it's left alone rather than folded in
> here — flagging it instead of silently leaving it wrong. What *is* true as
> of this PR: `initialize` now also takes a `governance_init: GovernanceInit`,
> and the contract has the same `propose_upgrade`/`approve_upgrade`/
> `cancel_upgrade`/`migrate` surface described in
> [Upgrade governance](#upgrade-governance), plus the `pause`/`unpause`
> surface described in
> [Emergency circuit breaker](#emergency-circuit-breaker) — see the source
> and `src/test.rs` for the actual current interface.
>
> This PR adds one more piece on top of that already-drifted interface:
> `set_router(router: Address)` (admin-only) and `get_router() -> Option<Address>`.
> Once a router is set, `submit_attestation` additionally requires that
> router's own authorization alongside the client's — see
> [settlement-router](#settlement-router) for why, and for the `Router`
> storage key this adds (instance, holds an `Option<Address>`, defaults
> unset).

- `submit_review(appointment_id: u64, client: Address, worker: Address, rating: u32, comment: String)` — 1-5 stars, one review per `appointment_id`
- `get_rating(worker: Address) -> Rating { count, sum }`
- `get_average_rating_x100(worker: Address) -> u32` — e.g. `437` = 4.37 stars
- `get_review(worker: Address, index: u32) -> Option<Review>`
- `get_review_count(worker: Address) -> u32`

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Reviewed(u64)` | persistent | A `bool` flag per `appointment_id`, used only to enforce one review per appointment. |
| `Rating(Address)` | persistent | A `Rating { count, sum }` aggregate per worker; `sum` is the running total of all star ratings given. |
| `Review(Address, u32)` | persistent | An individual `Review { client, rating, comment }`, keyed by `(worker, index)` where `index` is a per-worker sequence number starting at 0. |
| `ReviewCount(Address)` | persistent | The next free `index` for a given worker — also doubles as the total review count. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `InvalidRating` | 1 | `rating` outside the 1-5 range. |
| `AlreadyReviewed` | 2 | `submit_review` called twice for the same `appointment_id`. |
| `NoReviews` | 3 | `get_average_rating_x100` called for a worker with zero reviews (avoids a divide-by-zero). |

#### CLI usage

```sh
stellar contract invoke --id $REPUTATION --source client --network testnet \
  -- submit_review --appointment_id 1 --client $CLIENT_ADDR \
     --worker $WORKER_ADDR --rating 5 --comment "Great job, on time"

stellar contract invoke --id $REPUTATION --source admin --network testnet \
  -- get_rating --worker $WORKER_ADDR

stellar contract invoke --id $REPUTATION --source admin --network testnet \
  -- get_average_rating_x100 --worker $WORKER_ADDR

stellar contract invoke --id $REPUTATION --source admin --network testnet \
  -- get_review --worker $WORKER_ADDR --index 0

stellar contract invoke --id $REPUTATION --source admin --network testnet \
  -- get_review_count --worker $WORKER_ADDR
```

### loyalty-token

- `initialize(admin: Address, minter: Address, decimals: u32, name: String, symbol: String, governance_init: GovernanceInit)`
- `set_minter(new_minter: Address)` — admin-only
- `mint(to: Address, amount: i128)` — minter-only
- `transfer`, `transfer_from`, `approve`, `allowance`, `burn`, `balance`, `decimals`, `name`, `symbol` — standard SEP-41 token surface
- `propose_upgrade`, `approve_upgrade`, `cancel_upgrade`, `migrate`, `get_signers`, `get_upgrade_threshold`, `get_pending_upgrade`, `get_storage_version` — see [Upgrade governance](#upgrade-governance)
- `pause`, `unpause`, `get_pause_state`, `paused_scopes`, `is_paused` — see [Emergency circuit breaker](#emergency-circuit-breaker)

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Admin` | instance | The admin `Address`, allowed to call `set_minter`. |
| `Minter` | instance | The only `Address` allowed to call `mint`. |
| `Metadata` | instance | `TokenMetadata { decimals, name, symbol }`. |
| `Balance(Address)` | persistent | The `i128` token balance for a given holder. |
| `Allowance(Address, Address)` | temporary | An `AllowanceValue { amount, expiration_ledger }` for `(from, spender)`, cleared once `expiration_ledger` passes. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `AlreadyInitialized` | 1 | `initialize` called more than once. |
| `NotInitialized` | 2 | `mint`/`set_minter` called before `initialize`. |
| `InsufficientBalance` | 3 | `transfer`/`transfer_from`/`burn` amount exceeds the sender's balance. |
| `InsufficientAllowance` | 4 | `transfer_from` amount exceeds the spender's remaining allowance. |
| `InvalidAmount` | 5 | A negative/zero amount passed where a positive amount is required. |
| `NotAuthorized` | 6 | Reserved for authorization failures; current checks rely on `require_auth()` panicking directly rather than returning this variant. |
| `GovernanceAlreadyInitialized` | 7 | `initialize` called more than once (surfaced via the governance guard). |
| `GovernanceNotInitialized` | 8 | A governance call before `initialize`. |
| `InvalidThreshold` | 9 | `threshold` is `0` or exceeds the number of signers. |
| `DuplicateSigner` | 10 | The same address appears twice in `signers`. |
| `NotASigner` | 11 | `propose_upgrade`/`approve_upgrade`/`cancel_upgrade`/`migrate` called by a non-signer. |
| `NoPendingUpgrade` | 12 | `approve_upgrade`/`cancel_upgrade` with nothing proposed. |
| `AlreadyApproved` | 13 | The same signer approving the same proposal twice. |
| `ProposalExpired` | 14 | `approve_upgrade` more than ~7 days after `propose_upgrade`. |
| `HashMismatch` | 15 | `approve_upgrade` with a hash that doesn't match the pending proposal. |
| `AlreadyMigrated` | 16 | `migrate` targeting a version already applied or behind the current one. |
| `NothingToMigrate` | 17 | `migrate` called when the stored version is already current. |

Codes 18-23 (signer rotation) are documented in `src/lib.rs`; codes 24-28 are
the circuit breaker's, listed in
[Emergency circuit breaker](#emergency-circuit-breaker).

#### CLI usage

```sh
stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --minter $MINTER_ADDR \
     --decimals 2 --name '"GuildWorkman Points"' --symbol '"GWP"' \
     --governance_init '{"signers":["'$SIGNER_1'","'$SIGNER_2'","'$SIGNER_3'"],"threshold":2}'

stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- set_minter --new_minter $NEW_MINTER_ADDR

stellar contract invoke --id $LOYALTY --source minter --network testnet \
  -- mint --to $CLIENT_ADDR --amount 500

stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- balance --id $CLIENT_ADDR

stellar contract invoke --id $LOYALTY --source client --network testnet \
  -- transfer --from $CLIENT_ADDR --to $WORKER_ADDR --amount 100

stellar contract invoke --id $LOYALTY --source client --network testnet \
  -- approve --from $CLIENT_ADDR --spender $SPENDER_ADDR \
     --amount 200 --expiration_ledger 5000000

stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- allowance --from $CLIENT_ADDR --spender $SPENDER_ADDR

stellar contract invoke --id $LOYALTY --source spender --network testnet \
  -- transfer_from --spender $SPENDER_ADDR --from $CLIENT_ADDR \
     --to $WORKER_ADDR --amount 150

stellar contract invoke --id $LOYALTY --source client --network testnet \
  -- burn --from $CLIENT_ADDR --amount 50

stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- decimals
stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- name
stellar contract invoke --id $LOYALTY --source admin --network testnet \
  -- symbol
```

### loyalty-emissions

An emission engine layered on top of `loyalty-token`. It holds the token's
`minter` role and releases rewards as **linear vesting streams** rather than
lump-sum mints, gated by anti-abuse rate limits, with an admin reclaim path for
allocations left unclaimed past a deadline.

- `initialize(admin: Address, token: Address, config: Config, governance_init: GovernanceInit)` —
  `config` is `{ window: u32, account_cap: i128, global_cap: i128 }`
- `create_schedule(beneficiary: Address, total: i128, start: u32, cliff: u32, duration: u32, reclaimable_after: u32)` — admin-only; `start: 0` means "start now"
- `claim(beneficiary: Address) -> i128` — beneficiary-authorized; mints the
  vested-and-unclaimed amount clamped to the current rate-limit budget, returns
  the amount minted
- `reclaim(beneficiary: Address) -> i128` — admin-only; after `reclaimable_after`,
  marks the schedule reclaimed and returns the never-minted remainder
- `vested(beneficiary) -> i128`, `claimable(beneficiary) -> i128`,
  `get_schedule(beneficiary) -> Schedule`, `get_config() -> Config`,
  `get_admin() -> Address`, `get_token() -> Address` — read-only views
- `propose_upgrade`, `approve_upgrade`, `cancel_upgrade`, `migrate`, `get_signers`, `get_upgrade_threshold`, `get_pending_upgrade`, `get_storage_version` — see [Upgrade governance](#upgrade-governance)
- `pause`, `unpause`, `get_pause_state`, `paused_scopes`, `is_paused` — see [Emergency circuit breaker](#emergency-circuit-breaker)

#### Vesting & rate-limit model

A schedule is `{ total, start, cliff, duration }`. At ledger `t`:

- before `start + cliff` → nothing vested;
- `start + cliff` .. `start + duration` → `vested = total * (t - start) / duration`;
- at/after `start + duration` → fully vested (`total`).

`claimable = vested - claimed`, then clamped to the smaller of the account's and
the global remaining budget for the current window. Windows are fixed tumbling
windows of `window` ledgers (`ledger / window`): each account may claim at most
`account_cap` per window, and all accounts combined at most `global_cap` per
window. `create_schedule` requires `reclaimable_after >= start + duration`, so
the admin can never reclaim allocations that are still vesting.

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Admin` | instance | The admin `Address`; the only caller allowed to `create_schedule`/`reclaim`. |
| `Token` | instance | The `loyalty-token` contract `Address` this engine mints through. |
| `Config` | instance | `Config { window, account_cap, global_cap }`, fixed at `initialize`. |
| `Schedule(Address)` | persistent | A `Schedule { total, claimed, start, cliff, duration, reclaimable_after, reclaimed }` per beneficiary. |
| `AccountWindow(Address, u64)` | temporary | Units a given account has claimed in a given window index; enforces the per-account cap. Auto-expires ~2 windows after use. |
| `GlobalWindow(u64)` | temporary | Units all accounts have claimed in a given window index; enforces the global cap. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `AlreadyInitialized` | 1 | `initialize` called more than once. |
| `NotInitialized` | 2 | A method needing state was called before `initialize`. |
| `InvalidConfig` | 3 | `window == 0` or a non-positive cap passed to `initialize`. |
| `InvalidSchedule` | 4 | `total <= 0`, `duration == 0`, `cliff > duration`, or `reclaimable_after < start + duration`. |
| `ScheduleExists` | 5 | `create_schedule` for a beneficiary that already has one. |
| `ScheduleNotFound` | 6 | No schedule stored for that beneficiary. |
| `NothingToClaim` | 7 | Nothing has vested beyond what's already claimed. |
| `AccountRateLimited` | 8 | The account's per-window budget is already exhausted. |
| `GlobalRateLimited` | 9 | The global per-window budget is already exhausted. |
| `NotYetReclaimable` | 10 | `reclaim` called before `reclaimable_after`. |
| `AlreadyReclaimed` | 11 | `claim`/`reclaim` on an already-reclaimed schedule. |
| `NothingToReclaim` | 12 | `reclaim` when the schedule was already fully claimed. |
| `GovernanceAlreadyInitialized` | 13 | `initialize` called more than once (surfaced via the governance guard). |
| `GovernanceNotInitialized` | 14 | A governance call before `initialize`. |
| `InvalidThreshold` | 15 | `threshold` is `0` or exceeds the number of signers. |
| `DuplicateSigner` | 16 | The same address appears twice in `signers`. |
| `NotASigner` | 17 | `propose_upgrade`/`approve_upgrade`/`cancel_upgrade`/`migrate` called by a non-signer. |
| `NoPendingUpgrade` | 18 | `approve_upgrade`/`cancel_upgrade` with nothing proposed. |
| `AlreadyApproved` | 19 | The same signer approving the same proposal twice. |
| `ProposalExpired` | 20 | `approve_upgrade` more than ~7 days after `propose_upgrade`. |
| `HashMismatch` | 21 | `approve_upgrade` with a hash that doesn't match the pending proposal. |
| `AlreadyMigrated` | 22 | `migrate` targeting a version already applied or behind the current one. |
| `NothingToMigrate` | 23 | `migrate` called when the stored version is already current. |

Codes 24-29 (signer rotation) are documented in `src/lib.rs`; codes 30-34 are
the circuit breaker's, listed in
[Emergency circuit breaker](#emergency-circuit-breaker).

#### Authorization & safety notes

- **Trust root is the admin.** Only the admin can register or reclaim
  schedules. `claim` is authorized by the beneficiary themselves.
- **The engine, not the backend, is the token's `minter`.** After deployment
  the admin must call `loyalty-token.set_minter` with the emission contract's
  address; the token then only mints through vesting + rate-limit checks.
- **Reclaim cannot rug a vesting stream.** Because `reclaimable_after` is forced
  to be at or after the vesting end, the admin can only reclaim what a
  beneficiary chose not to claim after the stream fully vested — never
  still-vesting funds.
- **Reclaim is un-mint, not burn.** Reclaimed units were never minted, so no
  balance is touched; the schedule is simply closed.

#### CLI usage

```sh
# One-time setup (see the Deploy section for wiring the minter role).
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --token $LOYALTY_CONTRACT_ID \
     --config '{ "window": 17280, "account_cap": "1000", "global_cap": "100000" }' \
     --governance_init '{"signers":["'$SIGNER_1'","'$SIGNER_2'","'$SIGNER_3'"],"threshold":2}'

# Admin registers a stream: 5000 points vesting over 30 days (starting now),
# reclaimable after ~31 days if left unclaimed.
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- create_schedule --beneficiary $WORKER_ADDR --total 5000 \
     --start 0 --cliff 0 --duration 518400 --reclaimable_after 535680

# Worker claims whatever has vested and fits the rate limit.
stellar contract invoke --id $EMISSIONS --source worker --network testnet \
  -- claim --beneficiary $WORKER_ADDR

# Read-only views.
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- vested --beneficiary $WORKER_ADDR
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- claimable --beneficiary $WORKER_ADDR

# Admin reclaims the unclaimed remainder after the deadline.
stellar contract invoke --id $EMISSIONS --source admin --network testnet \
  -- reclaim --beneficiary $WORKER_ADDR
```

### settlement-router

Orchestrates `escrow`, `reputation` and `loyalty-token` atomically. Before
this contract, those three were independent doors: `escrow::confirm_completion`
released funds and stopped there; `reputation::submit_attestation` accepted
*any* `appointment_id` with no proof it was ever funded or completed;
`loyalty-token::mint` trusted whichever address held the `minter` role. A
client could review a worker for an appointment that never happened, and
loyalty points were only as trustworthy as the backend's private key.

A single `settle(appointment_id, rating, attestation_hash)` call now proves
the appointment is `Funded` in `escrow`, then drives all three effects in one
transaction: any `Err` from a sub-contract call — or a pause on its side —
aborts the whole invocation, so a completed appointment settles as one
indivisible unit (funds released, review recorded, loyalty minted) or none
of it happens.

- `initialize(admin: Address, escrow: Address, reputation: Address, loyalty_token: Address, reward_config: RewardConfig, governance_init: GovernanceInit)` —
  `reward_config` is `{ client_reward: i128, worker_reward: i128 }`, the
  fixed loyalty amounts minted on settlement (never taken from a `settle`
  caller's own arguments)
- `settle(appointment_id: u64, rating: u32, attestation_hash: BytesN<32>) -> ()` —
  **permissionless caller**; the appointment's `client` must still authorize
  the nested `escrow.confirm_completion` and `reputation.submit_attestation`
  calls (see "Authorization" below)
- `set_contracts(escrow: Address, reputation: Address, loyalty_token: Address)` — admin-only
- `set_reward_config(reward_config: RewardConfig)` — admin-only
- `is_settled(appointment_id: u64) -> bool`, `get_admin() -> Address`,
  `get_escrow() -> Address`, `get_reputation() -> Address`,
  `get_loyalty_token() -> Address`, `get_reward_config() -> RewardConfig` — read-only views
- `propose_upgrade`, `approve_upgrade`, `cancel_upgrade`, `migrate`, `get_signers`, `get_upgrade_threshold`, `get_pending_upgrade`, `get_storage_version` — see [Upgrade governance](#upgrade-governance)
- `pause`, `unpause`, `get_pause_state`, `paused_scopes`, `is_paused` — see [Emergency circuit breaker](#emergency-circuit-breaker); only `SCOPE_SETTLEMENT` has teeth here (guards `settle`) — this router defines no scope of its own, since every effect it produces flows through a guard the sub-contract already enforces

#### Deploying under partial rollout

Wiring this router in is an ordered rollout, not a single flag flip, because
a paused or unreachable sub-contract fails the whole `settle` call:

1. Deploy this contract via `initialize`, pointing it at the already-deployed
   `escrow`, `reputation` and `loyalty-token` addresses.
2. `loyalty_token.set_minter(router_address)` — `loyalty-emissions`, if
   deployed, loses mint access at this point.
3. `reputation.set_router(router_address)` — the step that actually closes
   the "any `appointment_id`" hole; direct `submit_attestation` calls that
   omit the router's authorization stop working the instant this lands.
4. Point front ends at `settle` instead of calling `escrow.confirm_completion`
   directly — that entrypoint still works standalone (by design; see
   `escrow`'s own docs on why fund-recovery-adjacent paths stay
   permissionless) but bypasses the reputation/loyalty side effects.

#### Authorization

`settle` itself calls no `require_auth` — it is a permissionless relay, the
same pattern `escrow::release_milestone_funds` uses. The authorizations it
depends on (the appointment's `client`, required by the nested
`escrow.confirm_completion` and `reputation.submit_attestation` calls) must
already be present in the submitted transaction. Client-side tooling should
simulate and sign against the `settle` entrypoint specifically, so the
resulting authorization entry's root invocation is `settle`, with
`confirm_completion` and `submit_attestation` as sub-invocations — that is
what binds the signature to the whole atomic settlement.

Reward amounts are fixed by the admin in `RewardConfig` and never taken from
`settle`'s own arguments; letting a caller name their own mint amount would
turn `settle` into an unbounded mint.

#### Idempotency

`DataKey::Settled(appointment_id)` is written before any cross-contract call
is made (checks-effects-interactions, mirroring
`escrow::release_milestone_funds`). A replayed `settle` for the same
`appointment_id` is rejected before touching any other contract. This is
defense in depth, not the only guard — `escrow::confirm_completion` itself
refuses a second call once the appointment is no longer `Funded`.

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Admin` | instance | The admin `Address`; configures contract addresses and `RewardConfig`. |
| `Escrow` / `Reputation` / `LoyaltyToken` | instance | The three contracts this router orchestrates. |
| `RewardConfig` | instance | `RewardConfig { client_reward, worker_reward }`, the fixed loyalty mint amounts per settlement. |
| `Settled(u64)` | persistent | A `bool` flag per `appointment_id`, written before any cross-contract call. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `AlreadyInitialized` | 1 | `initialize` called more than once. |
| `NotInitialized` | 2 | A method needing state was called before `initialize`. |
| `InvalidRewardConfig` | 3 | A negative `client_reward`/`worker_reward` passed to `initialize`/`set_reward_config`. |
| `AlreadySettled` | 4 | `settle` called again for an `appointment_id` already settled. |
| `AppointmentNotFunded` | 5 | The appointment `escrow` reports is not currently `Funded` (missing, already completed, cancelled, or disputed). |
| `GovernanceAlreadyInitialized` | 6 | `initialize` called more than once (surfaced via the governance guard). |
| `GovernanceNotInitialized` | 7 | A governance call before `initialize`. |
| `InvalidThreshold` | 8 | `threshold` is `0` or exceeds the number of signers. |
| `DuplicateSigner` | 9 | The same address appears twice in `signers`. |
| `NotASigner` | 10 | `propose_upgrade`/`approve_upgrade`/`cancel_upgrade`/`migrate` called by a non-signer. |
| `NoPendingUpgrade` | 11 | `approve_upgrade`/`cancel_upgrade` with nothing proposed. |
| `AlreadyApproved` | 12 | The same signer approving the same proposal twice. |
| `ProposalExpired` | 13 | `approve_upgrade` more than ~7 days after `propose_upgrade`. |
| `HashMismatch` | 14 | `approve_upgrade` with a hash that doesn't match the pending proposal. |
| `AlreadyMigrated` | 15 | `migrate` targeting a version already applied or behind the current one. |
| `NothingToMigrate` | 16 | `migrate` called when the stored version is already current. |

Codes 17-22 (signer rotation) are documented in `src/lib.rs`; codes 23-27 are
the circuit breaker's, listed in
[Emergency circuit breaker](#emergency-circuit-breaker).

`settle` itself never returns any `escrow`/`reputation`/`loyalty-token` error
value — a sub-contract's `Err`, or a pause on its side, always panics the
whole transaction (see the crate's own docs for why: any non-`try_`
cross-contract call does this by construction, which is exactly what makes
the settlement atomic).

#### CLI usage

```sh
# One-time setup (see "Deploying under partial rollout" above for the full
# wiring order, including set_minter / set_router on the sub-contracts).
stellar contract invoke --id $ROUTER --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --escrow $ESCROW --reputation $REPUTATION \
     --loyalty_token $LOYALTY --reward_config '{ "client_reward": "50", "worker_reward": "100" }' \
     --governance_init '{"signers":["'$SIGNER_1'","'$SIGNER_2'","'$SIGNER_3'"],"threshold":2}'

# Atomically release funds, record the attestation, and mint loyalty.
stellar contract invoke --id $ROUTER --source client --network testnet \
  -- settle --appointment_id 1 --rating 5 \
     --attestation_hash 0000000000000000000000000000000000000000000000000000000000000000

# Read-only views.
stellar contract invoke --id $ROUTER --source admin --network testnet \
  -- is_settled --appointment_id 1
stellar contract invoke --id $ROUTER --source admin --network testnet \
  -- get_reward_config
```

### dispute-resolution

> ⚠️ **v1, unaudited, no appeals** — single-round staked-jury voting with no sybil-resistant/weighted jury selection. Read [Security considerations / known limitations](#security-considerations--known-limitations) before integrating.

Decentralized dispute resolution: a staked jury decides the outcome via
commit-reveal voting, and the majority is paid out of the slashed stakes of the
minority and no-shows. This is the trust-minimized counterpart to `escrow`'s
admin-only `resolve_dispute` — an integration could have `escrow` read a
resolved dispute's `Outcome` instead of trusting a single arbiter.

- `initialize(admin: Address, token: Address, config: Config)` — `config` is
  `{ juror_stake: i128, min_jurors: u32, commit_window: u32, reveal_window: u32 }`
- `open_dispute(dispute_id: u64, plaintiff: Address, defendant: Address)` —
  admin-only; starts the commit phase (`commit_deadline = now + commit_window`,
  `reveal_deadline = commit_deadline + reveal_window`)
- `commit_vote(dispute_id: u64, juror: Address, commitment: BytesN<32>)` —
  juror-authorized; stakes `juror_stake` and records a hidden vote. Accepted
  only while `now <= commit_deadline`
- `reveal_vote(dispute_id: u64, juror: Address, vote: bool, salt: BytesN<32>)` —
  juror-authorized; discloses the vote (`true` = plaintiff, `false` = defendant)
  during the reveal phase. `vote`+`salt` must hash to the committed value
- `resolve(dispute_id: u64) -> Outcome` — permissionless; after
  `reveal_deadline`, tallies revealed votes and fixes the per-winner reward
- `withdraw(dispute_id: u64, juror: Address) -> i128` — juror-authorized;
  after resolution, pays a winning juror `juror_stake + reward_per_winner`,
  refunds every staker on a tie/quorum-failure, and slashes losers/no-shows
- `compute_commitment(juror: Address, vote: bool, salt: BytesN<32>) -> BytesN<32>` —
  helper so callers build the commitment with the exact domain separation the
  contract enforces
- `get_dispute(dispute_id) -> Dispute`, `get_juror(dispute_id, juror) -> Juror`,
  `get_config() -> Config`, `get_admin() -> Address`, `get_token() -> Address`
  — read-only views

#### Commit-reveal & incentive model

A juror votes in two steps so no one can copy the current leader or be coerced
for a visible vote:

1. **Commit**: submit `commitment = sha256(salt || vote_byte || juror_xdr)` and
   stake `juror_stake`. Binding the juror's own address into the preimage means
   a copycat who replays someone else's commitment can never produce a matching
   reveal from their own address.
2. **Reveal**: disclose `(vote, salt)`; the contract recomputes the hash and, on
   a match, records the vote and bumps the tally.

At `resolve` the side with more revealed votes wins. The **slashed pot** — the
stakes of the minority voters *and* of everyone who committed but never revealed
— is split evenly among the winners (integer division; any remainder dust stays
in the contract). On a **tie** or a turnout below `min_jurors` (`QuorumFailed`)
nobody is slashed and every staker reclaims their stake. Withdrawals use a pull
pattern, so resolution never loops over an unbounded juror set, and a juror is
marked settled before any transfer (checks-effects-interactions).

#### Storage layout

| `DataKey` variant | Storage | Holds |
|---|---|---|
| `Admin` | instance | The `Address` allowed to `open_dispute`, set once in `initialize`. |
| `Token` | instance | The staking-token contract `Address` jurors post collateral in and are paid from. |
| `Config` | instance | `Config { juror_stake, min_jurors, commit_window, reveal_window }`, fixed at `initialize`. |
| `Dispute(u64)` | persistent | A `Dispute { plaintiff, defendant, commit_deadline, reveal_deadline, juror_count, yes_count, no_count, outcome, reward_per_winner, resolved }` per `dispute_id`. |
| `Juror(u64, Address)` | persistent | A `Juror { commitment, revealed, vote, withdrawn }` per `(dispute_id, juror)`. |

#### Errors

| Variant | Code | Meaning |
|---|---|---|
| `AlreadyInitialized` | 1 | `initialize` called more than once. |
| `NotInitialized` | 2 | A method needing state was called before `initialize`. |
| `InvalidConfig` | 3 | A non-positive stake, or a zero quorum/commit/reveal window, passed to `initialize`. |
| `DisputeExists` | 4 | `open_dispute` reused an existing `dispute_id`. |
| `DisputeNotFound` | 5 | No dispute stored under that `dispute_id`. |
| `NotCommitPhase` | 6 | `commit_vote` after the commit deadline. |
| `NotRevealPhase` | 7 | `reveal_vote` outside the reveal window. |
| `AlreadyCommitted` | 8 | `commit_vote` twice for the same `(dispute, juror)`. |
| `NotCommitted` | 9 | `reveal_vote`/`withdraw`/`get_juror` for a juror who never committed. |
| `AlreadyRevealed` | 10 | `reveal_vote` twice. |
| `InvalidReveal` | 11 | Revealed `(vote, salt)` doesn't hash to the stored commitment. |
| `NotResolvable` | 12 | `resolve` called on or before the reveal deadline. |
| `AlreadyResolved` | 13 | `resolve` called on an already-resolved dispute. |
| `NotResolved` | 14 | `withdraw` before the dispute is resolved. |
| `AlreadyWithdrawn` | 15 | `withdraw` called twice by the same juror. |
| `NothingToWithdraw` | 16 | `withdraw` by a slashed juror (minority voter or no-show) on a decided dispute. |
| `SameParties` | 17 | `open_dispute` with `plaintiff == defendant`. |

#### CLI usage

```sh
# One-time setup: stake 100 units, quorum of 3, ~1h commit + ~1h reveal windows.
stellar contract invoke --id $DISPUTES --source admin --network testnet \
  -- initialize --admin $ADMIN_ADDR --token $TOKEN_ADDR \
     --config '{ "juror_stake": "100", "min_jurors": 3, "commit_window": 720, "reveal_window": 720 }'

# Admin opens a dispute between a client (plaintiff) and worker (defendant).
stellar contract invoke --id $DISPUTES --source admin --network testnet \
  -- open_dispute --dispute_id 1 --plaintiff $CLIENT_ADDR --defendant $WORKER_ADDR

# A juror builds their commitment off-chain (favoring the plaintiff), then stakes + commits.
stellar contract invoke --id $DISPUTES --source juror --network testnet \
  -- compute_commitment --juror $JUROR_ADDR --vote true --salt $SALT_32B_HEX
stellar contract invoke --id $DISPUTES --source juror --network testnet \
  -- commit_vote --dispute_id 1 --juror $JUROR_ADDR --commitment $COMMITMENT_HEX

# During the reveal window, the juror discloses their vote and salt.
stellar contract invoke --id $DISPUTES --source juror --network testnet \
  -- reveal_vote --dispute_id 1 --juror $JUROR_ADDR --vote true --salt $SALT_32B_HEX

# After the reveal window, anyone tallies the result.
stellar contract invoke --id $DISPUTES --source anyone --network testnet \
  -- resolve --dispute_id 1

# Each juror settles: winners collect stake + reward, losers are slashed.
stellar contract invoke --id $DISPUTES --source juror --network testnet \
  -- withdraw --dispute_id 1 --juror $JUROR_ADDR
```

## Security considerations / known limitations

- **Unaudited.** None of these contracts have had an independent security
  review. Do not move real funds through `escrow` or mint real value through
  `loyalty-token` on mainnet before getting one.
- **Trusted token contract.** `escrow` assumes the `token` address passed to
  `create_appointment` behaves like a well-formed SEP-41/Stellar Asset
  Contract. A malicious or buggy token contract (e.g. one that lies about
  transfer success) could break escrow's invariants.
- **No partial completion or partial refunds.** `escrow` is all-or-nothing:
  the full amount goes to either the client or the worker. There's no
  mechanism for a partially-completed job.
- **Single point of trust for admin.** The `escrow` admin unilaterally
  decides disputes, the `loyalty-token` admin unilaterally controls who can
  mint, and the `loyalty-emissions` admin unilaterally registers and reclaims
  vesting schedules. All are single-key roles with no timelock or on-chain
  governance for their day-to-day actions — compromising that key
  compromises the contract's normal operation. Note `loyalty-emissions`
  deliberately blocks reclaiming still-vesting funds
  (`reclaimable_after >= start + duration`), so even a compromised admin cannot
  claw back what has already vested to a beneficiary before they claim it.
  Upgrading a contract's code specifically *is* multi-sig gated (see
  [Upgrade governance](#upgrade-governance)) — a compromised admin key alone
  cannot ship new code, only a signer set reaching threshold can — but that
  guard was scoped narrowly to just the upgrade path, not extended to
  `admin`'s existing day-to-day powers.
- **No spam/rate limiting beyond one-review-per-appointment.** `reputation`
  only prevents double-reviewing the same `appointment_id`; it does not
  prevent a client and worker from colluding to create fake appointments
  (that responsibility sits with whatever system calls `create_appointment`
  and `submit_review` with real appointment IDs — today, nothing does, since
  the Java backend isn't integrated yet).
- **A pause is a liveness risk, not a custody risk — by construction.** All
  four contracts can now be halted per-scope (see
  [Emergency circuit breaker](#emergency-circuit-breaker)), and any *single*
  governance signer can do it. Be clear about what that buys an attacker who
  compromises one key: they can block new bookings, payouts and attestations
  for up to 7 days per call, and can re-arm each time a window lapses. They
  cannot touch a balance that already exists — refunds, disputes and token
  transfers of held balances carry no pause check at all — and they cannot
  make a halt outlive their own presence, because expiry is evaluated on
  every read. Sizing `MAX_PAUSE_DURATION` is the knob that trades response
  headroom against that griefing window.
- **The upgrade path's actual Wasm swap is untested by `cargo test`.**
  `propose_upgrade`/`approve_upgrade` crossing the configured threshold
  calls `update_current_contract_wasm`, which requires the target hash to
  already be uploaded on the ledger — there's no such artifact inside a
  plain unit test run. Every test that exercises the governance flow through
  a real contract stops one approval short of the threshold on purpose; the
  underlying threshold/replay/expiry math is covered directly in
  `contracts/governance-guard`'s own test suite, and the SDK call itself was
  verified against its documented behavior rather than exercised live. A
  testnet deploy-and-upgrade dry run is the natural next verification step
  before this ships anywhere real funds move through.
- **`settlement-router` fails shut, not open.** `settle` makes plain
  (non-`try_`) cross-contract calls, so a paused `reputation`/`loyalty-token`,
  or one simply not yet wired to trust this router
  (`reputation.set_router`/`loyalty_token.set_minter`), aborts the whole
  settlement rather than degrading to "release funds anyway." That is the
  intended atomicity trade-off, but it does mean a completed appointment's
  payout is gated on infrastructure — two other contracts' liveness and
  correct configuration — that `escrow.confirm_completion` alone never
  depended on. Operators should treat the "Deploying under partial rollout"
  sequencing in [settlement-router](#settlement-router) as load-bearing, not
  optional ordering advice.
- **Router reward amounts are fixed, not proportional.** `RewardConfig`'s
  `client_reward`/`worker_reward` are flat amounts set once by the router's
  admin, unrelated to the appointment's escrowed `amount`. A protocol that
  wants loyalty proportional to spend needs that logic added explicitly —
  it is not inferred from escrow state today.
- **Comments are not authenticated content.** `reputation`'s `comment` field
  is an arbitrary `String` supplied by the reviewer with no length cap or
  content moderation — treat it as untrusted user input wherever it's
  displayed.
- **Jury sybil / stake-weighting.** `dispute-resolution` gives every juror who
  posts `juror_stake` exactly one vote and lets anyone join a dispute during the
  commit phase. It resists *free* sybils (each identity must lock real
  collateral) and hidden-vote manipulation (commit-reveal), but it does **not**
  defend against a well-capitalized actor funding many jurors to swing a verdict
  — there is no random jury selection, reputation weighting, or per-identity
  gating. Set `juror_stake`/`min_jurors` relative to the value at stake, and
  treat this as a coordination mechanism among semi-trusted jurors, not a
  Kleros-grade court. Reputation-weighted / randomized jury selection (building
  on the attestation-based reputation scoring in #20) is a deliberate v1 scope
  boundary tracked in #29, not an oversight.
- **No appeals and majority-takes-all slashing.** A dispute resolves in a single
  round; there is no appeal path, and honest jurors who happen to land in the
  minority are slashed alongside malicious ones. A dishonest majority both wins
  the verdict and confiscates the honest minority's stake. Ties and below-quorum
  turnouts are handled safely (everyone is refunded, no slashing), and integer
  division of the slashed pot can leave at most `winner_count - 1` units of dust
  in the contract. Slashing the whole minority is an intentional Schelling-point
  incentive (commit-reveal is what makes it defensible), but softening it for
  close calls — margin-based partial refunds or an appeal round — is tracked as a
  candidate v2 direction in #29.
- **Reveal-phase liveness assumption.** A juror who commits but never reveals is
  treated as a loser and slashed (on a decided outcome), which is the intended
  anti-griefing incentive — but it also means a juror censored or offline during
  the reveal window loses their stake. Size `reveal_window` accordingly.

## Notes / follow-ups

- `escrow` expects a standard Soroban token contract address (e.g. a Stellar
  Asset Contract wrapping USDC or XLM) for the `token` parameter — it does not
  handle native fiat payments, which stay on Paystack in the existing backend.
- These contracts have not been audited. Get an independent security review
  before moving any real funds through `escrow` or `loyalty-token` on mainnet.
- Governance signer rotation. Right now the signer set and threshold are
  fixed forever at `initialize` — see
  [Upgrade governance](#upgrade-governance) for why that's a deliberate
  scope boundary rather than an oversight, and a real candidate for a
  follow-up issue.
- Real dispute resolution for `escrow` beyond a single admin call, and a
  genuine storage migration exercising `migrate`'s version-transform path
  (nothing has needed one yet — every contract is still on storage version 1).
- `settlement-router` is deployed and tested but not called from
  `backend-api/` yet, same as everything else in
  [Suggested backend integration](#suggested-backend-integration-not-yet-wired-in) —
  wiring it in requires re-pointing `loyalty-token`'s `minter` and
  `reputation`'s router away from whatever (if anything) currently holds
  those roles, per its own "Deploying under partial rollout" notes.

## License

MIT — see [LICENSE](./LICENSE).
