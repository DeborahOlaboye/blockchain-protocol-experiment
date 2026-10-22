use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, U256};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, address_of, one_eth, sign_call, sign_deployment,
        sign_transfer, sign_transfer_with,
    },
};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, GAS_PRICE},
    node::BreejaGenesis,
};

const BURNER_INITCODE: &str = "0x6001600c60003960016000f3fe";
const BURN_GAS: u64 = 1_000_000;

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

fn mine_until(chain: &Chain, hashes: &[B256]) {
    let handle = miner::spawn(chain.clone());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !hashes.iter().all(|h| chain.read().get_receipt(h).is_some()) {
        assert!(Instant::now() < deadline, "timed out waiting for receipts");
        std::thread::sleep(Duration::from_millis(10));
    }
    chain.shutdown();
    handle.join().unwrap();
}

#[test]
fn a_block_never_exceeds_the_gas_limit_with_transactions_of_varying_cost() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let chain = test_chain(&[(alice, one_eth()), (bob, one_eth())]);
    let burner = alice.create(0);

    let mut alice_txs = vec![
        sign_deployment(DEV_PRIVATE_KEY, BURNER_INITCODE.parse().unwrap(), 0, 100_000).unwrap(),
    ];
    for nonce in 1..=6 {
        alice_txs.push(sign_call(DEV_PRIVATE_KEY, burner, Bytes::new(), nonce, BURN_GAS).unwrap());
    }
    let bob_txs: Vec<Bytes> = (0..10)
        .map(|nonce| sign_transfer(DEV_PRIVATE_KEY_1, alice, U256::from(1u64), nonce).unwrap())
        .collect();

    let hashes: Vec<B256> = alice_txs
        .iter()
        .chain(&bob_txs)
        .map(|raw| chain.submit_raw_transaction(raw).unwrap())
        .collect();
    mine_until(&chain, &hashes);

    let state = chain.read();
    let mut fullest = 0;
    for number in 1..=state.get_block_number() {
        let block = state.get_block(number).unwrap();
        assert!(
            block.gas_used <= block.gas_limit,
            "block {number} used {} of {}",
            block.gas_used,
            block.gas_limit
        );
        let receipts_total: u64 = block
            .transactions
            .iter()
            .map(|hash| state.get_receipt(hash).unwrap().gas_used)
            .sum();
        assert_eq!(block.gas_used, receipts_total, "block {number}");
        fullest = fullest.max(block.gas_used);
    }
    let deployment = state.get_receipt(&hashes[0]).unwrap().gas_used;
    assert_eq!(fullest, deployment + 3 * BURN_GAS + 4 * 21_000);
    assert!(BLOCK_GAS_LIMIT - fullest < 21_000);

    for hash in &hashes[1..=6] {
        let receipt = state.get_receipt(hash).unwrap();
        assert_eq!(receipt.status, 0);
        assert_eq!(receipt.gas_used, BURN_GAS);
    }
    assert_eq!(state.get_account(&alice).nonce, 7);
    assert_eq!(state.get_account(&bob).nonce, 10);
}

#[test]
fn a_gas_limit_above_the_block_gas_limit_is_rejected_by_the_mempool() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth() * U256::from(10u64))]);

    let too_big = sign_transfer_with(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        U256::ZERO,
        0,
        GAS_PRICE,
        BLOCK_GAS_LIMIT + 1,
    )
    .unwrap();
    let error = chain.submit_raw_transaction(&too_big).unwrap_err();
    assert!(error.to_string().contains("block gas limit"), "{error}");

    let at_limit = sign_transfer_with(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        U256::ZERO,
        0,
        GAS_PRICE,
        BLOCK_GAS_LIMIT,
    )
    .unwrap();
    chain.submit_raw_transaction(&at_limit).unwrap();
}

#[test]
fn oversized_initcode_is_rejected_by_the_mempool() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let chain = test_chain(&[(alice, one_eth() * U256::from(10u64))]);
    let initcode = Bytes::from(vec![0x00; 49_153]);

    let raw = sign_deployment(DEV_PRIVATE_KEY, initcode, 0, BLOCK_GAS_LIMIT).unwrap();
    let error = chain.submit_raw_transaction(&raw).unwrap_err();
    assert!(error.to_string().contains("initcode"), "{error}");
}
