# EVM-NOTES: giving Breeja an EVM

Route taken: **REVM** (`revm = "43"`, resolved to 43.0.3 — the version
FINAL-PROJECT.md was verified against).

## Milestone 0: baseline

Branch `feat/evm` was cut from upstream `main` at `bbbdf34`.

`cargo test --workspace` before any change: **48 passed, 0 failed**.

| Suite | Tests |
| --- | --- |
| `node` unit tests (`executor.rs`, `pow.rs`) | 13 |
| `node` integration (`tests/mining.rs`) | 8 |
| `primitives` unit tests | 14 |
| `rpc` integration (`tests/json_rpc.rs`) | 13 |

`cargo clippy --all-targets`: no warnings.

`cargo run --release -- --dev` (first blocks, trimmed):

```text
INFO genesis block sealed genesis_hash=0xcd766ee4…1434 accounts=4
INFO miner started miner=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 difficulty=3 target_block_time_secs=2
INFO sealed block number=1 hash=0x0f16b220…7963 txs=0 difficulty=3 nonce=4 attempts=5 took=0.00s
INFO retargeting difficulty from=3 to=4
INFO sealed block number=2 hash=0x07645d40…9dca txs=0 difficulty=4 nonce=5 attempts=6 took=0.00s
INFO JSON-RPC server listening address=127.0.0.1:18545 methods=11
INFO sealed block number=3 hash=0x04bfea6d…69ed9 txs=0 difficulty=5 nonce=41 attempts=42 took=0.00s
INFO sealed block number=4 hash=0x00ca753e…91da5 txs=0 difficulty=6 nonce=40 attempts=41 took=0.00s
INFO sealed block number=5 hash=0x01e68bbc…b34a txs=0 difficulty=7 nonce=147 attempts=148 took=0.00s
INFO sealed block number=6 hash=0x00cb5bfc…6555 txs=0 difficulty=8 nonce=260 attempts=261 took=0.00s
```

Each block is far faster than the 2 s target, so the miner adds one difficulty
bit per block until it gets into the target band.

`revm = "43"` was added with `cargo add revm@43 --package node` and the
workspace still builds.

## Part 0: answers before writing code

### 1. Which pool checks exist only because there is no EVM?

In `pool.rs::decode_and_check`:

- **`TxKind::Create` is rejected** ("contract creation is not supported").
  → **Removed.** A transaction with no `to` is a deployment, and REVM handles it.
- **Non-empty calldata is rejected** ("transaction carries N bytes of calldata").
  → **Removed.** Calldata is how a contract is called.
