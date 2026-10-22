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

## Milestone 2: transfers through REVM

`apply_transfer` is gone. `executor.rs` now has `execute_transaction(db, block, tx)`,
which runs one transaction through REVM and commits its changes to the `BreejaDb`
snapshot. `execute_block` calls it for every transaction in the batch.

### Mapping

- `PendingTransaction` → `TxEnv`: `tx_type: 0` (legacy), `caller`, `gas_limit`,
  `gas_price`, `kind: TxKind::Call(to)`, `value`, `nonce`, `chain_id`. Calldata is
  still empty at this milestone.
- Block → `BlockEnv` through a new `BlockContext` struct: `number`, `beneficiary`
  (the miner), `timestamp`, `gas_limit`, `basefee: 0`, `difficulty`, and
  `prevrandao: Some(parent_hash)`.
- `CfgEnv::new_with_spec(SpecId::SHANGHAI).with_chain_id(CHAIN_ID)`.
- `ExecutionResult` → `TxOutcome { success, gas_used, created, logs, output }`, which
  becomes the `Transaction` and `TransactionReceipt`. `gas_used` comes from
  `tx_gas_used()`, never the deprecated `gas_used()` (Pitfall 4).

### What had to move

**The block timestamp is now fixed before execution.** Previously the miner
executed the batch first and only then picked a timestamp for the header. With an
EVM that is wrong: the `TIMESTAMP` opcode reads `BlockEnv.timestamp`, so the value
the EVM sees must be the value that ends up in the sealed header. The miner now
builds a `BlockContext` (number, timestamp, parent hash, difficulty, gas limit,
miner) first, executes against it, and copies the same timestamp into the header.
This is the same reasoning as Pitfall 7: anything the EVM reads about "this block"
has to be known before proof of work starts.

**`commit_block` takes the code map.** The miner snapshots
`BreejaDb::snapshot(&state)` (accounts, code, last 256 block hashes) under the read
lock, releases it, executes, and hands back both the accounts and the code. The code
map is append-only, so `commit_block` extends it rather than replacing it.

### Pitfalls avoided

1. **REVM already pays the miner.** The manual `credit(fee)` to the miner is gone.
   REVM pays `BlockEnv.beneficiary` itself. The block reward is still paid by
   `pay_block_reward`, because REVM knows nothing about block rewards. They are
   Breeja's policy.
2. **`basefee = 0`**, so nothing is burned and Breeja stays pre-EIP-1559.
3. **REVM increments the nonce.** The manual `increase_nonce()` is gone.
6. **Borrow and lock.** `transact` builds the EVM over `&mut *db`, takes
   `outcome.result` and `outcome.state` out by value, and the EVM is dropped when the
   function returns, so `db.commit(state)` happens after the mutable borrow has
   ended. No lock is held: the miner executes on its own snapshot.
7. **`prevrandao = Some(parent.hash)`.** The current block's hash does not exist yet
   when the EVM runs.

### Invalid versus failed

`transact` returning `Err` means REVM refused to execute the transaction at all (bad
nonce, cannot afford `gas_limit × gas_price + value`). Those are still dropped, which
is why `a_transaction_that_no_longer_applies_is_skipped_not_mined` still passes
unchanged. `Ok` with a `Revert` or `Halt` result is recorded with `status = 0`; the
tests for that come in Milestone 3.

### Tests

- New: `a_transfer_through_revm_costs_exactly_21000_gas_and_pays_the_beneficiary_once`.
  It checks `tx_gas_used() == 21000` and that the beneficiary received exactly
  `21_000_000_000_000` wei, the figure from Pitfall 1. If the manual fee credit were
  still there, it would be 42,000 gwei.
- The seven existing `executor.rs` tests changed **signature only**: they build a
  `BreejaDb` and a `BlockContext` instead of a bare account map. Every assertion is
  unchanged, which is what parity means.
- `the_chain_never_creates_or_destroys_eth_outside_the_block_reward` passes without
  modification.

`cargo test --workspace`: **54 passed**. `cargo clippy --all-targets`: clean.

### Manual check

`cargo run --release -- --dev`, then the README's `cast mktx` transfer of 1 ETH from
`0xf39F…2266` to `0x7099…79C8`. It was mined in block 17 with `gasUsed: 0x5208` and
`status: 0x1`, and the recipient's balance went from 10,000 to 10,001 ETH.

## Milestone 3: contracts

### Mempool rules (Part 0, Q1 carried out)

- The `TxKind::Create` rejection and the non-empty-calldata rejection are **removed**.
- The flat `gas < TRANSFER_GAS` floor is **replaced** by `pool::intrinsic_gas(input,
  is_create)`:
  `21,000 + 4 × zero bytes + 16 × non-zero bytes`, plus, for a creation,
  `32,000 + 2 × ceil(len / 32)`. The last term is EIP-3860's initcode word cost,
  which is part of Shanghai. A plain transfer still comes out at exactly 21,000.
  The unit test `intrinsic_gas_matches_revm_under_shanghai` compares my function
  against REVM's own `calculate_initial_tx_gas(SpecId::SHANGHAI, ..)` for five
  inputs, including the test contract's initcode and a 33-byte initcode that crosses
  a word boundary. If the two ever disagree, the pool would admit transactions the
  miner then drops, or reject ones the EVM would run.
