//! The `bank` module tracks client balances, and the progress of pending
//! transactions. It offers a high-level public API that signs transactions
//! on behalf of the caller, and a private low-level API for when they have
//! already been signed and verified.

extern crate libc;

use chrono::prelude::*;
use entry::Entry;
use hash::Hash;
use mint::Mint;
use payment_plan::{Payment, PaymentPlan, Witness};
use rayon::prelude::*;
use signature::{KeyPair, PublicKey, Signature};
use std::collections::hash_map::Entry::Occupied;
use std::collections::{HashMap, HashSet, VecDeque};
use std::result;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::RwLock;
use transaction::{Instruction, Plan, Transaction};

pub const MAX_ENTRY_IDS: usize = 1024 * 4;

#[derive(Debug, PartialEq, Eq)]
pub enum BankError {
    AccountNotFound(PublicKey),
    InsufficientFunds(PublicKey),
    DuplicateSiganture(Signature),
    LastIdNotFound(Hash),
    NegativeTokens,
}

pub type Result<T> = result::Result<T, BankError>;

pub struct Bank {
    balances: RwLock<HashMap<PublicKey, AtomicIsize>>,
    pending: RwLock<HashMap<Signature, Plan>>,
    last_ids: RwLock<VecDeque<(Hash, RwLock<HashSet<Signature>>)>>,
    time_sources: RwLock<HashSet<PublicKey>>,
    last_time: RwLock<DateTime<Utc>>,
    transaction_count: AtomicUsize,
}

impl Bank {
    /// Create an Bank using a deposit.
    pub fn new_from_deposit(deposit: &Payment) -> Self {
        let bank = Bank {
            balances: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            last_ids: RwLock::new(VecDeque::new()),
            time_sources: RwLock::new(HashSet::new()),
            last_time: RwLock::new(Utc.timestamp(0, 0)),
            transaction_count: AtomicUsize::new(0),
        };
        bank.apply_payment(deposit);
        bank
    }

    /// Create an Bank with only a Mint. Typically used by unit tests.
    pub fn new(mint: &Mint) -> Self {
        let deposit = Payment {
            to: mint.pubkey(),
            tokens: mint.tokens,
        };
        let bank = Self::new_from_deposit(&deposit);
        bank.register_entry_id(&mint.last_id());
        bank
    }

    /// Commit funds to the 'to' party.
    fn apply_payment(&self, payment: &Payment) {
        // First we check balances with a read lock to maximize potential parallelization.
        if self.balances
            .read()
            .expect("'balances' read lock in apply_payment")
            .contains_key(&payment.to)
        {
            let bals = self.balances.read().expect("'balances' read lock");
            bals[&payment.to].fetch_add(payment.tokens as isize, Ordering::Relaxed);
        } else {
            // Now we know the key wasn't present a nanosecond ago, but it might be there
            // by the time we aquire a write lock, so we'll have to check again.
            let mut bals = self.balances.write().expect("'balances' write lock");
            if bals.contains_key(&payment.to) {
                bals[&payment.to].fetch_add(payment.tokens as isize, Ordering::Relaxed);
            } else {
                bals.insert(payment.to, AtomicIsize::new(payment.tokens as isize));
            }
        }
    }

    /// Return the last entry ID registered
    pub fn last_id(&self) -> Hash {
        let last_ids = self.last_ids.read().expect("'last_ids' read lock");
        let last_item = last_ids.iter().last().expect("empty 'last_ids' list");
        last_item.0
    }

    fn reserve_signature(signatures: &RwLock<HashSet<Signature>>, sig: &Signature) -> Result<()> {
        if signatures
            .read()
            .expect("'signatures' read lock")
            .contains(sig)
        {
            return Err(BankError::DuplicateSiganture(*sig));
        }
        signatures
            .write()
            .expect("'signatures' write lock")
            .insert(*sig);
        Ok(())
    }

    fn forget_signature(signatures: &RwLock<HashSet<Signature>>, sig: &Signature) {
        signatures
            .write()
            .expect("'signatures' write lock in forget_signature")
            .remove(sig);
    }

