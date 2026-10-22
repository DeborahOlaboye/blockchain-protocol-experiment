use std::fmt::LowerHex;

use alloy::{
    consensus::EMPTY_OMMER_ROOT_HASH,
    primitives::{Address, B256, Bloom, Bytes, U256},
};
use serde::{Deserialize, Serialize};

use crate::{
    constants::EXTRA_DATA,
    node::{Block, Log, Transaction, TransactionReceipt},
};

/// Encodes a number as an Ethereum *quantity*: minimal hex, `0x0` for zero.
///
/// Works for every integer width the node uses, including `U256`, because they
/// all format the same way under `{:#x}`.
pub fn quantity(value: impl LowerHex) -> String {
    format!("{value:#x}")
}

/// Encodes a `u64` as a fixed-width 8-byte *data* field, which is how a block's
/// proof-of-work nonce appears on the wire.
pub fn data_u64(value: u64) -> String {
    format!("0x{value:016x}")
}

/// Encodes an address as lowercase hex, the form Ethereum nodes return.
pub fn data_address(address: &Address) -> String {
    format!("{address:#x}")
}

/// Encodes a 32-byte hash as lowercase hex.
pub fn data_hash(hash: &B256) -> String {
    format!("{hash:#x}")
}

pub fn data_bloom(bloom: &Bloom) -> String {
    format!("0x{}", alloy::hex::encode(bloom))
}

pub fn data_word(value: U256) -> String {
    format!("0x{value:064x}")
}

/// Encodes a byte string as lowercase hex, `0x` when empty.
pub fn data_bytes(bytes: &Bytes) -> String {
    format!("0x{}", alloy::hex::encode(bytes))
}

/// A block, as `eth_getBlockByNumber` and `eth_getBlockByHash` return it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockResponse {
    pub number: String,
    pub hash: String,
    pub parent_hash: String,
    pub sha3_uncles: String,
    pub mix_hash: String,
    pub state_root: String,
    #[serde(rename = "transactionsRoot")]
    pub transaction_root: String,
    pub receipts_root: String,
    pub logs_bloom: String,
    pub difficulty: String,
    pub total_difficulty: String,
    pub timestamp: String,
    pub gas_limit: String,
    pub gas_used: String,
    pub miner: String,
    pub extra_data: String,
    pub nonce: String,
    pub size: String,
    /// Transaction hashes, or full transaction objects when the caller asked for
    /// them with the `fullTransactions` flag.
    pub transactions: Vec<serde_json::Value>,
    pub uncles: Vec<String>,
}

/// A transaction, as `eth_getTransactionByHash` returns it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionResponse {
    pub hash: String,
    pub block_hash: String,
    pub block_number: String,
    pub transaction_index: String,
    pub chain_id: String,
    pub from: String,
    pub to: Option<String>,
    pub value: String,
    pub nonce: String,
    pub gas: String,
    pub gas_price: String,
    pub input: String,
    /// Always `0x0`: Breeja only accepts pre-EIP-1559 legacy transactions.
    #[serde(rename = "type")]
    pub tx_type: String,
    pub v: String,
    pub r: String,
    pub s: String,
}

/// A receipt, as `eth_getTransactionReceipt` returns it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceiptResponse {
    pub transaction_hash: String,
    pub transaction_index: String,
    pub block_hash: String,
    pub block_number: String,
    pub from: String,
    pub to: Option<String>,
    pub gas_used: String,
    #[serde(rename = "cumulativeGasUsed")]
    pub cummulative_gas_used: String,
    pub effective_gas_price: String,
    pub contract_address: Option<String>,
    pub logs: Vec<LogResponse>,
    pub logs_bloom: String,
    pub status: String,
    #[serde(rename = "type")]
    pub tx_type: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogResponse {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub block_number: String,
    pub block_hash: String,
    pub transaction_hash: String,
    pub transaction_index: String,
    pub log_index: String,
    pub removed: bool,
}

impl LogResponse {
    pub fn new(receipt: &TransactionReceipt, log: &Log) -> Self {
        Self {
            address: data_address(&log.address),
            topics: log.topics.iter().map(data_hash).collect(),
            data: data_bytes(&log.data),
            block_number: quantity(receipt.block_number),
            block_hash: data_hash(&receipt.block_hash),
            transaction_hash: data_hash(&receipt.transaction_hash),
            transaction_index: quantity(receipt.transaction_index),
            log_index: quantity(log.log_index),
            removed: false,
        }
    }
}

