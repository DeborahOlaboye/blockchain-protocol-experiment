use alloy::primitives::{Address, B256, Bytes, U256};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorObjectOwned, Params, error::ErrorCode},
};
use node::{
    evm::BreejaDb,
    executor::{BlockContext, TxOutcome, estimate_gas, simulate},
};
use primitives::{
    constants::CHAIN_ID,
    node::BlockState,
    params::{BlockTag, CallRequest, LogFilter},
    response::{
        BlockResponse, LogResponse, TransactionReceiptResponse, TransactionResponse, data_bytes,
        data_hash, data_word, quantity,
    },
};
use serde_json::Value;

use crate::RPCContext;

/// JSON-RPC error code Ethereum clients use for "your request was understood but
/// is not valid", as opposed to a malformed one.
const INVALID_INPUT: i32 = -32000;

/// Turns an internal error into a JSON-RPC error object the client can read.
fn invalid_input(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_INPUT, error.to_string(), None::<()>)
}

const EXECUTION_REVERTED: i32 = 3;

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::InvalidParams.code(),
        error.to_string(),
        None::<()>,
    )
}

/// Parses a 32-byte hash parameter.
fn parse_hash(raw: &str) -> Result<B256, ErrorObjectOwned> {
    raw.parse::<B256>()
        .map_err(|e| invalid_params(format!("invalid 32-byte hash {raw:?}: {e}")))
}

/// Resolves a block tag against the chain head, erroring on a block that does not
/// exist rather than silently substituting the latest one.
fn resolve_tag(state: &BlockState, tag: &BlockTag) -> Result<u64, ErrorObjectOwned> {
    tag.resolve(state.get_block_number()).map_err(invalid_input)
}


/// The chain's id, used by wallets for EIP-155 replay protection.
pub fn chain_id_handler() -> String {
    quantity(CHAIN_ID)
}


/// The number of the chain head. A freshly started node sits at `0x0`, the genesis
/// block, and climbs from there as the miner seals blocks.
pub fn block_number_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().get_block_number())
}


/// Renders a block, inlining full transaction objects if the caller asked for them.
fn render_block(state: &BlockState, number: u64, full_transactions: bool) -> Value {
    let Some(block) = state.get_block(number) else {
        return Value::Null;
    };

    let transactions: Vec<Value> = if full_transactions {
        block
            .transactions
            .iter()
            .filter_map(|hash| state.get_transaction(hash))
            .map(|tx| {
                serde_json::to_value(TransactionResponse::from(tx))
                    .expect("a transaction response always serializes")
            })
            .collect()
    } else {
        block
            .transactions
            .iter()
            .map(|hash| Value::String(data_hash(hash)))
            .collect()
    };

    serde_json::to_value(BlockResponse::from_block(block, transactions))
        .expect("a block response always serializes")
}

pub fn get_block_by_number_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    // Ethereum's optional trailing parameters have to be read one at a time: a
    // client may send just the block, or the block and the `fullTransactions`
    // flag, and both are valid.
    let mut sequence = params.sequence();
    let tag: BlockTag = sequence.next()?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_block_by_hash_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    let mut sequence = params.sequence();
    let hash = parse_hash(&sequence.next::<String>()?)?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    // An unknown hash is `null`, not an error: the caller asked a legitimate
    // question and the answer is "no such block".
    let Some(number) = state.get_block_by_hash(&hash).map(|block| block.number) else {
        return Ok(Value::Null);
    };
    Ok(render_block(&state, number, full_transactions))
}



pub fn get_transaction_by_hash_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_transaction(&hash) {
        Some(tx) => Ok(serde_json::to_value(TransactionResponse::from(tx))
            .expect("a transaction response always serializes")),
        None => Ok(Value::Null),
    }
}

/// A receipt exists only once a transaction has been mined. A transaction still
/// sitting in the mempool correctly returns `null` here, which is exactly how a
/// client polls for confirmation.
pub fn get_transaction_receipt_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_receipt(&hash) {
        Some(receipt) => Ok(
            serde_json::to_value(TransactionReceiptResponse::from(receipt))
                .expect("a receipt response always serializes"),
        ),
        None => Ok(Value::Null),
    }
}


pub fn get_balance_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let balance = match tag {
        // "pending" means "after everything in the mempool lands", which is what a
        // wallet wants before it builds the next transaction.
        BlockTag::Pending => state.get_pending_balance(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.balance)
                .unwrap_or(U256::ZERO)
        }
    };

    Ok(quantity(balance))
}

/// The account's nonce: how many transactions it has sent.
pub fn get_transaction_count_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let nonce = match tag {
        BlockTag::Pending => state.get_pending_nonce(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.nonce)
                .unwrap_or(0)
        }
    };

    Ok(quantity(nonce))
}