    fn forget_signature_with_last_id(&self, sig: &Signature, last_id: &Hash) {
        if let Some(entry) = self.last_ids
            .read()
            .expect("'last_ids' read lock in forget_signature_with_last_id")
            .iter()
            .rev()
            .find(|x| x.0 == *last_id)
        {
            Self::forget_signature(&entry.1, sig);
        }
    }

    fn reserve_signature_with_last_id(&self, sig: &Signature, last_id: &Hash) -> Result<()> {
        if let Some(entry) = self.last_ids
            .read()
            .expect("'last_ids' read lock in reserve_signature_with_last_id")
            .iter()
            .rev()
            .find(|x| x.0 == *last_id)
        {
            return Self::reserve_signature(&entry.1, sig);
        }
        Err(BankError::LastIdNotFound(*last_id))
    }

    /// Tell the bank which Entry IDs exist on the ledger. This function
    /// assumes subsequent calls correspond to later entries, and will boot
    /// the oldest ones once its internal cache is full. Once boot, the
    /// bank will reject transactions using that `last_id`.
    pub fn register_entry_id(&self, last_id: &Hash) {
        let mut last_ids = self.last_ids
            .write()
            .expect("'last_ids' write lock in register_entry_id");
        if last_ids.len() >= MAX_ENTRY_IDS {
            last_ids.pop_front();
        }
        last_ids.push_back((*last_id, RwLock::new(HashSet::new())));
    }