- **`gas < TRANSFER_GAS`** is the check that does not mention the EVM but
  assumes every transaction is a plain transfer. → **Relaxed, not removed.** A
  transaction still needs a gas limit that covers its *intrinsic* cost, but that
  cost depends on the transaction: 21,000 base, plus 4 gas per zero byte and 16
  per non-zero byte of calldata, plus 32,000 for a contract creation (and, under
  Shanghai, EIP-3860's 2 gas per 32-byte word of initcode). A flat 21,000 floor
  would admit a deployment that REVM is certain to reject.

### 2. Should a failing transaction still be skipped?

Today, if `apply_transfer` returns `Err` the transaction is dropped from the
block. The comment above `execute_block` says that is fine because every failure
left is a race, such as a nonce another transaction already used.

With an EVM that stops being true. A transaction can be completely valid
(right nonce, enough balance, enough gas) and still `REVERT` partway through
because the contract chose to reject it. Real Ethereum **includes** that
transaction: the sender pays for the gas that was burned, the nonce advances,
and the receipt has `status: 0x0`. If it were skipped, the sender would get a
free retry, and a contract could force a miner to spend CPU on work that is
never paid for.

So two kinds of failure need different handling:

- **Invalid** (REVM returns `Err(EVMError::Transaction(..))`): bad nonce, cannot
  afford `gas_limit × gas_price + value`, gas limit below the intrinsic cost.
  The transaction cannot appear in a valid block. **Skip it.**
- **Executed but failed** (`ExecutionResult::Revert` or `Halt`): **include it**
  with `status = 0`.

### 3. Why reserve `gas_limit × gas_price` but charge only `TRANSFER_GAS`?

Ethereum charges the sender `gas_limit × gas_price` before execution and refunds
`(gas_limit − gas_used) × gas_price` afterwards. The sender must be able to
afford the worst case before anything runs, because nobody knows in advance how
much gas will be used. Breeja copied the up-front check so the accounting matches
Ethereum, but in Breeja every transfer costs exactly 21,000, so the difference was
never really at risk.

Once gas is variable, that reservation is required for correctness. REVM itself
rejects a transaction whose sender cannot cover `gas_limit × gas_price + value`
(`LackOfFundForMaxFee`), so the mempool has to apply the same rule or it will
admit transactions the miner then drops. The unused part is refunded by REVM, not
charged. `get_pending_balance` already subtracts `gas × gas_price` for each queued
transaction, which is the correct (conservative) amount.

### 4. What does the per-block snapshot cost once accounts have storage?

`commit_block` pushes `self.accounts.clone()` into `state_history` after every
block. With only a nonce and a balance per account, that is small. Once accounts
have storage, each clone copies every storage slot of every contract, so the
memory and time per block grow with the total size of storage. Over a long chain
that comes to roughly *blocks × storage*, even when one block changes one slot.

If code were stored inline in `Account`, every snapshot would copy every contract's
bytecode as well. This is the main reason to store code once, in a map keyed by
code hash, and keep only the hash in the account. A real node stores per-block
*diffs*, or uses a trie whose nodes are shared between versions. For a teaching
chain with a handful of accounts, full snapshots stay affordable, and I keep them,
but the cost is now visible.

## Milestone 1: state model

### Representation

- `Account` keeps its four Ethereum fields (`nonce`, `balance`, `storage_root`,
  `code_hash`) and gains `storage: HashMap<U256, U256>`. The storage map is marked
  `#[rlp(skip)]`, so the RLP encoding stays the four-field Ethereum account.
  Storage is committed to through `storage_root`, never inlined.
- Code is **not** stored in `Account`. `BlockState` has a `code: HashMap<B256, Bytes>`
  keyed by code hash, and an account holds only the hash. I picked this over storing
  `Bytecode` inline for three reasons:
  1. Identical contracts share one copy, which is what REVM's `code_by_hash` assumes.
  2. Code is immutable, so the map only grows and needs no per-block history. An
     account's `code_hash` at any historical block is enough to find its code.
  3. The per-block `state_history` snapshots do not copy bytecode (Part 0, Q4).
- `primitives` stores code as `alloy` `Bytes`, not REVM's `Bytecode`, so the
  primitives crate does not depend on REVM. `BreejaDb` converts to `Bytecode` lazily,
  only for the contracts a transaction actually touches.
- "No code" stays `code_hash == B256::ZERO` in Breeja, which is what every existing
  account already has. REVM uses `keccak256("")` (`KECCAK_EMPTY`). The bridge
  translates both ways, so the state never holds two spellings of "no code", which
  would give two equal states different roots.

### `BlockState` accessors

`get_code(address)`, `code_by_hash(hash)`, `get_storage_at(address, slot)`,
`code_snapshot()`, and `recent_block_hashes(n)`.

### `Database` / `DatabaseCommit` (`crates/node/src/evm.rs`)

Implemented on `BreejaDb`, a snapshot built by `BreejaDb::snapshot(&BlockState)`,
not on `BlockState`. REVM holds `&mut` on its database for the whole execution, and
`BlockState` is behind the `RwLock` the RPC server reads through.

Decisions that differ from, or go beyond, the Appendix A reference:

- **`basic` returns `None` for an unknown address**, not an empty account. `None`
  tells REVM the account does not exist. Ethereum's gas rules depend on this: a
  `CALL` that sends value to a non-existent account pays 25,000 extra gas
  (`G_newaccount`). Returning an empty account would make every address look like it
  already existed and under-charge that call.
- **`basic` returns the code alongside the account info**, so REVM does not need a
  second `code_by_hash` round trip.
- **`commit` stores `code.original_bytes()`, not `code.bytes()`.** REVM pads analysed
  legacy bytecode with trailing zero bytes (`STOP`). The padding is not part of the
  contract, and `eth_getCode` must not return it.
- **A storage slot written to zero is removed**, not stored as zero. Zero and absent
  read identically through `SLOAD`, so keeping both would let two equal states hash
  to different storage roots (relevant to Milestone 6).
- **A newly created account has its storage cleared** before its writes are applied
  (`is_created()`), matching REVM's own `CacheDB`.
- **EIP-161 state clearing.** An account that a transaction touched and left empty
  (no nonce, no balance, no code, no storage) is deleted. Shanghai includes
  EIP-161, and without it a zero-value transfer to a fresh address would add an
  empty account to the state and change the state root for nothing.
- `block_hash` serves the last 256 block hashes from the snapshot. I checked that
  REVM 43 enforces the 256-block window itself (`interpreter/instructions/host.rs`),
  so the database only needs to hold what is inside it.
- `type Error = Infallible`, because an in-memory map cannot fail. Every non-zero
  `code_hash` is inserted by `commit` together with its bytes, so a lookup miss in
  `code_by_hash` cannot happen. If it ever did, the answer is empty code.

### Tests added (`evm.rs`)

| Test | Checks |
| --- | --- |
| `basic_returns_the_balance_and_nonce_of_a_funded_account` | Required test 1 |
| `basic_returns_none_for_an_address_that_was_never_funded` | Required test 2 (deliberately `None`) |
| `commit_persists_balance_nonce_code_and_storage` | Required test 3, plus a read-back round trip |
| `commit_ignores_accounts_revm_only_read` | `is_touched()` filter |
| `writing_a_slot_back_to_zero_removes_it` | Zero-slot removal |

`cargo test --workspace`: **53 passed** (48 baseline + 5). No behaviour change yet.
