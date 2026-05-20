//! Stable set of sibling graph canister principals (router-maintained ACL).

use candid::Principal;
use ic_stable_structures::{BTreeSet, Memory};

pub struct PeerGraphCanisterSet<M: Memory> {
    peers: BTreeSet<Principal, M>,
}

impl<M: Memory> PeerGraphCanisterSet<M> {
    pub fn init(memory: M) -> Self {
        Self {
            peers: BTreeSet::init(memory),
        }
    }

    pub fn contains(&self, principal: &Principal) -> bool {
        self.peers.contains(principal)
    }

    pub fn insert(&mut self, principal: Principal) {
        if principal == Principal::anonymous() {
            return;
        }
        self.peers.insert(principal);
    }

    pub fn insert_many(&mut self, peers: &[Principal], exclude: Principal) {
        for p in peers {
            if *p != exclude {
                self.insert(*p);
            }
        }
    }

    pub fn remove(&mut self, principal: &Principal) -> bool {
        self.peers.remove(principal)
    }

    pub fn len(&self) -> u64 {
        self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;

    #[test]
    fn set_membership_and_excludes_self() {
        let self_p = Principal::self_authenticating([1u8; 32]);
        let peer_a = Principal::self_authenticating([2u8; 32]);
        let peer_b = Principal::self_authenticating([3u8; 32]);

        let mut set = PeerGraphCanisterSet::init(DefaultMemoryImpl::default());
        set.insert_many(&[self_p, peer_a, peer_b], self_p);
        assert!(!set.contains(&self_p));
        assert!(set.contains(&peer_a));
        assert!(set.contains(&peer_b));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn remove_drops_membership() {
        let peer_a = Principal::self_authenticating([2u8; 32]);
        let peer_b = Principal::self_authenticating([3u8; 32]);

        let mut set = PeerGraphCanisterSet::init(DefaultMemoryImpl::default());
        set.insert(peer_a);
        set.insert(peer_b);
        assert!(set.remove(&peer_a));
        assert!(!set.contains(&peer_a));
        assert!(set.contains(&peer_b));
        assert!(!set.remove(&peer_a));
    }
}
