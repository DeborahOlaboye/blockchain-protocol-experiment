use std::time::Duration;

use alloy::primitives::{Address, Bytes, U256};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use node::{
    Chain, ChainConfig, miner,
    testing::{
        DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, STORE_AND_LOG_INITCODE, STORE_AND_LOG_RUNTIME,
        STORE_AND_RETURN_INITCODE, STORE_THEN_REVERT_INITCODE, TWO_TOPIC_LOG_INITCODE, address_of,
        one_eth, sign_call, sign_deployment, sign_transfer,
    },
};
use primitives::node::{BreejaGenesis, TransactionReceipt};
use rpc::{RPCContext, start_server};
use serde_json::Value;

/// A running node and a client pointed at it.
struct Harness {
    client: HttpClient,
    chain: Chain,
    _server: jsonrpsee::server::ServerHandle,
    miner: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Starts a node with the given funded accounts, mining on a dedicated thread.
    async fn start(funded: &[(Address, U256)], mine: bool) -> Self {
        let config = ChainConfig {
            difficulty: 4,
            target_block_time: Duration::from_millis(50),
            miner: Address::repeat_byte(0x99),
        };
        let chain = Chain::new(
            BreejaGenesis {
                accounts: funded.to_vec(),
            },
            config,
        );

        // Port 0 lets the OS pick a free port, so tests can run in parallel.
        let (server, address) = start_server(RPCContext::new(chain.clone()), "127.0.0.1:0")
            .await
            .expect("server should start");
        let url = format!("http://{address}");
        let client = HttpClientBuilder::default().build(url).unwrap();

        let miner = mine.then(|| miner::spawn(chain.clone()));

        Self {
            client,
            chain,
            _server: server,
            miner,
        }
    }

    /// Polls `predicate` until it holds, or panics.
    async fn wait_for(&self, label: &str, mut predicate: impl FnMut(&Chain) -> bool) {
        for _ in 0..1_000 {
            if predicate(&self.chain) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {label}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.chain.shutdown();
        if let Some(handle) = self.miner.take() {
            let _ = handle.join();
        }
    }
}

/// Parses a hex quantity the way a client would.
fn from_quantity(raw: &str) -> u128 {
    u128::from_str_radix(raw.trim_start_matches("0x"), 16).expect("a valid hex quantity")
}

#[tokio::test]
async fn eth_chain_id_returns_the_configured_chain_id() {
    let harness = Harness::start(&[], false).await;
    let id: String = harness
        .client
        .request("eth_chainId", rpc_params![])
        .await
        .unwrap();
    assert_eq!(id, "0x1");
}

#[tokio::test]
async fn eth_block_number_starts_at_genesis_and_climbs_as_blocks_are_mined() {
    let harness = Harness::start(&[], true).await;

    harness
        .wait_for("blocks to be mined", |c| c.read().get_block_number() >= 3)
        .await;

    let number: String = harness
        .client
        .request("eth_blockNumber", rpc_params![])
        .await
        .unwrap();
    assert!(
        number.starts_with("0x"),
        "must be a hex quantity, got {number}"
    );
    assert!(from_quantity(&number) >= 3);
}

#[tokio::test]
async fn eth_get_block_by_number_accepts_tags_and_numbers() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 2)
        .await;

    // By tag, with the fullTransactions flag.
    let latest: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .unwrap();
    assert!(latest["hash"].as_str().unwrap().starts_with("0x"));

    // By tag, with the flag omitted entirely — also valid JSON-RPC.
    let genesis: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["earliest"])
        .await
        .unwrap();
    assert_eq!(genesis["number"], "0x0");
    assert_eq!(
        genesis["parentHash"],
        format!("0x{}", "0".repeat(64)),
        "genesis has no parent"
    );

    // By number.
    let first: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    assert_eq!(first["number"], "0x1");

    // Every field a client expects is present and hex-encoded.
    for field in [
        "number",
        "hash",
        "parentHash",
        "stateRoot",
        "transactionsRoot",
        "receiptsRoot",
        "difficulty",
        "totalDifficulty",
        "timestamp",
        "gasLimit",
        "gasUsed",
        "miner",
        "extraData",
        "nonce",
        "size",
    ] {
        let value = first[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        assert!(value.starts_with("0x"), "{field} is not hex: {value}");
    }
    // The proof-of-work nonce is a fixed-width 8-byte field, not a quantity.
    assert_eq!(first["nonce"].as_str().unwrap().len(), 18);
}

#[tokio::test]
async fn eth_get_block_by_number_rejects_a_block_that_does_not_exist() {
    let harness = Harness::start(&[], false).await;
    let result: Result<Value, _> = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0xffffff", false])
        .await;
    assert!(
        result.is_err(),
        "a future block should be an error, not null"
    );
}

