use std::{collections::HashMap, convert::Infallible};

use alloy::primitives::{Address, B256, Bytes, U256};
use primitives::node::{Account, BlockState};
use revm::{
    Database, DatabaseCommit,
    bytecode::Bytecode,
    primitives::{AddressMap, KECCAK_EMPTY, StorageKey, StorageValue},
    state::{Account as EvmAccount, AccountInfo},
};

const BLOCK_HASH_WINDOW: usize = 256;

#[derive(Debug, Clone, Default)]
pub struct BreejaDb {
    pub accounts: HashMap<Address, Account>,
    pub code_by_hash: HashMap<B256, Bytes>,
    pub block_hashes: HashMap<u64, B256>,
}

impl BreejaDb {
    pub fn snapshot(state: &BlockState) -> Self {
        Self {
            accounts: state.accounts_snapshot(),
            code_by_hash: state.code_snapshot(),
            block_hashes: state.recent_block_hashes(BLOCK_HASH_WINDOW),
        }
    }

    fn code_for(&self, code_hash: B256) -> Bytecode {
        if code_hash == B256::ZERO || code_hash == KECCAK_EMPTY {
            return Bytecode::default();
        }
        self.code_by_hash
            .get(&code_hash)
            .map(|bytes| Bytecode::new_legacy(bytes.clone()))
            .unwrap_or_default()
    }
}

impl Database for BreejaDb {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(account) = self.accounts.get(&address) else {
            return Ok(None);
        };
        let code = self.code_for(account.code_hash);
        Ok(Some(AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.code_for(code_hash))
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Ok(self
            .accounts
            .get(&address)
            .and_then(|account| account.storage.get(&index).copied())
            .unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.block_hashes.get(&number).copied().unwrap_or_default())
    }
}

impl DatabaseCommit for BreejaDb {
    fn commit(&mut self, changes: AddressMap<EvmAccount>) {
        for (address, account) in changes {
            if !account.is_touched() {
                continue;
            }
            if account.is_selfdestructed() {
                self.accounts.remove(&address);
                continue;
            }

            let entry = self.accounts.entry(address).or_default();

            if account.is_created() {
                entry.storage.clear();
            }

            entry.balance = account.info.balance;
            entry.nonce = account.info.nonce;

            if let Some(code) = &account.info.code
                && !code.is_empty()
            {
                self.code_by_hash
                    .entry(account.info.code_hash)
                    .or_insert_with(|| code.original_bytes());
                entry.code_hash = account.info.code_hash;
            }

            for (slot, value) in account.storage {
                let present = value.present_value();
                if present == U256::ZERO {
                    entry.storage.remove(&slot);
                } else {
                    entry.storage.insert(slot, present);
                }
            }

            if entry.nonce == 0
                && entry.balance == U256::ZERO
                && entry.code_hash == B256::ZERO
                && entry.storage.is_empty()
            {
                self.accounts.remove(&address);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use revm::state::EvmStorageSlot;

    use super::*;

    fn funded_db(address: Address, balance: U256, nonce: u64) -> BreejaDb {
        let mut db = BreejaDb::default();
        db.accounts.insert(
            address,
            Account {
                nonce,
                ..Account::new(balance)
            },
        );
        db
    }

    #[test]
    fn basic_returns_the_balance_and_nonce_of_a_funded_account() {
        let address = Address::repeat_byte(0x11);
        let mut db = funded_db(address, U256::from(1_000u64), 7);

        let info = db.basic(address).unwrap().expect("a funded account exists");
        assert_eq!(info.balance, U256::from(1_000u64));
        assert_eq!(info.nonce, 7);
        assert_eq!(info.code_hash, KECCAK_EMPTY);
    }

    #[test]
    fn basic_returns_none_for_an_address_that_was_never_funded() {
        let mut db = funded_db(Address::repeat_byte(0x11), U256::from(1u64), 0);
        assert!(db.basic(Address::repeat_byte(0x22)).unwrap().is_none());
    }

    #[test]
    fn commit_persists_balance_nonce_code_and_storage() {
        let address = Address::repeat_byte(0x11);
        let mut db = funded_db(address, U256::from(1_000u64), 0);

        let code = Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x55]));
        let code_hash = code.hash_slow();
        let mut changed = EvmAccount::from(AccountInfo {
            balance: U256::from(400u64),
            nonce: 1,
            code_hash,
            code: Some(code),
            ..Default::default()
        });
        changed.storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(42u64), Default::default()),
        );
        changed.mark_touch();

        let mut changes = AddressMap::default();
        changes.insert(address, changed);
        db.commit(changes);

        let account = &db.accounts[&address];
        assert_eq!(account.balance, U256::from(400u64));
        assert_eq!(account.nonce, 1);
        assert_eq!(account.code_hash, code_hash);
        assert_eq!(account.storage[&U256::ZERO], U256::from(42u64));
        assert_eq!(
            db.code_by_hash[&code_hash],
            Bytes::from_static(&[0x60, 0x2a, 0x60, 0x00, 0x55])
        );

        let info = db.basic(address).unwrap().unwrap();
        assert_eq!(info.code_hash, code_hash);
        assert_eq!(db.storage(address, U256::ZERO).unwrap(), U256::from(42u64));
    }

    #[test]
    fn commit_ignores_accounts_revm_only_read() {
        let address = Address::repeat_byte(0x11);
        let mut db = funded_db(address, U256::from(1_000u64), 0);

        let read_only = EvmAccount::from(AccountInfo {
            balance: U256::from(5u64),
            ..Default::default()
        });
        let mut changes = AddressMap::default();
        changes.insert(address, read_only);
        db.commit(changes);

        assert_eq!(db.accounts[&address].balance, U256::from(1_000u64));
    }

    #[test]
    fn writing_a_slot_back_to_zero_removes_it() {
        let address = Address::repeat_byte(0x11);
        let mut db = funded_db(address, U256::from(1u64), 1);
        db.accounts
            .get_mut(&address)
            .unwrap()
            .storage
            .insert(U256::ZERO, U256::from(42u64));

        let mut changed = EvmAccount::from(AccountInfo {
            balance: U256::from(1u64),
            nonce: 1,
            ..Default::default()
        });
        changed.storage.insert(
            U256::ZERO,
            EvmStorageSlot::new_changed(U256::from(42u64), U256::ZERO, Default::default()),
        );
        changed.mark_touch();
        let mut changes = AddressMap::default();
        changes.insert(address, changed);
        db.commit(changes);

        assert!(db.accounts[&address].storage.is_empty());
    }
}
