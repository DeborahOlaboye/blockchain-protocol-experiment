use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::{Address, B256, Bloom, Bytes, Log, TxKind, U256};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, BLOCK_REWARD, CHAIN_ID},
    node::{
        Account, Block, PendingTransaction, Transaction, TransactionReceipt, hash_list_root,
        logs_bloom,
    },
    params::CallRequest,
};
use revm::{
    Context, DatabaseCommit, ExecuteEvm, MainBuilder, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::result::{ExecutionResult, HaltReason},
    primitives::hardfork::SpecId,
    state::EvmState,
};

use crate::{evm::BreejaDb, pool::intrinsic_gas};

/// What executing a batch of transactions produced.
pub struct Execution {
    /// Account state after the batch, ready to become the new head state.
    pub accounts: HashMap<Address, Account>,
    pub code: HashMap<B256, Bytes>,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<TransactionReceipt>,
    pub gas_used: u64,
    pub transaction_root: B256,
    pub receipts_root: B256,
    pub logs_bloom: Bloom,
    pub state_root: B256,
}

#[derive(Debug, Clone, Copy)]
pub struct BlockContext {
    pub number: u64,
    pub timestamp: u64,
    pub parent_hash: B256,
    pub difficulty: u8,
    pub gas_limit: u64,
    pub miner: Address,
}

#[derive(Debug, Clone)]
pub struct TxOutcome {
    pub success: bool,
    pub gas_used: u64,
    pub created: Option<Address>,
    pub logs: Vec<Log>,
    pub output: Bytes,
    pub halt_reason: Option<String>,
}

impl TxOutcome {
    pub fn failure(&self) -> String {
        match &self.halt_reason {
            Some(reason) => format!("execution halted: {reason}"),
            None => format!("execution reverted: 0x{}", alloy::hex::encode(&self.output)),
        }
    }
}

impl BlockContext {
    pub fn on_top_of(parent: &Block, difficulty: u8, miner: Address) -> Self {
        Self {
            number: parent.number + 1,
            // A block's timestamp must advance, even if the system clock does not.
            timestamp: unix_now().max(parent.timestamp + 1),
            parent_hash: parent.hash,
            difficulty,
            gas_limit: BLOCK_GAS_LIMIT,
            miner,
        }
    }

    fn block_env(&self) -> BlockEnv {
        BlockEnv {
            number: U256::from(self.number),
            beneficiary: self.miner,
            timestamp: U256::from(self.timestamp),
            gas_limit: self.gas_limit,
            basefee: 0,
            difficulty: U256::from(self.difficulty),
            prevrandao: Some(self.parent_hash),
            ..Default::default()
        }
    }
}

fn cfg_env() -> CfgEnv {
    CfgEnv::new_with_spec(SpecId::SHANGHAI).with_chain_id(CHAIN_ID)
}

fn tx_env(tx: &PendingTransaction) -> TxEnv {
    TxEnv {
        tx_type: 0,
        caller: tx.from,
        gas_limit: tx.gas,
        gas_price: tx.gas_price,
        kind: match tx.to {
            Some(to) => TxKind::Call(to),
            None => TxKind::Create,
        },
        value: tx.value,
        data: tx.input.clone(),
        nonce: tx.nonce,
        chain_id: Some(tx.chain_id),
        ..Default::default()
    }
}

fn transact(
    db: &mut BreejaDb,
    block: &BlockContext,
    cfg: CfgEnv,
    tx: TxEnv,
) -> Result<(ExecutionResult<HaltReason>, EvmState), anyhow::Error> {
    let mut evm = Context::mainnet()
        .with_db(&mut *db)
        .with_block(block.block_env())
        .with_cfg(cfg)
        .build_mainnet();
    let outcome = evm
        .transact(tx)
        .map_err(|e| anyhow::anyhow!("transaction is invalid: {e}"))?;
    Ok((outcome.result, outcome.state))
}