/// Accepts a signed transaction, validates it, and queues it for mining.
///
/// The response is the transaction hash, returned the moment the transaction is
/// *accepted* — not when it is mined. The client then polls
/// `eth_getTransactionReceipt` until a receipt appears, which is the standard
/// Ethereum confirmation flow.
pub fn send_raw_transaction_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let raw: String = params.one()?;
    let raw: Bytes = raw
        .parse()
        .map_err(|e| invalid_params(format!("raw transaction is not valid hex: {e}")))?;

    let hash = context
        .chain
        .submit_raw_transaction(&raw)
        .map_err(invalid_input)?;

    Ok(data_hash(&hash))
}


fn simulation_snapshot(
    context: &RPCContext,
    tag: Option<BlockTag>,
) -> Result<(BreejaDb, BlockContext), ErrorObjectOwned> {
    let state = context.chain.read();
    let number = resolve_tag(&state, &tag.unwrap_or(BlockTag::Latest))?;
    let parent = state
        .get_block(number)
        .ok_or_else(|| invalid_input(format!("block {number} does not exist")))?;
    let block = BlockContext::on_top_of(parent, parent.difficulty, context.chain.config().miner);
    let db = BreejaDb::snapshot_at(&state, number)
        .ok_or_else(|| invalid_input(format!("no state recorded for block {number}")))?;
    Ok((db, block))
}

fn execution_failed(outcome: &TxOutcome) -> ErrorObjectOwned {
    match outcome.halt_reason {
        Some(_) => invalid_input(outcome.failure()),
        None => ErrorObjectOwned::owned(
            EXECUTION_REVERTED,
            "execution reverted",
            Some(data_bytes(&outcome.output)),
        ),
    }
}

pub fn call_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;

    let (mut db, block) = simulation_snapshot(context, tag)?;
    let gas_limit = request
        .gas
        .map(|gas| gas.saturating_to::<u64>().min(block.gas_limit))
        .unwrap_or(block.gas_limit);

    let outcome = simulate(&mut db, &block, &request, gas_limit).map_err(invalid_input)?;
    if !outcome.success {
        return Err(execution_failed(&outcome));
    }
    Ok(data_bytes(&outcome.output))
}

pub fn estimate_gas_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;

    let (mut db, block) = simulation_snapshot(context, tag)?;
    let gas = estimate_gas(&mut db, &block, &request).map_err(invalid_input)?;
    Ok(quantity(gas))
}

pub fn gas_price_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().gas_price())
}

pub fn get_code_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag.unwrap_or(BlockTag::Latest))?;
    let code = state
        .get_code_at(&address, number)
        .ok_or_else(|| invalid_input(format!("no state recorded for block {number}")))?;
    Ok(data_bytes(&code))
}

pub fn get_storage_at_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let slot: U256 = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?;

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag.unwrap_or(BlockTag::Latest))?;
    let value = state
        .get_storage_at_block(&address, slot, number)
        .ok_or_else(|| invalid_input(format!("no state recorded for block {number}")))?;
    Ok(data_word(value))
}

pub fn get_logs_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    let filter: LogFilter = params.one()?;

    let state = context.chain.read();
    let (from, to) = match filter.block_hash {
        Some(hash) => {
            if filter.from_block.is_some() || filter.to_block.is_some() {
                return Err(invalid_params(
                    "blockHash cannot be combined with fromBlock or toBlock",
                ));
            }
            let number = state
                .get_block_by_hash(&hash)
                .map(|block| block.number)
                .ok_or_else(|| invalid_input(format!("unknown block {hash}")))?;
            (number, number)
        }
        None => {
            let from = resolve_tag(&state, &filter.from_block.unwrap_or(BlockTag::Latest))?;
            let to = resolve_tag(&state, &filter.to_block.unwrap_or(BlockTag::Latest))?;
            if from > to {
                return Err(invalid_input(format!(
                    "fromBlock {from} is after toBlock {to}"
                )));
            }
            (from, to)
        }
    };

    let mut logs = Vec::new();
    for number in from..=to {
        let Some(block) = state.get_block(number) else {
            continue;
        };
        for hash in &block.transactions {
            let Some(receipt) = state.get_receipt(hash) else {
                continue;
            };
            logs.extend(
                receipt
                    .logs
                    .iter()
                    .filter(|log| filter.matches(&log.address, &log.topics))
                    .map(|log| LogResponse::new(receipt, log)),
            );
        }
    }
    Ok(serde_json::to_value(logs).expect("log responses always serialize"))
}
