//! The `accountant` is a client of the `historian`. It uses the historian's
//! event log to record transactions. Its users can deposit funds and
//! transfer funds to other users.

use log::{hash, Entry, Sha256Hash};
use event::{Event, PublicKey, Signature};
use genesis::Genesis;
use historian::Historian;
use ring::signature::Ed25519KeyPair;
use std::sync::mpsc::SendError;
use std::collections::HashMap;
use std::result;

#[derive(Debug, PartialEq, Eq)]
pub enum AccountingError {
    InsufficientFunds,
    InvalidEvent,
    SendError,
}

pub type Result<T> = result::Result<T, AccountingError>;

pub struct Accountant {
    pub historian: Historian<u64>,
    pub balances: HashMap<PublicKey, u64>,
    pub end_hash: Sha256Hash,
}

impl Accountant {
    pub fn new(gen: &Genesis, ms_per_tick: Option<u64>) -> Self {
        let start_hash = hash(&gen.pkcs8);
        let hist = Historian::<u64>::new(&start_hash, ms_per_tick);
        let mut acc = Accountant {
            historian: hist,
            balances: HashMap::new(),
            end_hash: start_hash,
        };
        for (i, event) in gen.create_events().into_iter().enumerate() {
            acc.process_verified_event(event, i < 2).unwrap();
        }
        acc
    }

    pub fn sync(self: &mut Self) -> Vec<Entry<u64>> {
        let mut entries = vec![];
        while let Ok(entry) = self.historian.receiver.try_recv() {
            entries.push(entry);
        }

        if let Some(last_entry) = entries.last() {
            self.end_hash = last_entry.end_hash;
        }

        entries
    }

    pub fn deposit(self: &mut Self, n: u64, keypair: &Ed25519KeyPair) -> Result<Signature> {
        use event::{get_pubkey, sign_claim_data};
        let to = get_pubkey(keypair);
        let sig = sign_claim_data(&n, keypair);
        let event = Event::new_claim(to, n, sig);
        if !self.historian.verify_event(&event) {
            return Err(AccountingError::InvalidEvent);
        }
        self.process_verified_event(event, true).map(|_| sig)
    }

    fn is_deposit(allow_deposits: bool, from: &PublicKey, to: &PublicKey) -> bool {
        allow_deposits && from == to
    }

    pub fn process_event(self: &mut Self, event: Event<u64>) -> Result<()> {
        if !self.historian.verify_event(&event) {
            return Err(AccountingError::InvalidEvent);
        }
        self.process_verified_event(event, false)
    }

    fn process_verified_event(
        self: &mut Self,
        event: Event<u64>,
        allow_deposits: bool,
    ) -> Result<()> {
        match event {
            Event::Tick => Ok(()),
            Event::Transaction { from, to, data, .. } => {
                if !Self::is_deposit(allow_deposits, &from, &to) {
                    if self.get_balance(&from).unwrap_or(0) < data {
                        return Err(AccountingError::InsufficientFunds);
                    }
                }

                if let Err(SendError(_)) = self.historian.sender.send(event) {
                    return Err(AccountingError::SendError);
                }

                if !Self::is_deposit(allow_deposits, &from, &to) {
                    if let Some(x) = self.balances.get_mut(&from) {
                        *x -= data;
                    }
                }

                if self.balances.contains_key(&to) {
                    if let Some(x) = self.balances.get_mut(&to) {
                        *x += data;
                    }
                } else {
                    self.balances.insert(to, data);
                }
                Ok(())
            }
        }
    }

    pub fn transfer(
        self: &mut Self,
        n: u64,
        keypair: &Ed25519KeyPair,
        to: PublicKey,
    ) -> Result<Signature> {
        use event::{get_pubkey, sign_transaction_data};
        let from = get_pubkey(keypair);
        let sig = sign_transaction_data(&n, keypair, &to);
        let event = Event::Transaction {
            from,
            to,
            data: n,
            sig,
        };
        self.process_event(event).map(|_| sig)
    }

    pub fn get_balance(self: &Self, pubkey: &PublicKey) -> Option<u64> {
        self.balances.get(pubkey).map(|x| *x)
    }

    pub fn wait_on_signature(self: &mut Self, wait_sig: &Signature) {
        use std::thread::sleep;
        use std::time::Duration;
        let mut entries = self.sync();
        let mut found = false;
        while !found {
            found = entries.iter().any(|e| match e.event {
                Event::Transaction { sig, .. } => sig == *wait_sig,
                _ => false,
            });
            if !found {
                sleep(Duration::from_millis(30));
                entries = self.sync();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event::{generate_keypair, get_pubkey};
    use logger::ExitReason;
    use genesis::Creator;

    #[test]
    fn test_accountant() {
        let bob = Creator::new(1_000);
        let bob_pubkey = bob.pubkey;
        let alice = Genesis::new(10_000, vec![bob]);
        let mut acc = Accountant::new(&alice, Some(2));

        let sig = acc.transfer(500, &alice.get_keypair(), bob_pubkey).unwrap();
        acc.wait_on_signature(&sig);

        assert_eq!(acc.get_balance(&bob_pubkey).unwrap(), 1_500);

        drop(acc.historian.sender);
        assert_eq!(
            acc.historian.thread_hdl.join().unwrap().1,
            ExitReason::RecvDisconnected
        );
    }

    #[test]
    fn test_invalid_transfer() {
        use std::thread::sleep;
        use std::time::Duration;
        let bob = Creator::new(1_000);
        let bob_pubkey = bob.pubkey;
        let alice = Genesis::new(11_000, vec![bob]);
        let mut acc = Accountant::new(&alice, Some(2));

        assert_eq!(
            acc.transfer(10_001, &alice.get_keypair(), bob_pubkey),
            Err(AccountingError::InsufficientFunds)
        );
        sleep(Duration::from_millis(30));

        let alice_pubkey = get_pubkey(&alice.get_keypair());
        assert_eq!(acc.get_balance(&alice_pubkey).unwrap(), 10_000);
        assert_eq!(acc.get_balance(&bob_pubkey).unwrap(), 1_000);

        drop(acc.historian.sender);
        assert_eq!(
            acc.historian.thread_hdl.join().unwrap().1,
            ExitReason::RecvDisconnected
        );
    }

    #[test]
    fn test_transfer_to_newb() {
        let alice = Genesis::new(10_000, vec![]);
        let mut acc = Accountant::new(&alice, Some(2));
        let alice_keypair = alice.get_keypair();
        let bob_keypair = generate_keypair();
        let sig = acc.deposit(10_000, &alice_keypair).unwrap();
        acc.wait_on_signature(&sig);

        let bob_pubkey = get_pubkey(&bob_keypair);
        let sig = acc.transfer(500, &alice_keypair, bob_pubkey).unwrap();
        acc.wait_on_signature(&sig);
        assert_eq!(acc.get_balance(&bob_pubkey).unwrap(), 500);

        drop(acc.historian.sender);
        assert_eq!(
            acc.historian.thread_hdl.join().unwrap().1,
            ExitReason::RecvDisconnected
        );
    }
}