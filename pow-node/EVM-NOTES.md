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

## Milestone 4: `eth_call` and `eth_estimateGas`

### One execution path, one difference

Both methods use the same `transact` function as the miner. The only difference from
a real transaction is that `execute_transaction` calls `db.commit(state)` and the
simulation path (`run_without_commit`) throws `state` away. REVM's `Database` methods
only read, so the snapshot itself is never written. The discarded `EvmState` is the
"throwaway copy" the spec talks about, and no extra clone of the state is needed per
probe. The unit test `simulating_a_call_returns_its_output_but_writes_nothing`
compares the whole account map before and after.

The RPC handlers take a `BreejaDb` snapshot under the read lock, release it, and
only then execute (Pitfall 6). A 20-probe gas estimate never blocks other requests.

### Block context and block tags

A simulation runs as if it were the **next** block on top of the requested one
(`BlockContext::on_top_of`): number + 1, a fresh timestamp, `prevrandao` = that
block's hash, and the configured miner as `COINBASE`. That is the environment the
transaction would actually see if it were sent now.

Because `state_history` already keeps a full account snapshot per block, and code is
append-only, a block tag costs nothing extra: `BreejaDb::snapshot_at(state, n)`
builds the database from block `n`. `eth_call` at `"0x0"` against a contract
deployed later correctly returns `0x`, because there is no code at genesis.

### `CfgEnv` choices (the spec asks me to justify these)

| Check | `eth_call` | `eth_estimateGas` | Why |
| --- | --- | --- | --- |
| Nonce (`disable_nonce_check`) | off | off | A simulation is not part of the sender's transaction sequence. Making the caller supply the right nonce adds nothing. |
| Balance (`disable_balance_check`) | **off** | **on** | `eth_call` asks "what does this code return?", which should not depend on the caller being funded, and callers often omit `from`. `eth_estimateGas` asks "would this succeed if I sent it?", where an unaffordable `value` is exactly the answer the caller needs. |
| EIP-3607 (`disable_eip3607`) | **off** | on | Tools commonly simulate a call "as" a contract address. A real transaction can never come from a contract, so the estimate keeps the rule. |

`disable_balance_check` and `disable_eip3607` sit behind REVM's
`optional_balance_check` and `optional_eip3607` cargo features, so those two are now
enabled in `crates/node/Cargo.toml`. Both default to `false`, so block execution is
unaffected.

When `gasPrice` is omitted it defaults to 0, as geth does, so the gas itself costs
nothing in a simulation. When a gas price *is* given, the estimate's upper bound is
capped at what the sender can afford, `(balance − value) / gasPrice`, as geth does.

### Reverts and halts over RPC

A reverted `eth_call` returns JSON-RPC error **code 3**, message
`"execution reverted"`, with the revert data as hex in `data`. That is what `cast` and
other client libraries look for to decode revert reasons. A halt (out of gas, invalid
opcode) returns `-32000` with the reason, for example `execution halted: OutOfGas(Basic)`.
Returning `0x` for a failed call would be silently wrong.

### `eth_estimateGas`: binary search

1. `floor = intrinsic_gas(calldata, is_create)`, the same function the mempool uses.
   `ceiling = min(block gas limit, request.gas, what the sender can afford)`.
2. Execute once at `ceiling`. If that fails, no gas limit can make it succeed, so
   return an error carrying the revert data or halt reason.
3. If `floor` itself succeeds, return it unchanged. This is why a plain transfer
   estimates to exactly `0x5208`.
4. Otherwise, binary search `(floor, ceiling]` for the lowest passing limit. That is
   about 22 probes over the 3.1M range.
5. Add a 10% margin (capped at `ceiling`), but only when the answer is above the
   intrinsic cost, that is, only when code actually ran.

The margin exists because the estimate is taken against state *now*, and the
transaction runs against state *later*. Storage the contract reads may have changed
in between, and with it the gas.

**The search is a heuristic, not a proof.** It assumes "succeeds with g gas" is
monotonic in g. A contract can read its remaining gas with the `GAS` opcode and branch
on it, so it could succeed at 50,000, fail at 60,000, and succeed at 70,000. Binary
search can return a limit inside a failing island. EIP-150's 63/64 rule makes this
real even without malicious code: a contract that makes an inner call forwards less
gas at a lower limit, and the inner call can fail while the outer one survives. The
final step 2 check at the ceiling only proves *some* limit works, not that the
returned one does. Geth has the same weakness. The margin makes it rarer, but does
not remove it.

`the_binary_search_finds_the_lowest_gas_limit_that_succeeds` checks the result is
exact: the store-and-log call succeeds at `minimal` and fails at `minimal − 1`.

### Removed no-EVM code

