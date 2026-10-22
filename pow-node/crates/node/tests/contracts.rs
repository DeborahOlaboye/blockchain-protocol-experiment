use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bloom, BloomInput, Bytes, U256};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, STORE_AND_LOG_INITCODE, STORE_AND_LOG_RUNTIME, STORE_THEN_REVERT_INITCODE,
        address_of, one_eth, sign_call, sign_deployment,
    },
};
use primitives::{
    constants::GAS_PRICE,
    node::{BreejaGenesis, TransactionReceipt, logs_bloom},
};

fn test_chain(funded: &[(Address, U256)]) -> Chain {
    let config = ChainConfig {
        difficulty: 4,
        target_block_time: Duration::from_millis(50),
        miner: Address::repeat_byte(0x99),
    };
    Chain::new(
        BreejaGenesis {
            accounts: funded.to_vec(),
        },
        config,
    )
}

fn mine_until_receipts(chain: &Chain, hashes: &[B256]) -> Vec<TransactionReceipt> {
    let handle = miner::spawn(chain.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hashes.iter().all(|h| chain.read().get_receipt(h).is_some()) {
        assert!(Instant::now() < deadline, "timed out waiting for receipts");
        std::thread::sleep(Duration::from_millis(10));
    }
    chain.shutdown();
    handle.join().unwrap();

    let state = chain.read();
    hashes
        .iter()
        .map(|h| state.get_receipt(h).unwrap().clone())
        .collect()
}

fn bytes(hex: &str) -> Bytes {
    hex.parse().unwrap()
}

#[test]
fn deploying_a_contract_sets_contract_address_and_commits_its_code() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth())]);

    let raw = sign_deployment(DEV_PRIVATE_KEY, bytes(STORE_AND_LOG_INITCODE), 0, 100_000).unwrap();
    let hash = chain.submit_raw_transaction(&raw).unwrap();
    let receipt = &mine_until_receipts(&chain, &[hash])[0];

    assert_eq!(receipt.status, 1);
    assert_eq!(receipt.to, None);
    let contract = receipt
        .contract_address
        .expect("a deployment receipt names the new contract");
    assert_eq!(contract, alice.create(0));
    assert!(
        (55_000..56_000).contains(&receipt.gas_used),
        "deployment used {} gas",
        receipt.gas_used
    );

    let state = chain.read();
    assert_eq!(state.get_code(&contract), bytes(STORE_AND_LOG_RUNTIME));
    assert_eq!(state.get_account(&contract).nonce, 1);
    assert_eq!(state.get_transaction(&hash).unwrap().to, None);
    assert_eq!(
        state.get_transaction(&hash).unwrap().input,
        bytes(STORE_AND_LOG_INITCODE)
    );
}

#[test]
fn calling_a_contract_changes_storage_and_emits_a_log() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth())]);
    let contract = alice.create(0);

    let deploy =
        sign_deployment(DEV_PRIVATE_KEY, bytes(STORE_AND_LOG_INITCODE), 0, 100_000).unwrap();
    let call = sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, 100_000).unwrap();
    let hashes = [
        chain.submit_raw_transaction(&deploy).unwrap(),
        chain.submit_raw_transaction(&call).unwrap(),
    ];
    assert_eq!(
        chain.read().get_storage_at(&contract, U256::ZERO),
        U256::ZERO
    );

    let receipts = mine_until_receipts(&chain, &hashes);
    let receipt = &receipts[1];

    assert_eq!(receipt.status, 1);
    assert_eq!(receipt.to, Some(contract));
    assert_eq!(receipt.contract_address, None);
    assert!(
        (43_000..44_000).contains(&receipt.gas_used),
        "call used {} gas",
        receipt.gas_used
    );
    assert_eq!(receipt.logs.len(), 1);
    assert_eq!(receipt.logs[0].address, contract);
    assert!(receipt.logs[0].topics.is_empty());
    assert!(receipt.logs[0].data.is_empty());

    assert_eq!(
        chain.read().get_storage_at(&contract, U256::ZERO),
        U256::from(42u64)
    );
}

#[test]
fn a_reverting_transaction_is_mined_with_status_zero_and_still_charged() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth())]);
    let contract = alice.create(0);

    let deploy = sign_deployment(
        DEV_PRIVATE_KEY,
        bytes(STORE_THEN_REVERT_INITCODE),
        0,
        100_000,
    )
    .unwrap();
    let call = sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, 100_000).unwrap();
    let hashes = [
        chain.submit_raw_transaction(&deploy).unwrap(),
        chain.submit_raw_transaction(&call).unwrap(),
    ];

    let receipts = mine_until_receipts(&chain, &hashes);
    let (deployed, reverted) = (&receipts[0], &receipts[1]);

    assert_eq!(deployed.status, 1);
    assert_eq!(reverted.status, 0);
    assert!(reverted.gas_used > 21_000);
    assert!(reverted.gas_used < 100_000);
    assert!(reverted.logs.is_empty());

    let state = chain.read();
    assert!(state.get_transaction(&hashes[1]).is_some());
    let block = state.get_block_by_hash(&reverted.block_hash).unwrap();
    assert!(block.transactions.contains(&hashes[1]));

    let alice_now = state.get_account(&alice);
    assert_eq!(alice_now.nonce, 2);
    let spent = U256::from(deployed.gas_used + reverted.gas_used) * U256::from(GAS_PRICE);
    assert_eq!(alice_now.balance, one_eth() - spent);

    assert_eq!(state.get_storage_at(&contract, U256::ZERO), U256::ZERO);
}

#[test]
fn logs_bloom_is_set_for_blocks_with_logs_and_matches_them() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth())]);
    let contract = alice.create(0);

    let deploy =
        sign_deployment(DEV_PRIVATE_KEY, bytes(STORE_AND_LOG_INITCODE), 0, 100_000).unwrap();
    let call = sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, 100_000).unwrap();
    let hashes = [
        chain.submit_raw_transaction(&deploy).unwrap(),
        chain.submit_raw_transaction(&call).unwrap(),
    ];
    let receipts = mine_until_receipts(&chain, &hashes);
    let (deployed, called) = (&receipts[0], &receipts[1]);

    assert_eq!(deployed.logs_bloom, Bloom::ZERO);
    assert_ne!(called.logs_bloom, Bloom::ZERO);
    assert_eq!(called.logs_bloom, logs_bloom(&called.logs));
    assert!(
        called
            .logs_bloom
            .contains_input(BloomInput::Raw(contract.as_slice()))
    );

    let state = chain.read();
    let block = state.get_block_by_hash(&called.block_hash).unwrap();
    assert!(
        block
            .logs_bloom
            .contains_input(BloomInput::Raw(contract.as_slice()))
    );
    let mut expected = Bloom::ZERO;
    for hash in &block.transactions {
        expected.accrue_bloom(&state.get_receipt(hash).unwrap().logs_bloom);
    }
    assert_eq!(block.logs_bloom, expected);
    assert_eq!(state.get_block(0).unwrap().logs_bloom, Bloom::ZERO);
}