#[tokio::test]
async fn eth_get_block_by_hash_finds_the_same_block_as_by_number() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 1)
        .await;

    let by_number: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    let hash = by_number["hash"].as_str().unwrap().to_owned();

    let by_hash: Value = harness
        .client
        .request("eth_getBlockByHash", rpc_params![hash, false])
        .await
        .unwrap();
    assert_eq!(by_hash, by_number);

    // An unknown hash is null, not an error.
    let missing: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![format!("0x{}", "11".repeat(32)), false],
        )
        .await
        .unwrap();
    assert!(missing.is_null());
}

#[tokio::test]
async fn eth_get_balance_reads_current_pending_and_historical_state() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    // Latest, and the one-parameter form.
    let balance: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(balance, "0x56bc75e2d63100000");
    let without_tag: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice])
        .await
        .unwrap();
    assert_eq!(without_tag, balance);

    // An address nobody funded is zero, not an error.
    let empty: String = harness
        .client
        .request(
            "eth_getBalance",
            rpc_params![Address::repeat_byte(0x77), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(empty, "0x0");

    // Send 5 ETH and wait for it to land.
    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth() * U256::from(5u64), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_account(&bob).balance > U256::ZERO
        })
        .await;

    let bob_now: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "latest"])
        .await
        .unwrap();
    assert_eq!(from_quantity(&bob_now), 5 * 10u128.pow(18));

    // At genesis Bob had nothing, and the chain still remembers that.
    let bob_at_genesis: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "0x0"])
        .await
        .unwrap();
    assert_eq!(bob_at_genesis, "0x0");

    let _ = tx_hash;
}

#[tokio::test]
async fn eth_get_transaction_count_tracks_mined_and_pending_nonces() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    // No miner: the transactions stay pending, which is what this test is about.
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], false).await;

    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    for nonce in 0..2 {
        let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), nonce).unwrap();
        let _: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
    }

    // "latest" still sees no mined transactions...
    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    // ...but "pending" includes the queued ones, which is the nonce a wallet must
    // use for its next transaction.
    let pending: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "pending"])
        .await
        .unwrap();
    assert_eq!(pending, "0x2");
}

#[tokio::test]
async fn a_transfer_produces_a_transaction_and_a_receipt() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    assert_eq!(tx_hash.len(), 66, "a transaction hash is 32 bytes of hex");

    // Before it is mined, there is a transaction but no receipt. That difference
    // is exactly how a client knows to keep waiting.
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_receipt(&tx_hash.parse().unwrap()).is_some()
        })
        .await;

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(tx["from"], format!("{alice:#x}"));
    assert_eq!(tx["to"], format!("{bob:#x}"));
    assert_eq!(tx["value"], "0xde0b6b3a7640000");
    assert_eq!(tx["nonce"], "0x0");
    assert_eq!(tx["input"], "0x");
    assert_eq!(tx["type"], "0x0", "legacy transaction");
    assert_eq!(tx["chainId"], "0x1");

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["gasUsed"], "0x5208");
    assert_eq!(receipt["cumulativeGasUsed"], "0x5208");
    assert_eq!(receipt["from"], format!("{alice:#x}"));
    assert_eq!(receipt["to"], format!("{bob:#x}"));
    assert!(
        receipt["contractAddress"].is_null(),
        "no contracts without an EVM"
    );
    assert_eq!(receipt["logs"].as_array().unwrap().len(), 0);
    assert_eq!(receipt["blockHash"], tx["blockHash"]);

    // The block it landed in lists it, and can inline it.
    let block: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), true],
        )
        .await
        .unwrap();
    let inlined = block["transactions"].as_array().unwrap();
    assert_eq!(inlined.len(), 1);
    assert_eq!(inlined[0]["hash"], tx_hash);

    // With the flag off, the same block lists bare hashes instead.
    let hashes_only: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), false],
        )
        .await
        .unwrap();
    assert_eq!(hashes_only["transactions"][0], tx_hash);
}

#[tokio::test]
async fn an_unknown_transaction_or_receipt_is_null() {
    let harness = Harness::start(&[], false).await;
    let unknown = format!("0x{}", "11".repeat(32));

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![unknown.clone()])
        .await
        .unwrap();
    assert!(tx.is_null());

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![unknown])
        .await
        .unwrap();
    assert!(receipt.is_null());
}

#[tokio::test]
async fn eth_send_raw_transaction_rejects_what_the_node_cannot_execute() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], false).await;

    // Not hex at all.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["not hex"])
        .await;
    assert!(result.is_err());

    // Hex, but not a transaction.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["0xdeadbeef"])
        .await;
    assert!(result.is_err());

    // A real transaction for more ETH than the sender has.
    let raw = sign_transfer(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        one_eth() * U256::from(1_000u64),
        0,
    )
    .unwrap();
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await;
    let error = result.expect_err("an unaffordable transfer must be rejected");
    assert!(
        error.to_string().contains("insufficient funds"),
        "the error should say why: {error}"
    );
}