impl BlockResponse {
    /// Converts a stored block into its JSON-RPC form.
    ///
    /// `transactions` is supplied by the caller because `eth_getBlockBy*` takes a
    /// flag choosing between bare hashes and full transaction objects.
    pub fn from_block(block: &Block, transactions: Vec<serde_json::Value>) -> Self {
        Self {
            number: quantity(block.number),
            hash: data_hash(&block.hash),
            parent_hash: data_hash(&block.parent_hash),
            sha3_uncles: data_hash(&EMPTY_OMMER_ROOT_HASH),
            mix_hash: data_hash(&B256::ZERO),
            state_root: data_hash(&block.state_root),
            transaction_root: data_hash(&block.transaction_root),
            receipts_root: data_hash(&block.receipts_root),
            logs_bloom: data_bloom(&block.logs_bloom),
            difficulty: quantity(block.difficulty),
            total_difficulty: quantity(block.total_difficulty),
            timestamp: quantity(block.timestamp),
            gas_limit: quantity(block.gas_limit),
            gas_used: quantity(block.gas_used),
            miner: data_address(&block.miner),
            extra_data: data_bytes(&block.extra_data),
            nonce: data_u64(block.nonce),
            size: quantity(block_size(block)),
            transactions,
            uncles: Vec::new(),
        }
    }
}

impl From<&Transaction> for TransactionResponse {
    fn from(tx: &Transaction) -> Self {
        Self {
            hash: data_hash(&tx.hash),
            block_hash: data_hash(&tx.block_hash),
            block_number: quantity(tx.block_number),
            transaction_index: quantity(tx.transaction_index),
            chain_id: quantity(tx.chain_id),
            from: data_address(&tx.from),
            to: tx.to.as_ref().map(data_address),
            value: quantity(tx.value),
            nonce: quantity(tx.nonce),
            gas: quantity(tx.gas),
            gas_price: quantity(tx.gas_price),
            input: data_bytes(&tx.input),
            tx_type: "0x0".to_owned(),
            v: quantity(tx.v),
            r: quantity(tx.r),
            s: quantity(tx.s),
        }
    }
}

impl From<&TransactionReceipt> for TransactionReceiptResponse {
    fn from(receipt: &TransactionReceipt) -> Self {
        Self {
            transaction_hash: data_hash(&receipt.transaction_hash),
            transaction_index: quantity(receipt.transaction_index),
            block_hash: data_hash(&receipt.block_hash),
            block_number: quantity(receipt.block_number),
            from: data_address(&receipt.from),
            to: receipt.to.as_ref().map(data_address),
            gas_used: quantity(receipt.gas_used),
            cummulative_gas_used: quantity(receipt.cummulative_gas_used),
            effective_gas_price: quantity(receipt.effective_gas_price),
            contract_address: receipt.contract_address.as_ref().map(data_address),
            logs: receipt
                .logs
                .iter()
                .map(|log| LogResponse::new(receipt, log))
                .collect(),
            logs_bloom: data_bloom(&receipt.logs_bloom),
            status: quantity(receipt.status),
            tx_type: "0x0".to_owned(),
        }
    }
}

/// Approximates a block's serialized size in bytes.
///
/// A real node reports the length of the block's RLP encoding. Breeja estimates
/// it from the header plus its transactions, which is enough for a client that
/// only wants a rough figure.
fn block_size(block: &Block) -> u64 {
    const HEADER_BYTES: u64 = 180;
    const TX_BYTES: u64 = 110;
    HEADER_BYTES + EXTRA_DATA.len() as u64 + block.transactions.len() as u64 * TX_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_are_minimal_hex() {
        assert_eq!(quantity(0u64), "0x0");
        assert_eq!(quantity(1u64), "0x1");
        assert_eq!(quantity(31u64), "0x1f");
        assert_eq!(quantity(21_000u64), "0x5208");
    }

    #[test]
    fn data_fields_keep_their_full_width() {
        assert_eq!(data_u64(0), "0x0000000000000000");
        assert_eq!(data_hash(&B256::ZERO), format!("0x{}", "0".repeat(64)));
        assert_eq!(
            data_address(&Address::ZERO),
            format!("0x{}", "0".repeat(40))
        );
        assert_eq!(data_bytes(&Bytes::new()), "0x");
    }

    #[test]
    fn addresses_are_returned_lowercase_not_checksummed() {
        let address = crate::constants::DEV_ADDRESS;
        let encoded = data_address(&address);
        assert_eq!(encoded, encoded.to_lowercase());
        assert_eq!(encoded, "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    }
}