    /// Deduct tokens from the 'from' address the account has sufficient
    /// funds and isn't a duplicate.
    fn apply_debits(&self, tx: &Transaction) -> Result<()> {
        if let Instruction::NewContract(contract) = &tx.instruction {
            trace!("Transaction {}", contract.tokens);
            if contract.tokens < 0 {
                return Err(BankError::NegativeTokens);
            }
        }
        let bals = self.balances
            .read()
            .expect("'balances' read lock in apply_debits");
        let option = bals.get(&tx.from);

        if option.is_none() {
            return Err(BankError::AccountNotFound(tx.from));
        }

        self.reserve_signature_with_last_id(&tx.sig, &tx.last_id)?;

        loop {
            let result = if let Instruction::NewContract(contract) = &tx.instruction {
                let bal = option.expect("assignment of option to bal");
                let current = bal.load(Ordering::Relaxed) as i64;

                if current < contract.tokens {
                    self.forget_signature_with_last_id(&tx.sig, &tx.last_id);
                    return Err(BankError::InsufficientFunds(tx.from));
                }

                bal.compare_exchange(
                    current as isize,
                    (current - contract.tokens) as isize,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
            } else {
                Ok(0)
            };

            match result {
                Ok(_) => {
                    self.transaction_count.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(_) => continue,
            };
        }
    }

    fn apply_credits(&self, tx: &Transaction) {
        match &tx.instruction {
            Instruction::NewContract(contract) => {
                let mut plan = contract.plan.clone();
                plan.apply_witness(&Witness::Timestamp(*self.last_time
                    .read()
                    .expect("timestamp creation in apply_credits")));

                if let Some(payment) = plan.final_payment() {
                    self.apply_payment(&payment);
                } else {
                    let mut pending = self.pending
                        .write()
                        .expect("'pending' write lock in apply_credits");
                    pending.insert(tx.sig, plan);
                }
            }
            Instruction::ApplyTimestamp(dt) => {
                let _ = self.apply_timestamp(tx.from, *dt);
            }
            Instruction::ApplySignature(tx_sig) => {
                let _ = self.apply_signature(tx.from, *tx_sig);
            }
        }
    }

    /// Process a Transaction.
    fn process_transaction(&self, tx: &Transaction) -> Result<()> {
        self.apply_debits(tx)?;
        self.apply_credits(tx);
        Ok(())
    }

    /// Process a batch of transactions.
    pub fn process_transactions(&self, txs: Vec<Transaction>) -> Vec<Result<Transaction>> {
        // Run all debits first to filter out any transactions that can't be processed
        // in parallel deterministically.
        info!("processing Transactions {}", txs.len());
        let results: Vec<_> = txs.into_par_iter()
            .map(|tx| self.apply_debits(&tx).map(|_| tx))
            .collect(); // Calling collect() here forces all debits to complete before moving on.

        results
            .into_par_iter()
            .map(|result| {
                result.map(|tx| {
                    self.apply_credits(&tx);
                    tx
                })
            })
            .collect()
    }

    pub fn process_entries<I>(&self, entries: I) -> Result<()>
    where
        I: IntoIterator<Item = Entry>,
    {
        for entry in entries {
            for result in self.process_transactions(entry.transactions) {
                result?;
            }
            self.register_entry_id(&entry.id);
        }
        Ok(())
    }

    /// Process a Witness Signature.
    fn apply_signature(&self, from: PublicKey, tx_sig: Signature) -> Result<()> {
        if let Occupied(mut e) = self.pending
            .write()
            .expect("write() in apply_signature")
            .entry(tx_sig)
        {
            e.get_mut().apply_witness(&Witness::Signature(from));
            if let Some(payment) = e.get().final_payment() {
                self.apply_payment(&payment);
                e.remove_entry();
            }
        };

        Ok(())
    }

    /// Process a Witness Timestamp.
    fn apply_timestamp(&self, from: PublicKey, dt: DateTime<Utc>) -> Result<()> {
        // If this is the first timestamp we've seen, it probably came from the genesis block,
        // so we'll trust it.
        if *self.last_time
            .read()
            .expect("'last_time' read lock on first timestamp check")
            == Utc.timestamp(0, 0)
        {
            self.time_sources
                .write()
                .expect("'time_sources' write lock on first timestamp")
                .insert(from);
        }

        if self.time_sources
            .read()
            .expect("'time_sources' read lock")
            .contains(&from)
        {
            if dt > *self.last_time.read().expect("'last_time' read lock") {
                *self.last_time.write().expect("'last_time' write lock") = dt;
            }
        } else {
            return Ok(());
        }

        // Check to see if any timelocked transactions can be completed.
        let mut completed = vec![];

        // Hold 'pending' write lock until the end of this function. Otherwise another thread can
        // double-spend if it enters before the modified plan is removed from 'pending'.
        let mut pending = self.pending
            .write()
            .expect("'pending' write lock in apply_timestamp");
        for (key, plan) in pending.iter_mut() {
            plan.apply_witness(&Witness::Timestamp(*self.last_time
                .read()
                .expect("'last_time' read lock when creating timestamp")));
            if let Some(payment) = plan.final_payment() {
                self.apply_payment(&payment);
                completed.push(key.clone());
            }
        }

        for key in completed {
            pending.remove(&key);
        }

        Ok(())
    }

    /// Create, sign, and process a Transaction from `keypair` to `to` of
    /// `n` tokens where `last_id` is the last Entry ID observed by the client.
    pub fn transfer(
        &self,
        n: i64,
        keypair: &KeyPair,
        to: PublicKey,
        last_id: Hash,
    ) -> Result<Signature> {
        let tx = Transaction::new(keypair, to, n, last_id);
        let sig = tx.sig;
        self.process_transaction(&tx).map(|_| sig)
    }

    /// Create, sign, and process a postdated Transaction from `keypair`
    /// to `to` of `n` tokens on `dt` where `last_id` is the last Entry ID
    /// observed by the client.
    pub fn transfer_on_date(
        &self,
        n: i64,
        keypair: &KeyPair,
        to: PublicKey,
        dt: DateTime<Utc>,
        last_id: Hash,
    ) -> Result<Signature> {
        let tx = Transaction::new_on_date(keypair, to, dt, n, last_id);
        let sig = tx.sig;
        self.process_transaction(&tx).map(|_| sig)
    }

    pub fn get_balance(&self, pubkey: &PublicKey) -> Option<i64> {
        let bals = self.balances
            .read()
            .expect("'balances' read lock in get_balance");
        bals.get(pubkey).map(|x| x.load(Ordering::Relaxed) as i64)
    }

    pub fn transaction_count(&self) -> usize {
        self.transaction_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::serialize;
    use hash::hash;
    use signature::KeyPairUtil;

    #[test]
    fn test_bank() {
        let mint = Mint::new(10_000);
        let pubkey = KeyPair::new().pubkey();
        let bank = Bank::new(&mint);
        assert_eq!(bank.last_id(), mint.last_id());

        bank.transfer(1_000, &mint.keypair(), pubkey, mint.last_id())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey).unwrap(), 1_000);

        bank.transfer(500, &mint.keypair(), pubkey, mint.last_id())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey).unwrap(), 1_500);
        assert_eq!(bank.transaction_count(), 2);
    }

    #[test]
    fn test_invalid_tokens() {
        let mint = Mint::new(1);
        let pubkey = KeyPair::new().pubkey();
        let bank = Bank::new(&mint);
        assert_eq!(
            bank.transfer(-1, &mint.keypair(), pubkey, mint.last_id()),
            Err(BankError::NegativeTokens)
        );
        assert_eq!(bank.transaction_count(), 0);
    }

    #[test]
    fn test_account_not_found() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let keypair = KeyPair::new();
        assert_eq!(
            bank.transfer(1, &keypair, mint.pubkey(), mint.last_id()),
            Err(BankError::AccountNotFound(keypair.pubkey()))
        );
        assert_eq!(bank.transaction_count(), 0);
    }

    #[test]
    fn test_invalid_transfer() {
        let mint = Mint::new(11_000);
        let bank = Bank::new(&mint);
        let pubkey = KeyPair::new().pubkey();
        bank.transfer(1_000, &mint.keypair(), pubkey, mint.last_id())
            .unwrap();
        assert_eq!(bank.transaction_count(), 1);
        assert_eq!(
            bank.transfer(10_001, &mint.keypair(), pubkey, mint.last_id()),
            Err(BankError::InsufficientFunds(mint.pubkey()))
        );
        assert_eq!(bank.transaction_count(), 1);

        let mint_pubkey = mint.keypair().pubkey();
        assert_eq!(bank.get_balance(&mint_pubkey).unwrap(), 10_000);
        assert_eq!(bank.get_balance(&pubkey).unwrap(), 1_000);
    }

    #[test]
    fn test_transfer_to_newb() {
        let mint = Mint::new(10_000);
        let bank = Bank::new(&mint);
        let pubkey = KeyPair::new().pubkey();
        bank.transfer(500, &mint.keypair(), pubkey, mint.last_id())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey).unwrap(), 500);
    }

    #[test]
    fn test_transfer_on_date() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let pubkey = KeyPair::new().pubkey();
        let dt = Utc::now();
        bank.transfer_on_date(1, &mint.keypair(), pubkey, dt, mint.last_id())
            .unwrap();

        // Mint's balance will be zero because all funds are locked up.
        assert_eq!(bank.get_balance(&mint.pubkey()), Some(0));

        // tx count is 1, because debits were applied.
        assert_eq!(bank.transaction_count(), 1);

        // pubkey's balance will be None because the funds have not been
        // sent.
        assert_eq!(bank.get_balance(&pubkey), None);

        // Now, acknowledge the time in the condition occurred and
        // that pubkey's funds are now available.
        bank.apply_timestamp(mint.pubkey(), dt).unwrap();
        assert_eq!(bank.get_balance(&pubkey), Some(1));

        // tx count is still 1, because we chose not to count timestamp transactions
        // tx count.
        assert_eq!(bank.transaction_count(), 1);

        bank.apply_timestamp(mint.pubkey(), dt).unwrap(); // <-- Attack! Attempt to process completed transaction.
        assert_ne!(bank.get_balance(&pubkey), Some(2));
    }

    #[test]
    fn test_transfer_after_date() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let pubkey = KeyPair::new().pubkey();
        let dt = Utc::now();
        bank.apply_timestamp(mint.pubkey(), dt).unwrap();

        // It's now past now, so this transfer should be processed immediately.
        bank.transfer_on_date(1, &mint.keypair(), pubkey, dt, mint.last_id())
            .unwrap();

        assert_eq!(bank.get_balance(&mint.pubkey()), Some(0));
        assert_eq!(bank.get_balance(&pubkey), Some(1));
    }

    #[test]
    fn test_cancel_transfer() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let pubkey = KeyPair::new().pubkey();
        let dt = Utc::now();
        let sig = bank.transfer_on_date(1, &mint.keypair(), pubkey, dt, mint.last_id())
            .unwrap();

        // Assert the debit counts as a transaction.
        assert_eq!(bank.transaction_count(), 1);

        // Mint's balance will be zero because all funds are locked up.
        assert_eq!(bank.get_balance(&mint.pubkey()), Some(0));

        // pubkey's balance will be None because the funds have not been
        // sent.
        assert_eq!(bank.get_balance(&pubkey), None);

        // Now, cancel the trancaction. Mint gets her funds back, pubkey never sees them.
        bank.apply_signature(mint.pubkey(), sig).unwrap();
        assert_eq!(bank.get_balance(&mint.pubkey()), Some(1));
        assert_eq!(bank.get_balance(&pubkey), None);

        // Assert cancel doesn't cause count to go backward.
        assert_eq!(bank.transaction_count(), 1);

        bank.apply_signature(mint.pubkey(), sig).unwrap(); // <-- Attack! Attempt to cancel completed transaction.
        assert_ne!(bank.get_balance(&mint.pubkey()), Some(2));
    }

    #[test]
    fn test_duplicate_transaction_signature() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let sig = Signature::default();
        assert!(
            bank.reserve_signature_with_last_id(&sig, &mint.last_id())
                .is_ok()
        );
        assert_eq!(
            bank.reserve_signature_with_last_id(&sig, &mint.last_id()),
            Err(BankError::DuplicateSiganture(sig))
        );
    }

    #[test]
    fn test_forget_signature() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let sig = Signature::default();
        bank.reserve_signature_with_last_id(&sig, &mint.last_id())
            .unwrap();
        bank.forget_signature_with_last_id(&sig, &mint.last_id());
        assert!(
            bank.reserve_signature_with_last_id(&sig, &mint.last_id())
                .is_ok()
        );
    }

    #[test]
    fn test_max_entry_ids() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);
        let sig = Signature::default();
        for i in 0..MAX_ENTRY_IDS {
            let last_id = hash(&serialize(&i).unwrap()); // Unique hash
            bank.register_entry_id(&last_id);
        }
        // Assert we're no longer able to use the oldest entry ID.
        assert_eq!(
            bank.reserve_signature_with_last_id(&sig, &mint.last_id()),
            Err(BankError::LastIdNotFound(mint.last_id()))
        );
    }

    #[test]
    fn test_debits_before_credits() {
        let mint = Mint::new(2);
        let bank = Bank::new(&mint);
        let keypair = KeyPair::new();
        let tx0 = Transaction::new(&mint.keypair(), keypair.pubkey(), 2, mint.last_id());
        let tx1 = Transaction::new(&keypair, mint.pubkey(), 1, mint.last_id());
        let txs = vec![tx0, tx1];
        let results = bank.process_transactions(txs);
        assert!(results[1].is_err());

        // Assert bad transactions aren't counted.
        assert_eq!(bank.transaction_count(), 1);
    }
}

