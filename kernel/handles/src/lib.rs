//! A process's handle table (SDD 20, f.3).
//!
//! A handle value is a slot index and a generation, XORed with a salt drawn for the
//! process when it is created. The generation changes when a slot is freed, so a
//! closed value names nothing; the salt makes values differ between processes, so a
//! value learned from another process names nothing here either (KK-I2, KK-I3). Value
//! zero is never issued.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use moruna_abi::{Error, RawHandle, limits};

struct Slot<T> {
    generation: u16,
    entry: Option<T>,
}

pub struct HandleTable<T> {
    slots: Vec<Slot<T>>,
    free: Vec<usize>,
    salt: u32,
    count: usize,
}

impl<T> HandleTable<T> {
    pub fn new(salt: u32) -> HandleTable<T> {
        HandleTable {
            slots: Vec::new(),
            free: Vec::new(),
            salt,
            count: 0,
        }
    }

    fn encode(&self, slot: usize, generation: u16) -> RawHandle {
        (((generation as u32) << 16) | (slot as u32 + 1)) ^ self.salt
    }

    fn decode(&self, value: RawHandle) -> Option<(usize, u16)> {
        let x = value ^ self.salt;
        let index = (x & 0xffff) as usize;
        if index == 0 {
            return None;
        }
        Some((index - 1, (x >> 16) as u16))
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many more handles the table takes.
    pub fn room(&self) -> usize {
        limits::HANDLES_PER_PROCESS - self.count
    }

    pub fn insert(&mut self, handle: T) -> Result<RawHandle, Error> {
        if self.count >= limits::HANDLES_PER_PROCESS {
            return Err(Error::Limit);
        }
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => {
                self.slots.try_reserve(1).map_err(|_| Error::NoMemory)?;
                self.slots.push(Slot {
                    generation: 0,
                    entry: None,
                });
                self.slots.len() - 1
            }
        };
        if self.encode(slot, self.slots[slot].generation) == 0 {
            self.slots[slot].generation = self.slots[slot].generation.wrapping_add(1);
        }
        self.slots[slot].entry = Some(handle);
        self.count += 1;
        Ok(self.encode(slot, self.slots[slot].generation))
    }

    pub fn get(&self, value: RawHandle) -> Result<&T, Error> {
        let (slot, generation) = self.decode(value).ok_or(Error::BadHandle)?;
        match self.slots.get(slot) {
            Some(Slot {
                generation: g,
                entry: Some(handle),
            }) if *g == generation => Ok(handle),
            _ => Err(Error::BadHandle),
        }
    }

    pub fn remove(&mut self, value: RawHandle) -> Result<T, Error> {
        self.get(value)?;
        let (slot, _) = self.decode(value).ok_or(Error::BadHandle)?;
        let entry = &mut self.slots[slot];
        let handle = entry.entry.take().ok_or(Error::BadHandle)?;
        entry.generation = entry.generation.wrapping_add(1);
        self.free.push(slot);
        self.count -= 1;
        Ok(handle)
    }

    /// Empties the table, returning every handle so the caller drops them outside any lock.
    pub fn drain(&mut self) -> Vec<T> {
        let out = self
            .slots
            .iter_mut()
            .filter_map(|s| {
                s.generation = s.generation.wrapping_add(1);
                s.entry.take()
            })
            .collect();
        self.free = (0..self.slots.len()).collect();
        self.count = 0;
        out
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeSet, vec::Vec};

    use super::*;

    #[test]
    fn kk_t5_values_name_only_live_entries() {
        let mut t: HandleTable<u32> = HandleTable::new(0x5a5a_1234);
        assert_eq!(t.get(0), Err(Error::BadHandle));
        let a = t.insert(10).expect("room");
        let b = t.insert(20).expect("room");
        assert_ne!(a, 0);
        assert_ne!(a, b);
        assert_eq!(t.get(a), Ok(&10));
        assert_eq!(t.get(b), Ok(&20));
        assert_eq!(t.len(), 2);
        assert_eq!(t.remove(a), Ok(10));
        assert_eq!(t.get(a), Err(Error::BadHandle));
        assert_eq!(t.remove(a), Err(Error::BadHandle));
        let c = t.insert(30).expect("room");
        assert_ne!(c, a, "a reused slot gets a new value");
        assert_eq!(t.get(a), Err(Error::BadHandle));
        for v in 0..=4096u32 {
            if v != b && v != c {
                assert_eq!(t.get(v), Err(Error::BadHandle), "value {v}");
            }
        }
    }

    #[test]
    fn kk_t5_values_differ_between_salts() {
        let mut one: HandleTable<u8> = HandleTable::new(0x1111_0000);
        let mut two: HandleTable<u8> = HandleTable::new(0x2222_0000);
        let a: Vec<u32> = (0..64).map(|_| one.insert(1).expect("room")).collect();
        for v in &a {
            assert_eq!(two.get(*v), Err(Error::BadHandle));
        }
        let b: Vec<u32> = (0..64).map(|_| two.insert(2).expect("room")).collect();
        let a: BTreeSet<u32> = a.into_iter().collect();
        assert!(b.iter().all(|v| !a.contains(v)));
    }

    #[test]
    fn kk_t5_zero_is_never_issued() {
        // With salt 1, slot 0 at generation 0 would encode to zero.
        let mut t: HandleTable<u8> = HandleTable::new(1);
        let v = t.insert(7).expect("room");
        assert_ne!(v, 0);
        assert_eq!(t.get(v), Ok(&7));
        assert_eq!(t.get(0), Err(Error::BadHandle));
    }

    #[test]
    fn kk_t5_bounded_and_drained() {
        let mut t: HandleTable<usize> = HandleTable::new(0xdead_beef);
        let mut values = Vec::new();
        for i in 0..limits::HANDLES_PER_PROCESS {
            values.push(t.insert(i).expect("room"));
        }
        assert_eq!(t.room(), 0);
        assert_eq!(t.insert(0), Err(Error::Limit));
        let set: BTreeSet<u32> = values.iter().copied().collect();
        assert_eq!(set.len(), values.len());
        let drained = t.drain();
        assert_eq!(drained.len(), limits::HANDLES_PER_PROCESS);
        assert!(t.is_empty());
        for v in &values {
            assert_eq!(t.get(*v), Err(Error::BadHandle));
        }
        let again = t.insert(1).expect("room after drain");
        assert!(!set.contains(&again));
    }
}