fn summarize(result: &ExecutionResult<HaltReason>) -> TxOutcome {
    TxOutcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
        created: result.created_address(),
        logs: result.logs().to_vec(),
        output: result.output().cloned().unwrap_or_default(),
        halt_reason: match result {
            ExecutionResult::Halt { reason, .. } => Some(format!("{reason:?}")),
            _ => None,
        },
    }
}

pub fn execute_transaction(
    db: &mut BreejaDb,
    block: &BlockContext,
    tx: &PendingTransaction,
) -> Result<TxOutcome, anyhow::Error> {
    let (result, state) = transact(db, block, cfg_env(), tx_env(tx))?;
    db.commit(state);
    Ok(summarize(&result))
}

fn call_cfg() -> CfgEnv {
    let mut cfg = estimate_cfg();
    cfg.disable_balance_check = true;
    cfg.disable_eip3607 = true;
    cfg
}

fn estimate_cfg() -> CfgEnv {
    let mut cfg = cfg_env();
    cfg.disable_nonce_check = true;
    cfg
}

fn run_without_commit(
    db: &mut BreejaDb,
    block: &BlockContext,
    cfg: CfgEnv,
    request: &CallRequest,
    gas_limit: u64,
) -> Result<TxOutcome, anyhow::Error> {
    let (result, _discarded) = transact(db, block, cfg, simulation_tx(request, gas_limit)?)?;
    Ok(summarize(&result))
}

fn probe(
    db: &mut BreejaDb,
    block: &BlockContext,
    request: &CallRequest,
    gas_limit: u64,
) -> Result<TxOutcome, anyhow::Error> {
    run_without_commit(db, block, estimate_cfg(), request, gas_limit)
}

fn simulation_tx(request: &CallRequest, gas_limit: u64) -> Result<TxEnv, anyhow::Error> {
    let gas_price = match request.gas_price {
        Some(price) => u128::try_from(price)
            .map_err(|_| anyhow::anyhow!("gas price {price} does not fit in 128 bits"))?,
        None => 0,
    };
    Ok(TxEnv {
        tx_type: 0,
        caller: request.from.unwrap_or_default(),
        gas_limit,
        gas_price,
        kind: match request.to {
            Some(to) => TxKind::Call(to),
            None => TxKind::Create,
        },
        value: request.value.unwrap_or_default(),
        data: request.calldata(),
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    })
}

pub fn simulate(
    db: &mut BreejaDb,
    block: &BlockContext,
    request: &CallRequest,
    gas_limit: u64,
) -> Result<TxOutcome, anyhow::Error> {
    run_without_commit(db, block, call_cfg(), request, gas_limit)
}

pub fn estimate_gas(
    db: &mut BreejaDb,
    block: &BlockContext,
    request: &CallRequest,
) -> Result<u64, anyhow::Error> {
    let floor = intrinsic_gas(&request.calldata(), request.to.is_none());
    let ceiling = gas_ceiling(db, block, request)?;
    if ceiling < floor {
        anyhow::bail!("gas allowance {ceiling} is below the intrinsic cost {floor}");
    }

    let at_ceiling = probe(db, block, request, ceiling)?;
    if !at_ceiling.success {
        anyhow::bail!(
            "{} (with the maximum of {ceiling} gas, so no gas limit can make it succeed)",
            at_ceiling.failure()
        );
    }

    let minimal = minimal_gas_limit(db, block, request, floor, ceiling);
    if minimal == floor {
        return Ok(minimal);
    }
    Ok((minimal + minimal / ESTIMATE_MARGIN_DIVISOR).min(ceiling))
}

const ESTIMATE_MARGIN_DIVISOR: u64 = 10;

