# EVM-NOTES: giving Breeja an EVM

Route: **REVM 43.0.3**, the version FINAL-PROJECT.md was verified against. The
milestone-by-milestone detail (exact tests, numbers, and live-node transcripts) is
in [`EVM-LOG.md`](./EVM-LOG.md). This document is the reasoning.

## Where it stands

All six milestones are done. 48 baseline tests became **84**, `cargo clippy
--all-targets` is clean, and the supply-conservation canary passed unmodified at
every step. Stock Foundry works against the node with no flags beyond `--legacy`.

| Check | Expected | Measured |
| --- | --- | --- |
| Plain transfer | 21,000 | 21,000 (`eth_estimateGas` = `0x5208`) |
| Beneficiary paid for one transfer | 21,000 gwei | 21,000 gwei |
| Deploy the spec's test contract | ~55,522 | 55,522 |
| Call it | ~43,487 | 43,487 |

## Part 0 answers

1. **Pool checks.** The `TxKind::Create` and non-empty-calldata rejections are
   *removed*. The `gas < TRANSFER_GAS` floor is *relaxed* into real intrinsic gas:
   21,000, plus 4 or 16 per calldata byte, plus 32,000 and 2 per initcode word for a
   creation. It is tested against REVM's own calculation.
2. **Skip or include failures?** Include. A valid transaction that reverts still did
   work, so Ethereum mines it, charges it, advances its nonce, and sets `status: 0`.
   Skipping it would give a free retry. Only transactions REVM refuses outright (bad
   nonce, cannot afford the up-front reservation) are dropped.
3. **Reserve the limit, charge the usage.** Ethereum takes `gas_limit × price` up front
   and refunds what is unused, because usage is unknown before execution. Breeja's
   fixed 21,000 made that look like ceremony. With variable gas it is required, and
   REVM itself refuses a sender who cannot cover it.
4. **Per-block snapshots with storage.** Each snapshot now copies every storage slot,
   so the cost grows as *blocks × storage*. I kept snapshots, because a teaching chain
   is small, but stored code once by hash so they do not copy bytecode too. A real node
   stores diffs or a trie with shared nodes.

## The decisions that matter

**The database is a snapshot, never the live state.** REVM borrows its `Database`
mutably for the whole execution, and `BlockState` sits behind the `RwLock` the RPC
server reads through. `BreejaDb` is a copy taken under a brief read lock. The miner
executes on it with no lock held, and `eth_call` and `eth_estimateGas` do the same.
That inversion (REVM *asks* for balances, code, and slots as it runs, then hands back
a change set for `DatabaseCommit`) is the core of the integration.

**Accounts.** Storage is a map in `Account`, skipped by RLP, so the encoding stays
Ethereum's four fields. Code lives once in a map keyed by hash, and the account holds
only the hash. Code is immutable, so that map needs no history, and historical
`eth_getCode` is free.

**Small choices in the database, each deliberate:**

- `basic()` returns `None` for an unknown address, not an empty account. `None` means
  "does not exist", which Ethereum's 25,000-gas new-account charge depends on.
- Zero slots are deleted rather than stored, so equal states hash equally.
- Code is stored as `original_bytes()`, because REVM pads analysed bytecode.
- A created account starts with empty storage.
- EIP-161: a touched account left empty is deleted.
- Breeja's "no code" stays `B256::ZERO`, and the bridge translates to REVM's
  `KECCAK_EMPTY` and back, so there are never two spellings in the state root.

**Execution context is fixed before execution.** The miner used to pick the block
timestamp *after* executing. But `TIMESTAMP`, `NUMBER`, `COINBASE`, and `PREVRANDAO`
are read *during* execution, so the miner now builds a `BlockContext` first and
copies the same values into the header. That is also why `prevrandao` is the parent
hash: this block's hash depends on the state root that execution produces.

**`eth_call` versus a transaction.** These differ in one line: the transaction commits
REVM's change set and the call drops it. For `CfgEnv`, both turn off the nonce check,
since a simulation is not part of the sender's sequence. `eth_call` also turns off the
balance check and EIP-3607, because it asks "what does this code return?". The
estimate keeps them, because it asks "would this succeed if sent?". Those two flags
are behind REVM cargo features that are off by default, so mining is unaffected. A
revert is error code 3 with the revert data, which is what clients decode.

**`eth_estimateGas` is a heuristic.** It checks success at the ceiling (an error if
even that fails), binary searches down to the lowest passing limit, and adds 10% only
when code ran, so a transfer is exactly `0x5208`. A test proves the result is exact
(`minimal` passes, `minimal − 1` fails). But the search assumes success is monotonic
in gas, and a contract can read `GAS` and branch on it, and EIP-150's 63/64 forwarding
can make an inner call fail at one limit and succeed at another. The margin makes this
rarer. It does not remove it.