#[cfg(all(feature = "unstable", test))]
mod bench {
    extern crate test;
    use self::test::Bencher;
    use bank::*;
    use bincode::serialize;
    use hash::hash;
    use signature::KeyPairUtil;

    #[bench]
    fn bench_process_transaction(bencher: &mut Bencher) {
        let mint = Mint::new(100_000_000);
        let bank = Bank::new(&mint);
        // Create transactions between unrelated parties.
        let transactions: Vec<_> = (0..4096)
            .into_par_iter()
            .map(|i| {
                // Seed the 'from' account.
                let rando0 = KeyPair::new();
                let tx = Transaction::new(&mint.keypair(), rando0.pubkey(), 1_000, mint.last_id());
                bank.process_transaction(&tx).unwrap();

                // Seed the 'to' account and a cell for its signature.
                let last_id = hash(&serialize(&i).unwrap()); // Unique hash
                bank.register_entry_id(&last_id);

                let rando1 = KeyPair::new();
                let tx = Transaction::new(&rando0, rando1.pubkey(), 1, last_id);
                bank.process_transaction(&tx).unwrap();

                // Finally, return a transaction that's unique
                Transaction::new(&rando0, rando1.pubkey(), 1, last_id)
            })
            .collect();
        bencher.iter(|| {
            // Since benchmarker runs this multiple times, we need to clear the signatures.
            for sigs in bank.last_ids.read().unwrap().iter() {
                sigs.1.write().unwrap().clear();
            }

            assert!(
                bank.process_transactions(transactions.clone())
                    .iter()
                    .all(|x| x.is_ok())
            );
        });
    }
}