fn gas_ceiling(
    db: &BreejaDb,
    block: &BlockContext,
    request: &CallRequest,
) -> Result<u64, anyhow::Error> {
    let mut ceiling = block.gas_limit;
    if let Some(gas) = request.gas {
        ceiling = ceiling.min(gas.saturating_to::<u64>());
    }

    let gas_price = request.gas_price.unwrap_or_default();
    if gas_price > U256::ZERO {
        let from = request.from.unwrap_or_default();
        let balance = db
            .accounts
            .get(&from)
            .map(|account| account.balance)
            .unwrap_or_default();
        let value = request.value.unwrap_or_default();
        let spendable = balance.checked_sub(value).ok_or_else(|| {
            anyhow::anyhow!("insufficient funds: {from} has {balance} but sends {value}")
        })?;
        ceiling = ceiling.min((spendable / gas_price).saturating_to::<u64>());
    }
    Ok(ceiling)
}

pub fn minimal_gas_limit(
    db: &mut BreejaDb,
    block: &BlockContext,
    request: &CallRequest,
    floor: u64,
    ceiling: u64,
) -> u64 {
    let mut succeeds = |gas: u64| {
        probe(db, block, request, gas)
            .map(|outcome| outcome.success)
            .unwrap_or(false)
    };
    if succeeds(floor) {
        return floor;
    }
    let (mut failing, mut passing) = (floor, ceiling);
    while passing - failing > 1 {
        let middle = failing + (passing - failing) / 2;
        if succeeds(middle) {
            passing = middle;
        } else {
            failing = middle;
        }
    }
    passing
}

/// Current wall-clock time as a Unix timestamp in seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pays the miner for sealing a block.
///
/// This is the only way new ETH enters the supply, and it is what makes mining
/// worth the electricity. Breeja pays Frontier's 5 ETH; Ethereum reduced this
/// twice before dropping it entirely at the merge.
pub fn pay_block_reward(
    accounts: &mut HashMap<Address, Account>,
    miner: Address,
) -> Result<(), anyhow::Error> {
    accounts.entry(miner).or_default().credit(BLOCK_REWARD)?;
    Ok(())
}

pub fn execute_block(
    mut db: BreejaDb,
    batch: &[PendingTransaction],
    block: &BlockContext,
) -> Execution {
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut gas_used = 0u64;
    let mut log_index = 0u64;

    for tx in batch {
        if tx.gas > block.gas_limit - gas_used {
            continue;
        }
        match execute_transaction(&mut db, block, tx) {
            Ok(outcome) => {
                let index = transactions.len() as u64;
                gas_used += outcome.gas_used;

                let logs = outcome
                    .logs
                    .iter()
                    .map(|log| {
                        let log = primitives::node::Log {
                            address: log.address,
                            topics: log.topics().to_vec(),
                            data: log.data.data.clone(),
                            log_index,
                        };
                        log_index += 1;
                        log
                    })
                    .collect();

                transactions.push(Transaction {
                    hash: tx.hash,
                    // The block hash is unknown until the block is sealed, so it
                    // is stamped in afterwards.
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    transaction_index: index,
                    chain_id: tx.chain_id,
                    from: tx.from,
                    to: tx.to,
                    value: tx.value,
                    nonce: tx.nonce,
                    gas: tx.gas,
                    gas_price: tx.gas_price,
                    input: tx.input.clone(),
                    v: tx.v,
                    r: tx.r,
                    s: tx.s,
                });

                receipts.push(TransactionReceipt {
                    transaction_hash: tx.hash,
                    transaction_index: index,
                    block_hash: B256::ZERO,
                    block_number: block.number,
                    from: tx.from,
                    to: tx.to,
                    gas_used: outcome.gas_used,
                    cummulative_gas_used: gas_used,
                    effective_gas_price: tx.gas_price,
                    contract_address: outcome.created,
                    logs_bloom: logs_bloom(&logs),
                    logs,
                    status: outcome.success as u8,
                });
            }
            Err(error) => {
                tracing::warn!(tx = %tx.hash, %error, "dropping transaction that no longer applies");
            }
        }
    }

    // The miner's reward is paid after the transactions, so a miner cannot spend
    // it inside the very block that grants it.
    if let Err(error) = pay_block_reward(&mut db.accounts, block.miner) {
        tracing::error!(%error, "failed to pay block reward");
    }

    let mut block_bloom = Bloom::ZERO;
    for receipt in &receipts {
        block_bloom.accrue_bloom(&receipt.logs_bloom);
    }

    let tx_hashes: Vec<B256> = transactions.iter().map(|tx| tx.hash).collect();
    let receipt_hashes: Vec<B256> = receipts.iter().map(|r| r.transaction_hash).collect();

    Execution {
        transaction_root: hash_list_root(&tx_hashes),
        receipts_root: hash_list_root(&receipt_hashes),
        logs_bloom: block_bloom,
        state_root: primitives::node::state_root_of(&db.accounts),
        accounts: db.accounts,
        code: db.code_by_hash,
        transactions,
        receipts,
        gas_used,
    }
}

