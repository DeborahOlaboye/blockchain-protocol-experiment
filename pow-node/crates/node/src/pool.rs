use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction as TxTrait, TxEnvelope, TxType};
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use primitives::{
    constants::{
        CHAIN_ID, GAS_PRICE, INITCODE_WORD_GAS, TRANSFER_GAS, TX_CREATE_GAS, TX_DATA_NON_ZERO_GAS,
        TX_DATA_ZERO_GAS,
    },
    node::{BlockState, PendingTransaction},
};

/// A signed transaction that has been decoded and checked against the protocol
/// rules, but not yet against chain state.
struct DecodedTransaction {
    hash: B256,
    from: Address,
    to: Option<Address>,
    value: U256,
    nonce: u64,
    gas: u64,
    gas_price: u128,
    input: Bytes,
    chain_id: u64,
    v: u64,
    r: U256,
    s: U256,
}

/// Decodes a raw signed transaction and applies every rule that can be checked
/// without looking at chain state.
fn decode_and_check(raw: &Bytes) -> Result<DecodedTransaction, anyhow::Error> {
    if raw.is_empty() {
        anyhow::bail!("raw transaction is empty");
    }

    let envelope = TxEnvelope::decode_2718(&mut raw.as_ref())
        .map_err(|e| anyhow::anyhow!("could not RLP-decode the raw transaction: {e}"))?;

    // Breeja predates typed transactions, so it only understands legacy ones.
    // There is no base fee here for an EIP-1559 transaction to bid against.
    if !matches!(envelope.tx_type(), TxType::Legacy) {
        anyhow::bail!(
            "unsupported transaction type {:?}: this node only accepts legacy \
             (pre-EIP-1559) transactions, so sign with --legacy",
            envelope.tx_type()
        );
    }

    // The sender is derived from the signature, never declared.
    let from = envelope
        .recover_signer()
        .map_err(|e| anyhow::anyhow!("invalid signature, could not recover sender: {e}"))?;

    // EIP-155 replay protection: a transaction signed for another chain must not
    // be mineable here. A legacy transaction with no chain id at all predates
    // EIP-155 and is accepted, exactly as on Ethereum.
    let chain_id = match envelope.chain_id() {
        Some(id) if id == CHAIN_ID => id,
        Some(id) => {
            anyhow::bail!("transaction is signed for chain {id}, but this chain is {CHAIN_ID}")
        }
        None => CHAIN_ID,
    };

    let to = match envelope.kind() {
        TxKind::Call(to) => Some(to),
        TxKind::Create => None,
    };
    let input = envelope.input().clone();

    let gas = envelope.gas_limit();
    let intrinsic = intrinsic_gas(&input, to.is_none());
    if gas < intrinsic {
        anyhow::bail!("gas limit {gas} is below the intrinsic cost of this transaction ({intrinsic})");
    }

    let gas_price = envelope.max_fee_per_gas();
    if gas_price < GAS_PRICE {
        anyhow::bail!("gas price {gas_price} is below this chain's fixed price of {GAS_PRICE}");
    }

    let signature = envelope.signature();
    Ok(DecodedTransaction {
        hash: *envelope.tx_hash(),
        from,
        to,
        value: envelope.value(),
        nonce: envelope.nonce(),
        gas,
        gas_price,
        input,
        chain_id,
        v: legacy_v(signature.v(), envelope.chain_id()),
        r: signature.r(),
        s: signature.s(),
    })
}

pub fn intrinsic_gas(input: &[u8], is_create: bool) -> u64 {
    let zero_bytes = input.iter().filter(|byte| **byte == 0).count() as u64;
    let non_zero_bytes = input.len() as u64 - zero_bytes;
    let mut gas =
        TRANSFER_GAS + zero_bytes * TX_DATA_ZERO_GAS + non_zero_bytes * TX_DATA_NON_ZERO_GAS;
    if is_create {
        gas += TX_CREATE_GAS + (input.len() as u64).div_ceil(32) * INITCODE_WORD_GAS;
    }
    gas
}