// Changed: calldata to an address with no code runs nothing and returns 0x, not a "no EVM" error.
#[tokio::test]
async fn eth_call_to_an_account_without_code_returns_no_data() {
    let harness = Harness::start(&[], false).await;

    // A call with no calldata invokes no code, so it returns no data.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": Address::repeat_byte(0x22), "value": "0x1"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");

    // The one-parameter form is valid too.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": Address::repeat_byte(0x22)})],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");

    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![
                serde_json::json!({"to": Address::repeat_byte(0x22), "data": "0x70a08231"}),
                "latest"
            ],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");
}

#[tokio::test]
async fn eth_estimate_gas_returns_the_intrinsic_cost_of_a_transfer() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(10u64))], false).await;

    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice,
                "to": Address::repeat_byte(0x22),
                "value": "0xde0b6b3a7640000"
            })],
        )
        .await
        .unwrap();
    assert_eq!(estimate, "0x5208", "21000 gas");

    // Changed: 0x6080604052 is valid initcode, so with an EVM this deployment estimate succeeds.
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "data": "0x6080604052"})],
        )
        .await
        .unwrap();
    assert!(
        from_quantity(&estimate)
            >= node::pool::intrinsic_gas(&[0x60, 0x80, 0x60, 0x40, 0x52], true) as u128
    );
}

async fn deploy(harness: &Harness, initcode: &str, nonce: u64) -> Address {
    let raw = sign_deployment(DEV_PRIVATE_KEY, initcode.parse().unwrap(), nonce, 200_000).unwrap();
    let hash = harness.chain.submit_raw_transaction(&raw).unwrap();
    harness
        .wait_for("the deployment to be mined", |c| {
            c.read().get_receipt(&hash).is_some()
        })
        .await;
    harness
        .chain
        .read()
        .get_receipt(&hash)
        .unwrap()
        .contract_address
        .unwrap()
}

#[tokio::test]
async fn eth_call_returns_contract_output_and_leaves_state_unchanged() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_AND_RETURN_INITCODE, 0).await;

    let output: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"from": alice, "to": contract}), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(output, format!("0x{:064x}", 42));

    {
        let state = harness.chain.read();
        assert_eq!(state.get_storage_at(&contract, U256::ZERO), U256::ZERO);
        assert_eq!(state.get_account(&alice).nonce, 1);
    }

    let at_genesis: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"from": alice, "to": contract}), "0x0"],
        )
        .await
        .unwrap();
    assert_eq!(at_genesis, "0x", "the contract did not exist at genesis");
}

#[tokio::test]
async fn eth_call_reports_a_revert_as_an_error() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_THEN_REVERT_INITCODE, 0).await;

    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"from": alice, "to": contract}), "latest"],
        )
        .await;
    let error = result.expect_err("a reverting call is an error, not empty output");
    assert!(
        error.to_string().contains("execution reverted"),
        "got: {error}"
    );
}

#[tokio::test]
async fn eth_estimate_gas_is_enough_to_send_the_transaction() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_AND_LOG_INITCODE, 0).await;

    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "to": contract})],
        )
        .await
        .unwrap();
    let gas = from_quantity(&estimate) as u64;
    assert!(gas > 21_000);

    let raw = sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, gas).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    let tx_hash = tx_hash.parse().unwrap();
    harness
        .wait_for("the call to be mined", |c| {
            c.read().get_receipt(&tx_hash).is_some()
        })
        .await;

    let state = harness.chain.read();
    let receipt = state.get_receipt(&tx_hash).unwrap();
    assert_eq!(
        receipt.status, 1,
        "a transaction sent with the estimate succeeds"
    );
    assert!(receipt.gas_used <= gas);
    assert_eq!(
        state.get_storage_at(&contract, U256::ZERO),
        U256::from(42u64)
    );
}

#[tokio::test]
async fn an_unsupported_method_gets_the_standard_method_not_found_error() {
    let harness = Harness::start(&[], false).await;

    // Changed: eth_gasPrice is now implemented, so eth_getProof stands in as a real but unimplemented method.
    let result: Result<Value, _> = harness
        .client
        .request("eth_getProof", rpc_params![Address::ZERO, Vec::<String>::new(), "latest"])
        .await;
    let error = result.expect_err("an unregistered method must be an error");
    assert!(
        error.to_string().contains("Method not found") || error.to_string().contains("-32601"),
        "got: {error}"
    );
}

async fn send_and_wait(harness: &Harness, raw: Bytes) -> TransactionReceipt {
    let hash = harness.chain.submit_raw_transaction(&raw).unwrap();
    harness
        .wait_for("the transaction to be mined", |c| {
            c.read().get_receipt(&hash).is_some()
        })
        .await;
    harness.chain.read().get_receipt(&hash).unwrap().clone()
}