#[cfg(test)]
mod tests {
    use primitives::constants::{
        BLOCK_GAS_LIMIT, DEV_ADDRESS, DEV_ADDRESS_1, GAS_PRICE, TRANSFER_GAS,
    };

    use super::*;

    fn one_eth() -> U256 {
        U256::from(10u64).pow(U256::from(18))
    }

    fn funded(balance: U256) -> BreejaDb {
        let mut db = BreejaDb::default();
        db.accounts.insert(DEV_ADDRESS, Account::new(balance));
        db
    }

    fn block(miner: Address) -> BlockContext {
        BlockContext {
            number: 1,
            timestamp: 1,
            parent_hash: B256::repeat_byte(0x01),
            difficulty: 4,
            gas_limit: BLOCK_GAS_LIMIT,
            miner,
        }
    }

    fn transfer(value: U256, nonce: u64) -> PendingTransaction {
        PendingTransaction {
            hash: B256::repeat_byte(0xab),
            from: DEV_ADDRESS,
            to: Some(DEV_ADDRESS_1),
            value,
            nonce,
            gas: TRANSFER_GAS,
            gas_price: GAS_PRICE,
            input: Bytes::new(),
            chain_id: 1,
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
        }
    }

    fn contract_tx(to: Option<Address>, input: &[u8], nonce: u64, gas: u64) -> PendingTransaction {
        PendingTransaction {
            to,
            input: Bytes::copy_from_slice(input),
            gas,
            ..transfer(U256::ZERO, nonce)
        }
    }

    const STORE_AND_LOG_INITCODE: [u8; 23] = [
        0x60, 0x0b, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0b, 0x60, 0x00, 0xf3, 0x60, 0x2a, 0x60,
        0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa0, 0x00,
    ];