/// Reconstructs the `v` value a legacy transaction reports over JSON-RPC.
///
/// `v` encodes the signature's y-parity, and since EIP-155 it also encodes the
/// chain id, which is how replay protection is folded into the signature itself.
fn legacy_v(y_parity: bool, chain_id: Option<u64>) -> u64 {
    match chain_id {
        Some(id) => y_parity as u64 + 35 + id * 2,
        None => y_parity as u64 + 27,
    }
}

/// Validates a raw transaction against the current chain state and admits it to
/// the mempool, returning its hash.
///
/// The state checks use *pending* values — the nonce and balance including
/// everything already queued — so that a client can fire several transactions
/// back to back without waiting for a block in between.
pub fn admit(state: &mut BlockState, raw: &Bytes) -> Result<B256, anyhow::Error> {
    let tx = decode_and_check(raw)?;

    if state.get_transaction(&tx.hash).is_some() {
        anyhow::bail!("transaction {} has already been mined", tx.hash);
    }
    if state.mempool_contains(&tx.hash) {
        anyhow::bail!("transaction {} is already pending", tx.hash);
    }

    let expected_nonce = state.get_pending_nonce(&tx.from);
    if tx.nonce != expected_nonce {
        anyhow::bail!(
            "nonce too {}: {} expects nonce {expected_nonce}, transaction has {}",
            if tx.nonce < expected_nonce {
                "low"
            } else {
                "high"
            },
            tx.from,
            tx.nonce
        );
    }

    let max_cost = tx.value + U256::from(tx.gas) * U256::from(tx.gas_price);
    let available = state.get_pending_balance(&tx.from);
    if available < max_cost {
        anyhow::bail!(
            "insufficient funds: {} has {available} available but the transaction \
             needs up to {max_cost} (value plus gas)",
            tx.from
        );
    }

    state.add_to_mempool(PendingTransaction {
        hash: tx.hash,
        from: tx.from,
        to: tx.to,
        value: tx.value,
        nonce: tx.nonce,
        gas: tx.gas,
        gas_price: tx.gas_price,
        input: tx.input,
        chain_id: tx.chain_id,
        v: tx.v,
        r: tx.r,
        s: tx.s,
    });

    Ok(tx.hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_v_encodes_parity_and_chain_id() {
        // Pre-EIP-155: 27 or 28.
        assert_eq!(legacy_v(false, None), 27);
        assert_eq!(legacy_v(true, None), 28);
        // EIP-155 on chain 1: 37 or 38.
        assert_eq!(legacy_v(false, Some(1)), 37);
        assert_eq!(legacy_v(true, Some(1)), 38);
    }

    #[test]
    fn intrinsic_gas_matches_revm_under_shanghai() {
        use revm::{
            context_interface::cfg::gas::calculate_initial_tx_gas,
            primitives::hardfork::SpecId,
        };

        let initcode: &[u8] = &[
            0x60, 0x0b, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x0b, 0x60, 0x00, 0xf3, 0x60, 0x2a,
            0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xa0, 0x00,
        ];
        let cases: [(&[u8], bool); 5] = [
            (&[], false),
            (&[0x00, 0x00, 0x01], false),
            (&[0xde, 0xad, 0xbe, 0xef], false),
            (initcode, true),
            (&[0u8; 33], true),
        ];
        for (input, is_create) in cases {
            let revm = calculate_initial_tx_gas(SpecId::SHANGHAI, input, is_create, 0, 0, 0, None);
            assert_eq!(
                intrinsic_gas(input, is_create),
                revm.initial_regular_gas,
                "input {input:?}, create {is_create}"
            );
        }
        assert_eq!(intrinsic_gas(&[], false), TRANSFER_GAS);
    }

    #[test]
    fn garbage_bytes_are_rejected_not_panicked_on() {
        assert!(decode_and_check(&Bytes::new()).is_err());
        assert!(decode_and_check(&Bytes::from_static(&[0xff, 0x00, 0x13])).is_err());
    }
}
