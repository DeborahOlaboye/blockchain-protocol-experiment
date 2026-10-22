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