    #[test]
    fn a_deployment_reports_its_address_and_a_call_runs_the_code() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth());

        let deployed = execute_transaction(
            &mut db,
            &block(miner),
            &contract_tx(None, &STORE_AND_LOG_INITCODE, 0, 100_000),
        )
        .unwrap();
        assert!(deployed.success);
        let contract = deployed.created.expect("a deployment creates an address");
        assert_eq!(contract, DEV_ADDRESS.create(0));

        let called = execute_transaction(
            &mut db,
            &block(miner),
            &contract_tx(Some(contract), &[], 1, 100_000),
        )
        .unwrap();
        assert!(called.success);
        assert_eq!(called.logs.len(), 1);
        assert_eq!(called.logs[0].address, contract);
        assert_eq!(
            db.accounts[&contract].storage[&U256::ZERO],
            U256::from(42u64)
        );
    }

    #[test]
    fn running_out_of_gas_halts_and_consumes_the_whole_limit() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth());
        let contract = execute_transaction(
            &mut db,
            &block(miner),
            &contract_tx(None, &STORE_AND_LOG_INITCODE, 0, 100_000),
        )
        .unwrap()
        .created
        .unwrap();

        let starved = execute_transaction(
            &mut db,
            &block(miner),
            &contract_tx(Some(contract), &[], 1, 22_000),
        )
        .unwrap();

        assert!(!starved.success);
        assert_eq!(starved.gas_used, 22_000);
        assert_eq!(db.accounts[&DEV_ADDRESS].nonce, 2);
        assert!(db.accounts[&contract].storage.is_empty());
        assert!(starved.failure().contains("OutOfGas"));
    }

    fn deploy(db: &mut BreejaDb, initcode: &str) -> Address {
        let nonce = db.accounts[&DEV_ADDRESS].nonce;
        let initcode: Bytes = initcode.parse().unwrap();
        execute_transaction(
            db,
            &block(Address::repeat_byte(0x99)),
            &contract_tx(None, &initcode, nonce, 200_000),
        )
        .unwrap()
        .created
        .unwrap()
    }

    fn call_to(to: Address) -> CallRequest {
        CallRequest {
            from: Some(DEV_ADDRESS),
            to: Some(to),
            ..Default::default()
        }
    }

    #[test]
    fn simulating_a_call_returns_its_output_but_writes_nothing() {
        let mut db = funded(one_eth());
        let contract = deploy(&mut db, crate::testing::STORE_AND_RETURN_INITCODE);
        let before = db.accounts.clone();

        let outcome = simulate(
            &mut db,
            &block(Address::repeat_byte(0x99)),
            &call_to(contract),
            BLOCK_GAS_LIMIT,
        )
        .unwrap();

        assert!(outcome.success);
        let mut expected = [0u8; 32];
        expected[31] = 42;
        assert_eq!(outcome.output, Bytes::copy_from_slice(&expected));
        assert_eq!(db.accounts, before);
    }

    #[test]
    fn a_call_from_an_account_with_no_funds_still_runs() {
        let mut db = funded(one_eth());
        let request = CallRequest {
            to: Some(DEV_ADDRESS_1),
            value: Some(U256::from(1u64)),
            ..Default::default()
        };
        let outcome = simulate(
            &mut db,
            &block(Address::repeat_byte(0x99)),
            &request,
            BLOCK_GAS_LIMIT,
        )
        .unwrap();
        assert!(outcome.success);
        assert!(outcome.output.is_empty());
    }

    #[test]
    fn the_estimate_for_a_plain_transfer_is_exactly_21000() {
        let mut db = funded(one_eth() * U256::from(10u64));
        let request = CallRequest {
            value: Some(one_eth()),
            ..call_to(DEV_ADDRESS_1)
        };
        assert_eq!(
            estimate_gas(&mut db, &block(Address::repeat_byte(0x99)), &request).unwrap(),
            21_000
        );
    }

    #[test]
    fn the_binary_search_finds_the_lowest_gas_limit_that_succeeds() {
        let mut db = funded(one_eth());
        let contract = deploy(&mut db, crate::testing::STORE_AND_LOG_INITCODE);
        let context = block(Address::repeat_byte(0x99));
        let request = call_to(contract);

        let minimal = minimal_gas_limit(&mut db, &context, &request, TRANSFER_GAS, BLOCK_GAS_LIMIT);

        assert!(probe(&mut db, &context, &request, minimal).unwrap().success);
        assert!(
            !probe(&mut db, &context, &request, minimal - 1)
                .unwrap()
                .success
        );
        let estimate = estimate_gas(&mut db, &context, &request).unwrap();
        assert_eq!(estimate, minimal + minimal / ESTIMATE_MARGIN_DIVISOR);
    }

    #[test]
    fn estimating_a_transaction_that_always_reverts_is_an_error() {
        let mut db = funded(one_eth());
        let contract = deploy(&mut db, crate::testing::STORE_THEN_REVERT_INITCODE);

        let error = estimate_gas(
            &mut db,
            &block(Address::repeat_byte(0x99)),
            &call_to(contract),
        )
        .unwrap_err();
        assert!(error.to_string().contains("execution reverted"), "{error}");
    }

    #[test]
    fn estimating_a_transfer_the_sender_cannot_afford_is_an_error() {
        let mut db = funded(one_eth());
        let request = CallRequest {
            value: Some(one_eth() * U256::from(2u64)),
            ..call_to(DEV_ADDRESS_1)
        };
        assert!(estimate_gas(&mut db, &block(Address::repeat_byte(0x99)), &request).is_err());
    }

    #[test]
    fn a_transfer_through_revm_costs_exactly_21000_gas_and_pays_the_beneficiary_once() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));

        let outcome = execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();

        assert!(outcome.success);
        assert_eq!(outcome.gas_used, 21_000);
        assert_eq!(
            db.accounts[&miner].balance,
            U256::from(21_000_000_000_000u64)
        );
    }

    #[test]
    fn a_transfer_moves_value_charges_a_fee_and_bumps_the_nonce() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);

        let outcome = execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();
        assert_eq!(outcome.gas_used, TRANSFER_GAS);

        let sender = &db.accounts[&DEV_ADDRESS];
        assert_eq!(
            sender.balance,
            one_eth() * U256::from(10u64) - one_eth() - fee
        );
        assert_eq!(sender.nonce, 1);
        assert_eq!(db.accounts[&DEV_ADDRESS_1].balance, one_eth());
        assert_eq!(db.accounts[&miner].balance, fee);
    }

    #[test]
    fn no_eth_is_created_or_destroyed_by_a_transfer() {
        let miner = Address::repeat_byte(0x99);
        let start = one_eth() * U256::from(10u64);
        let mut db = funded(start);

        execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).unwrap();

        let total: U256 = db
            .accounts
            .values()
            .map(|a| a.balance)
            .fold(U256::ZERO, |a, b| a + b);
        assert_eq!(total, start);
    }

    #[test]
    fn a_transfer_the_sender_cannot_afford_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        // Enough for the value but not the fee on top.
        let mut db = funded(one_eth());
        assert!(execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 0)).is_err());
    }

    #[test]
    fn a_transfer_with_the_wrong_nonce_is_rejected() {
        let miner = Address::repeat_byte(0x99);
        let mut db = funded(one_eth() * U256::from(10u64));
        assert!(execute_transaction(&mut db, &block(miner), &transfer(one_eth(), 7)).is_err());
    }

    #[test]
    fn executing_a_block_pays_the_reward_and_records_receipts() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        let batch = vec![transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &block(miner));

        assert_eq!(execution.transactions.len(), 1);
        assert_eq!(execution.receipts.len(), 1);
        assert_eq!(execution.receipts[0].status, 1);
        assert_eq!(execution.gas_used, TRANSFER_GAS);
        assert_eq!(execution.receipts[0].cummulative_gas_used, TRANSFER_GAS);

        let fee = U256::from(TRANSFER_GAS) * U256::from(GAS_PRICE);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD + fee);

        // Non-empty roots, because this block committed to a transaction.
        assert_ne!(execution.transaction_root, B256::ZERO);
        assert_ne!(execution.state_root, B256::ZERO);
    }

    #[test]
    fn an_empty_block_still_pays_the_miner_but_commits_to_no_transactions() {
        let miner = Address::repeat_byte(0x99);
        let execution = execute_block(funded(one_eth()), &[], &block(miner));

        assert!(execution.transactions.is_empty());
        assert_eq!(execution.gas_used, 0);
        assert_eq!(execution.transaction_root, B256::ZERO);
        assert_eq!(execution.accounts[&miner].balance, BLOCK_REWARD);
    }

    #[test]
    fn a_transaction_that_no_longer_applies_is_skipped_not_mined() {
        let miner = Address::repeat_byte(0x99);
        let db = funded(one_eth() * U256::from(10u64));
        // Two transactions claiming the same nonce: only the first can apply.
        let batch = vec![transfer(one_eth(), 0), transfer(one_eth(), 0)];

        let execution = execute_block(db, &batch, &block(miner));
        assert_eq!(execution.transactions.len(), 1);
    }
}
