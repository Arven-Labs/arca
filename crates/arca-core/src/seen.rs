//! Cache negativo do `pull` (ADR 0018, item 2b): ids de manifests de outra época já
//! classificados, para não baixar de novo, a cada rodada, o histórico estrangeiro (até 64 MiB
//! por manifest). Só em memória e com teto: passou dele, sai o mais antigo.

use crate::object::ObjectId;
use std::collections::{HashSet, VecDeque};

/// Teto de ids lembrados.
pub(crate) const MAX_SEEN: usize = 4096;

#[derive(Debug)]
pub(crate) struct Seen {
    cap: usize,
    order: VecDeque<ObjectId>,
    set: HashSet<ObjectId>,
}

impl Seen {
    pub(crate) fn new(cap: usize) -> Seen {
        Seen {
            cap,
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    pub(crate) fn contains(&self, _id: &ObjectId) -> bool {
        false
    }

    pub(crate) fn insert(&mut self, _id: ObjectId) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectKind;

    fn id(n: u8) -> ObjectId {
        ObjectId {
            kind: ObjectKind::Manifest,
            hash: [n; 32],
        }
    }

    #[test]
    fn what_was_inserted_is_remembered_and_the_rest_is_not() {
        let mut seen = Seen::new(3);
        seen.insert(id(1));
        assert!(seen.contains(&id(1)));
        assert!(!seen.contains(&id(2)));
    }

    #[test]
    fn inserting_the_same_id_twice_does_not_use_two_slots() {
        let mut seen = Seen::new(2);
        seen.insert(id(1));
        seen.insert(id(1));
        seen.insert(id(2));
        assert!(seen.contains(&id(1)) && seen.contains(&id(2)));
    }

    #[test]
    fn past_the_cap_the_oldest_is_forgotten_and_the_newest_stay() {
        let mut seen = Seen::new(3);
        for n in 1..=4 {
            seen.insert(id(n));
        }
        assert!(!seen.contains(&id(1)));
        assert!(seen.contains(&id(2)) && seen.contains(&id(3)) && seen.contains(&id(4)));
    }
}