- `no_evm` (handlers), `CallRequest::needs_evm` (params), and `TRANSFER_GAS_ESTIMATE`
  (response) existed only to refuse requests that needed an EVM. All three are gone.
- The stale doc comments on `call_handler` and `estimate_gas_handler` ("Breeja has no
  EVM…") are removed.
- `unix_now` moved from `miner.rs` to `executor.rs`, because `BlockContext::on_top_of`
  is now shared by the miner and the RPC handlers.

### Tests

New unit tests (`executor.rs`):

| Test | Checks |
| --- | --- |
| `simulating_a_call_returns_its_output_but_writes_nothing` | Output is 32-byte 42, account map identical before and after |
| `a_call_from_an_account_with_no_funds_still_runs` | Balance check disabled for `eth_call` |
| `the_estimate_for_a_plain_transfer_is_exactly_21000` | Parity, with no margin |
| `the_binary_search_finds_the_lowest_gas_limit_that_succeeds` | `minimal` passes, `minimal − 1` fails, estimate = minimal + 10% |
| `estimating_a_transaction_that_always_reverts_is_an_error` | Error carries "execution reverted" |
| `estimating_a_transfer_the_sender_cannot_afford_is_an_error` | Balance check kept for estimates |

New RPC tests (`json_rpc.rs`), on a real server with the miner running:

| Test | Spec item |
| --- | --- |
| `eth_call_returns_contract_output_and_leaves_state_unchanged` | 12: output `0x…2a`, slot 0 still 0 after calling code that writes it, sender nonce unchanged, and `0x` at block `0x0` |
| `eth_call_reports_a_revert_as_an_error` | Revert is an error, not empty output |
| `eth_estimate_gas_is_enough_to_send_the_transaction` | 13: estimate, then send with exactly that gas; mined with status 1, `gas_used ≤ estimate`, slot 0 = 42 |

A third test contract was added to `testing.rs` for this. `STORE_AND_RETURN` writes
42 to slot 0 and returns 42 as a 32-byte word, so a single call proves both
"returns the right bytes" and "the write was discarded":

```text
602a 6000 55          storage[0] = 42
602a 6000 52          memory[0..32] = 42
6020 6000 f3          return memory[0..32]
```

**Existing tests whose expectations changed** (each has a one-line comment in code):

- `eth_call_answers_transfers_and_refuses_anything_needing_the_evm` →
  `eth_call_to_an_account_without_code_returns_no_data`. It used to expect a
  "no EVM" error for calldata. Calling an address with no code runs nothing, so it
  returns `0x` with or without calldata, as on Ethereum.
- `eth_estimate_gas_returns_the_intrinsic_cost_of_a_transfer`: the deployment half
  expected an error. `0x6080604052` (`PUSH1 0x80 PUSH1 0x40 MSTORE`) is valid
  initcode, so the estimate now succeeds and must be at least the creation's intrinsic
  cost.
- `a_call_needs_an_evm_when_it_carries_calldata_or_creates_a_contract` (params) tested
  `needs_evm`, which was removed. It is replaced by
  `calldata_is_read_from_data_or_input`, which tests the helper that survives, so the
  test count does not drop.

`cargo test --workspace`: **69 passed**. `cargo clippy --all-targets`: clean.

### Manual check against a live `--dev` node, with stock Foundry

```text
cast call --from 0xf39F…2266 <contract>     → 0x…002a
cast estimate --from … 0x7099…79C8 --value 1ether → 21000
cast estimate --from … <contract>           → 47436   (minimum 43,124 + 10%)
send with --gas-limit 47436                 → status 0x1, gasUsed 43124
```

## Milestone 5: RPC methods for contracts

Four methods added: `eth_gasPrice`, `eth_getCode`, `eth_getStorageAt`, `eth_getLogs`.
The node now serves fifteen.

### `eth_getCode` and `eth_getStorageAt`

Both take a block tag and answer **historically**, not just at the head. This needs no
new storage:

- Code: the account's `code_hash` at block `n` comes from `state_history[n]`, and the
  code map is append-only, so `BlockState::get_code_at(address, n)` is a lookup in
  each.
- Storage: `state_history[n]` holds each account's full storage as it was after
  block `n`, so `get_storage_at_block(address, slot, n)` reads it directly.

An address with no code returns `0x`. A slot is returned as a full 32-byte word
(`data_word`, `0x` + 64 hex digits), not a minimal quantity, because
`eth_getStorageAt` returns DATA. The slot parameter is accepted either as a quantity
(`"0x0"`) or as a padded 32-byte value, since clients send both.

### `eth_getLogs`

`LogFilter` in `primitives/src/params.rs` accepts:

- `fromBlock` / `toBlock` (any block tag, default `latest`), or `blockHash` (EIP-234).
  Combining `blockHash` with either range field is rejected, and `fromBlock > toBlock`
  is an error rather than an empty result.
- `address`: one address or a list. A list means "any of these", and an empty list
  matches everything.
- `topics`: an array of **positions**. Each position is `null` (anything), one hash,
  or a list of hashes (any of them).

Topic matching is in `LogFilter::matches`. The parts that are genuinely unintuitive,
each covered by `log_filters_follow_the_positional_topic_rules`:

1. **Positions are AND, a list inside a position is OR.** `[A, B]` means topic0 = A
   *and* topic1 = B. `[[A, C]]` means topic0 = A *or* C.
2. **`null` is a positional wildcard, not "absent".** `[null, B]` matches any log whose
   *second* topic is B.
3. **A filter longer than the log never matches, even if the extra positions are
   `null`.** `[A, B, null]` does not match a log with exactly topics `[A, B]`. I
   followed geth here (`if len(filter.topics) > len(log.Topics) { continue }`). The
   spec wording ("null matches anything") could be read either way, and a client
   written against geth expects this.
4. `[]` (or no `topics` field) matches every log, including ones with no topics at all.

The handler walks blocks `from..=to`, then each block's transactions in order, then
each receipt's logs, so results come back in chain order with their block-wide
`logIndex`. **Limitation:** there is no cap on the range and no bloom-based skipping,
so `fromBlock: earliest` scans the whole chain under the read lock. That is fine at
teaching-chain sizes. A real node rejects huge ranges and uses `logsBloom` (Milestone
6) to skip blocks that cannot match.

### `eth_gasPrice`

One line: `quantity(state.gas_price())`, which is the fixed 1 gwei. As the spec says,
this is what makes stock tooling work. See the manual check below.

### A bug found by using real tooling

The first time I ran plain `cast send` against the node, it failed with
`missing field cumulativeGasUsed`. The receipt response serialized that field as
**`cummulativeGasUsed`**, a misspelling that predates this project. Every client
library that parses receipts requires the spec name. The unit tests never caught it,
because they only read fields they asked for by name.

Fix: `#[serde(rename = "cumulativeGasUsed")]` on the response field. The internal Rust
field names are unchanged, to keep the diff small. A regression assertion
(`receipt["cumulativeGasUsed"] == "0x5208"`) was added to the existing transfer
receipt test. That is a new expectation, not a changed one, so it carries no
justification comment.

### Tests

Unit (`params.rs`): `log_filters_follow_the_positional_topic_rules`, 13 assertions over
empty, single, wildcard, OR-list, too-long, and address filters.

RPC (`json_rpc.rs`):

| Test | Spec item |
| --- | --- |
| `eth_get_code_returns_runtime_bytecode_and_0x_without_code` | 10: exactly the 11 runtime bytes; `0x` for an EOA; `0x` for the contract at block `0x0` |
| `eth_get_storage_at_returns_the_slot_as_a_32_byte_word` | 11: slot 0 is `0x` + 64 hex digits ending `2a`; the block before the call returns the zero word |
| `eth_get_logs_filters_by_block_range_and_address` | 14: finds the log by `fromBlock`/`toBlock` + `address`; nothing in the range before it; nothing for another address; also found by `blockHash` |
| `eth_get_logs_filters_by_topic_position` | Topic filtering end to end over two contracts (one log with no topics, one with `[0xaa…, 0xbb…]`) |
| `eth_gas_price_returns_the_fixed_price` | `0x3b9aca00` |

A fourth hand-assembled contract, `TWO_TOPIC_LOG`, provides the topics:

```text
60bb 60aa 6000 6000 a2 00    LOG2(offset 0, size 0, topic0 = 0xaa, topic1 = 0xbb), STOP
```

**Existing test whose expectation changed:**
`an_unsupported_method_gets_the_standard_method_not_found_error` used `eth_gasPrice`
as its example of an unimplemented method. It now uses `eth_getProof`, which is real
and still unimplemented. One-line comment in code.

`cargo test --workspace`: **75 passed**. `cargo clippy --all-targets`: clean.

### Manual check: stock Foundry, no `cast mktx`

```text
cast gas-price                               → 1000000000
cast send --legacy … <bob> --value 1ether    → status 1, gasUsed 21000
cast send --legacy … --create <initcode>     → status 1, gasUsed 55522, contractAddress 0xe7f1…0512
cast send --legacy … <contract>              → status 1, gasUsed 43487, 1 log
cast code <contract>                         → 0x602a60005560006000a000
cast storage <contract> 0                    → 0x…002a
cast logs --from-block 0 --address <contract> → 1 log, logIndex 0
```

None of these needed `--gas-price`, `--nonce`, or `--gas-limit`. The `cast send`
command is in the README.