#[tokio::test]
async fn eth_gas_price_returns_the_fixed_price() {
    let harness = Harness::start(&[], false).await;
    let price: String = harness
        .client
        .request("eth_gasPrice", rpc_params![])
        .await
        .unwrap();
    assert_eq!(price, "0x3b9aca00", "1 gwei");
}

#[tokio::test]
async fn eth_get_code_returns_runtime_bytecode_and_0x_without_code() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_AND_LOG_INITCODE, 0).await;

    let code: String = harness
        .client
        .request("eth_getCode", rpc_params![contract, "latest"])
        .await
        .unwrap();
    assert_eq!(code, STORE_AND_LOG_RUNTIME);

    let eoa: String = harness
        .client
        .request("eth_getCode", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(eoa, "0x");

    let before_deployment: String = harness
        .client
        .request("eth_getCode", rpc_params![contract, "0x0"])
        .await
        .unwrap();
    assert_eq!(before_deployment, "0x");
}

#[tokio::test]
async fn eth_get_storage_at_returns_the_slot_as_a_32_byte_word() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_AND_LOG_INITCODE, 0).await;
    let deployed_at = harness.chain.read().get_block_number();

    let receipt = send_and_wait(
        &harness,
        sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, 100_000).unwrap(),
    )
    .await;
    assert_eq!(receipt.status, 1);

    let slot: String = harness
        .client
        .request("eth_getStorageAt", rpc_params![contract, "0x0", "latest"])
        .await
        .unwrap();
    assert_eq!(slot, format!("0x{:064x}", 42));
    assert_eq!(slot.len(), 66);

    let before_call: String = harness
        .client
        .request(
            "eth_getStorageAt",
            rpc_params![contract, "0x0", format!("{deployed_at:#x}")],
        )
        .await
        .unwrap();
    assert_eq!(before_call, format!("0x{}", "0".repeat(64)));
}

#[tokio::test]
async fn eth_get_logs_filters_by_block_range_and_address() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let contract = deploy(&harness, STORE_AND_LOG_INITCODE, 0).await;
    let receipt = send_and_wait(
        &harness,
        sign_call(DEV_PRIVATE_KEY, contract, Bytes::new(), 1, 100_000).unwrap(),
    )
    .await;
    let block = format!("{:#x}", receipt.block_number);
    let before = format!("{:#x}", receipt.block_number - 1);

    let found: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"fromBlock": block, "toBlock": block, "address": contract})],
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["address"], format!("{contract:#x}"));
    assert_eq!(found[0]["blockNumber"], block);
    assert_eq!(found[0]["transactionHash"], format!("{:#x}", receipt.transaction_hash));
    assert_eq!(found[0]["topics"].as_array().unwrap().len(), 0);

    let earlier: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"fromBlock": "0x0", "toBlock": before, "address": contract})],
        )
        .await
        .unwrap();
    assert!(earlier.is_empty());

    let other_address: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"fromBlock": "0x0", "toBlock": "latest", "address": alice})],
        )
        .await
        .unwrap();
    assert!(other_address.is_empty());

    let by_hash: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"blockHash": receipt.block_hash})],
        )
        .await
        .unwrap();
    assert_eq!(by_hash.len(), 1);
}

#[tokio::test]
async fn eth_get_logs_filters_by_topic_position() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], true).await;
    let two_topics = deploy(&harness, TWO_TOPIC_LOG_INITCODE, 0).await;
    let no_topics = deploy(&harness, STORE_AND_LOG_INITCODE, 1).await;
    for (nonce, to) in [(2, two_topics), (3, no_topics)] {
        let receipt = send_and_wait(
            &harness,
            sign_call(DEV_PRIVATE_KEY, to, Bytes::new(), nonce, 100_000).unwrap(),
        )
        .await;
        assert_eq!(receipt.status, 1);
    }

    let a = format!("0x{:064x}", 0xaa);
    let b = format!("0x{:064x}", 0xbb);
    let c = format!("0x{:064x}", 0xcc);
    let count = |topics: Value| {
        let client = harness.client.clone();
        async move {
            let logs: Vec<Value> = client
                .request(
                    "eth_getLogs",
                    rpc_params![serde_json::json!({"fromBlock": "earliest", "toBlock": "latest", "topics": topics})],
                )
                .await
                .unwrap();
            logs.len()
        }
    };

    assert_eq!(count(serde_json::json!([])).await, 2);
    assert_eq!(count(serde_json::json!([a])).await, 1);
    assert_eq!(count(serde_json::json!([b])).await, 0);
    assert_eq!(count(serde_json::json!([null, b])).await, 1);
    assert_eq!(count(serde_json::json!([[c, a]])).await, 1);
    assert_eq!(count(serde_json::json!([a, b, null])).await, 0);
}