**`eth_getLogs` topic rules.** Positions are AND, a list inside a position is OR,
`null` is a positional wildcard, and, following geth, a filter with more positions
than the log has topics never matches, even if the extras are `null`. The spec
wording could be read either way, and clients are written against geth.

**Blocks fill by gas.** A transaction must satisfy `gas_limit ≤ block limit − used`,
which uses the *limit* because usage is unknown in advance. One that does not fit
waits in the mempool. The mempool also rejects limits above the block limit, which
could never be mined.

**`logsBloom`** is the Yellow Paper's 2048-bit filter, implemented by hand and
checked bit for bit against `alloy`. It is set per receipt, ORed per block, and the
block bloom is in the hashed header, as in Ethereum.

**The state root commits to storage and code.** `state_root_of` computes each storage
root from the actual slots (sorted, hashed) instead of trusting a cached field, so it
cannot be stale. Code is committed through `code_hash`.

**Frontier consensus, Shanghai execution.** Breeja keeps Frontier's 5 ETH reward, pi
million gas limit, and no fee market, but executes under `SpecId::SHANGHAI`, because
current Solidity emits `PUSH0`. The demo contract's bytecode contains it and would not
run under Frontier. No real chain ever had this combination. It is a deliberate
teaching compromise: the consensus rules being taught are pre-merge, and the execution
rules have to accept code people actually write today. `basefee = 0` is what keeps the
two consistent: nothing is burned, so the supply rule still holds.

## Pitfalls

1. **REVM pays the miner.** The manual fee credit is removed and the block reward kept.
   A test pins the beneficiary at exactly 21,000 gwei for one transfer.
2. **Basefee burns.** It is `0`.
3. **REVM bumps the nonce.** The manual increment is removed.
4. **`gas_used()` is deprecated.** `tx_gas_used()` is used everywhere.
5. **`SpecId`.** Shanghai, discussed above.
6. **Borrow and lock.** The EVM is dropped before commit, and no lock is held during
   execution.
7. **`prevrandao`.** The parent hash.

## What broke, and what I learned from it

- **Using real tools found bugs the tests could not.** The first plain `cast send`
  failed: receipts serialized the field as `cummulativeGasUsed`, misspelled since
  before this project. Every client requires `cumulativeGasUsed`. Later, `cast` could
  not parse blocks at all, because they lacked `sha3Uncles` and `mixHash`. My tests had
  passed because they only read fields they asked for by name. Both are fixed, and a
  test now parses blocks, receipts, and logs with `alloy`'s own types.
- **The timestamp ordering bug** above was invisible to every existing test, because
  none of them ran code that reads `TIMESTAMP`.
- **My demo misled before the node did.** It reported "Alice gained 5.5 ETH" after a
  0.5 ETH withdrawal. In `--dev` mode Alice is the miner, and the block reward was in
  her balance. The node was right. The demo now measures the vault instead.
- **Version drift is real.** `disable_balance_check` did not compile until I found
  the cargo feature that gates it in REVM's own source, as the spec's "getting help"
  section predicts.

## Honest limitations

- `eth_estimateGas` can be wrong for gas-dependent contracts (see above).
- `eth_getLogs` has no range cap and does not use blooms to skip blocks. It scans
  every block in range under the read lock.
- The state root is a sorted hash, not a Merkle-Patricia trie, so there is no
  `eth_getProof` and no inclusion proofs.
- `pending` is treated as `latest` for `eth_call`, `eth_getCode`, `eth_getStorageAt`,
  and `eth_getLogs`. Only balance and nonce consider the mempool.
- `eth_call` ignores the caller's `gas` above the block limit and does not accept
  state overrides.
- Full state snapshots per block still cost *blocks × storage* in memory.
- No precompile, tracing, persistence, or fee-market stretch goals were attempted.

## Tests

84 tests (48 at baseline), covering every item in the spec's list (unit 1–4,
integration 5–9, RPC 10–14), plus topic filtering, bloom correctness, block packing,
the mempool caps, client-type parsing, and the Solidity vault.

Existing tests' expectations changed only where they depended on there being no EVM:
the `eth_call` and `eth_estimateGas` refusals, `eth_gasPrice` as the "unimplemented"
example, and the `needs_evm` test, replaced one-for-one because the function was
removed. Each carries a one-line comment. A few others needed shape-only edits with
the same expectations: `tx.to` became `Some(bob)`, a hand-built header gained
`logs_bloom`, and the transfer receipt test gained a `cumulativeGasUsed` assertion.
The supply canary was never touched.
