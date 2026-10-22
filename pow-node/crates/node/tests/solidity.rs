use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use node::{
    Chain, ChainConfig,
    evm::BreejaDb,
    executor::{BlockContext, TxOutcome, simulate},
    miner,
    testing::{
        DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, address_of, one_eth, sign_deployment, sign_legacy,
    },
};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, BLOCK_REWARD, GAS_PRICE},
    node::{BreejaGenesis, TransactionReceipt},
    params::CallRequest,
};

const VAULT_INITCODE: &str = include_str!("../../../demo/Vault.bin");

fn selector(signature: &str) -> [u8; 4] {
    keccak256(signature)[..4].try_into().unwrap()
}

fn calldata(signature: &str, args: &[B256]) -> Bytes {
    let mut data = selector(signature).to_vec();
    for arg in args {
        data.extend_from_slice(arg.as_slice());
    }
    data.into()
}

fn word(value: U256) -> B256 {
    B256::from(value)
}

fn mapping_slot(key: Address, slot: u64) -> U256 {
    let mut preimage = key.into_word().to_vec();
    preimage.extend_from_slice(&U256::from(slot).to_be_bytes::<32>());
    keccak256(preimage).into()
}

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

fn eth_call(chain: &Chain, from: Address, to: Address, data: Bytes) -> TxOutcome {
    let (mut db, block) = {
        let state = chain.read();
        let block = BlockContext::on_top_of(state.head(), state.head().difficulty, Address::ZERO);
        (BreejaDb::snapshot(&state), block)
    };
    let request = CallRequest {
        from: Some(from),
        to: Some(to),
        data: Some(data),
        ..Default::default()
    };
    simulate(&mut db, &block, &request, BLOCK_GAS_LIMIT).unwrap()
}

#[test]
fn a_compiled_solidity_vault_holds_and_returns_eth() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let start = one_eth() * U256::from(10u64);
    let chain = test_chain(&[(alice, start), (bob, start)]);
    let vault = alice.create(0);

    let two = one_eth() * U256::from(2u64);
    let half = one_eth() / U256::from(2u64);
    let five = one_eth() * U256::from(5u64);
    let send = |key: &str, nonce: u64, value: U256, data: Bytes| {
        let raw = sign_legacy(key, TxKind::Call(vault), value, data, nonce, GAS_PRICE, 150_000)
            .unwrap();
        chain.submit_raw_transaction(&raw).unwrap()
    };

    let initcode: Bytes = VAULT_INITCODE.trim().parse().unwrap();
    let deploy = sign_deployment(DEV_PRIVATE_KEY, initcode, 0, 400_000).unwrap();
    let hashes = [
        chain.submit_raw_transaction(&deploy).unwrap(),
        send(DEV_PRIVATE_KEY, 1, two, calldata("deposit()", &[])),
        send(DEV_PRIVATE_KEY_1, 0, one_eth(), calldata("deposit()", &[])),
        send(DEV_PRIVATE_KEY, 2, U256::ZERO, calldata("withdraw(uint256)", &[word(half)])),
        send(DEV_PRIVATE_KEY, 3, U256::ZERO, calldata("withdraw(uint256)", &[word(five)])),
    ];
    let receipts = mine_until_receipts(&chain, &hashes);

    assert_eq!(receipts[0].contract_address, Some(vault));
    assert!(receipts[..4].iter().all(|r| r.status == 1));
    assert_eq!(receipts[4].status, 0, "withdrawing more than deposited reverts");
    assert!(receipts[4].logs.is_empty());

    let deposited = keccak256("Deposited(address,uint256)");
    let withdrawn = keccak256("Withdrawn(address,uint256)");
    let expected_events = [
        (&receipts[1], deposited, alice, two),
        (&receipts[2], deposited, bob, one_eth()),
        (&receipts[3], withdrawn, alice, half),
    ];
    for (receipt, event, account, amount) in expected_events {
        assert_eq!(receipt.logs.len(), 1);
        let log = &receipt.logs[0];
        assert_eq!(log.address, vault);
        assert_eq!(log.topics, vec![event, account.into_word()]);
        assert_eq!(log.data.as_ref(), amount.to_be_bytes::<32>());
    }

    let held = two + one_eth() - half;
    let alice_share = two - half;
    {
        let state = chain.read();
        assert_eq!(state.get_account(&vault).balance, held);
        assert_eq!(state.get_storage_at(&vault, U256::from(1u64)), held);
        assert_eq!(state.get_storage_at(&vault, mapping_slot(alice, 0)), alice_share);
        assert_eq!(state.get_storage_at(&vault, mapping_slot(bob, 0)), one_eth());
        assert_eq!(state.get_account(&alice).nonce, 4);

        let blocks = U256::from(state.get_block_number());
        let total = [alice, bob, vault, chain.config().miner]
            .iter()
            .map(|a| state.get_account(a).balance)
            .fold(U256::ZERO, |acc, b| acc + b);
        assert_eq!(total, start + start + BLOCK_REWARD * blocks);
    }

    let balance = eth_call(&chain, alice, vault, calldata("balanceOf(address)", &[alice.into_word()]));
    assert!(balance.success);
    assert_eq!(balance.output.as_ref(), alice_share.to_be_bytes::<32>());

    let refused = eth_call(&chain, alice, vault, calldata("withdraw(uint256)", &[word(five)]));
    assert!(!refused.success);
    let mut expected = selector("InsufficientBalance(uint256,uint256)").to_vec();
    expected.extend_from_slice(&alice_share.to_be_bytes::<32>());
    expected.extend_from_slice(&five.to_be_bytes::<32>());
    assert_eq!(refused.output.as_ref(), expected.as_slice());
}