- The reservation check (`value + gas × gas_price` must be available) is unchanged.
  Part 0 Q3 explains why it is now required rather than just matching Ethereum.

### Data model

- `PendingTransaction`, `Transaction`, and `TransactionReceipt` now have
  `to: Option<Address>`. `None` is a deployment. Over JSON-RPC it serializes as
  `"to": null`, as on Ethereum, not as the zero address. The zero address is a real
  address, and sending to it is a transfer, not a deployment.
- `PendingTransaction` gained `input`. `Transaction.input` now stores the real
  calldata instead of `Default::default()`.
- New `primitives::node::Log { address, topics, data, log_index }`. `log_index` is the
  position within the **block**, not within the transaction, because that is what
  `logIndex` means in Ethereum's JSON-RPC.
- `TransactionReceipt` gained `contract_address` (from
  `ExecutionResult::created_address()`) and `logs` (from `ExecutionResult::logs()`).
  `status` comes from `is_success()`: `Success` → 1, `Revert` and `Halt` → 0.
- `LogResponse` renders a log with its block number, block hash, transaction hash,
  transaction index, log index, and `removed: false`. Breeja has no reorgs, so
  `removed` is always false. The same type will serve `eth_getLogs` in Milestone 5.

### Failed transactions are included

This is the behaviour change from Part 0 Q2. `execute_block` drops a transaction only
when REVM refuses it outright (`transact` returns `Err`: bad nonce, or cannot afford
the up-front reservation). A transaction that executes and reverts or halts is
mined, charged, and its nonce advances. The upstream doc comment on `execute_block`
described the old skip-everything behaviour, so I removed it rather than leave it
wrong.

### Test contracts (`testing.rs`)

- `STORE_AND_LOG_INITCODE` / `STORE_AND_LOG_RUNTIME`: the contract from the spec
  (`storage[0] = 42`, then an empty `LOG0`).
- `STORE_THEN_REVERT_INITCODE` / `STORE_THEN_REVERT_RUNTIME`, which I hand-assembled
  the same way:

  ```text
  initcode: 600a 600c 6000 39 600a 6000 f3   copy 10 bytes from offset 12, return them
  runtime:  602a 6000 55                     storage[0] = 42
            6000 6000 fd                     REVERT with no data
  ```

  It writes storage **before** reverting, so the test also proves the revert rolls
  the write back, not just that the status is 0.
- `sign_deployment`, `sign_call`, and the general `sign_legacy` they share with
  `sign_transfer_with`.

### Tests

Integration (`crates/node/tests/contracts.rs`), through the real mempool and miner:

| Test | Asserts |
| --- | --- |
| `deploying_a_contract_sets_contract_address_and_commits_its_code` | `status 1`, `to == None`, `contract_address == sender.create(0)`, gas in 55,000–56,000, `get_code` returns exactly the 11 runtime bytes, the contract's nonce is 1 (EIP-161), and the stored transaction keeps `to: None` and the initcode |
| `calling_a_contract_changes_storage_and_emits_a_log` | Slot 0 goes from 0 to 42, one log whose address is the contract with no topics or data, `contract_address == None`, gas in 43,000–44,000 |
| `a_reverting_transaction_is_mined_with_status_zero_and_still_charged` | `status 0`, the transaction is in the block, nonce is 2, Alice's balance is exactly `start − (deploy_gas + call_gas) × 1 gwei`, no logs, slot 0 still 0 |

Unit (`executor.rs`): `a_deployment_reports_its_address_and_a_call_runs_the_code`,
and `running_out_of_gas_halts_and_consumes_the_whole_limit`. The second calls the
store contract with a 22,000 gas limit: it halts, uses all 22,000, still bumps the
nonce, and leaves storage empty. This is the `Halt` variant, as opposed to `Revert`.

One existing test changed: `mining.rs` now compares `tx.to` with `Some(bob)` instead
of `bob`. That is a type change only. The expectation (Bob is the recipient) is the
same, so it has no justification comment.

`cargo test --workspace`: **60 passed**. `cargo clippy --all-targets`: clean.

### Manual check against a live `--dev` node

Deployed the spec's initcode with `cast mktx --legacy --create …`, then called the
contract with `cast mktx` to its address:

| | Gas used | Spec's reference |
| --- | --- | --- |
| Deployment | **55,522** | ~55,522 |
| Call | **43,487** | ~43,487 |

The deploy receipt had `contractAddress: 0x5fbdb2315678afecb367f032d93f642f64180aa3`
(the familiar first-deployment address for this dev key) and `to: null`. The call
receipt had one log at that address with `topics: []`, `data: 0x`, and `logIndex: 0x0`.

### Known gaps at this point

- `logsBloom` is still 256 zero bytes, even for a receipt with logs (Milestone 6).
- A transaction whose gas limit is above `BLOCK_GAS_LIMIT` is still admitted to the
  mempool, but REVM will refuse it at execution, so it would sit in the pool forever
  (fixed in Milestone 6).
- EIP-3860's 49,152-byte initcode size limit is enforced by REVM at execution but not
  by the pool, so an oversized deployment is admitted and then dropped.
