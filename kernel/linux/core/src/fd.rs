//! File descriptors (SDD 28, e.2): the lowest free number, `O_CLOEXEC`, and `dup`'s
//! sharing of one open file (its offset and its flags) between numbers.

use alloc::{rc::Rc, vec::Vec};
use core::cell::RefCell;

/// One number's entry: the open file it shares, and whether `execve` closes it.
pub struct Entry<T> {
    pub file: Rc<RefCell<T>>,
    pub cloexec: bool,
}

impl<T> Clone for Entry<T> {
    fn clone(&self) -> Entry<T> {
        Entry {
            file: self.file.clone(),
            cloexec: self.cloexec,
        }
    }
}

/// A process's descriptors, at most `max` of them.
pub struct Fds<T> {
    slots: Vec<Option<Entry<T>>>,
    max: usize,
}

impl<T> Fds<T> {
    pub fn new(max: usize) -> Fds<T> {
        Fds {
            slots: Vec::new(),
            max,
        }
    }

    /// The lowest free number at or above `from`, or `None` when every number below
    /// `max` is taken.
    fn free(&self, from: usize) -> Option<usize> {
        (from..self.max).find(|&n| self.slots.get(n).is_none_or(|s| s.is_none()))
    }

    fn put(&mut self, n: usize, entry: Entry<T>) {
        if self.slots.len() <= n {
            self.slots.resize_with(n + 1, || None);
        }
        self.slots[n] = Some(entry);
    }

    /// A new number for `file` at or above `from`; `None` when none is free (`EMFILE`).
    pub fn open(&mut self, file: T, cloexec: bool, from: usize) -> Option<usize> {
        let n = self.free(from)?;
        self.put(
            n,
            Entry {
                file: Rc::new(RefCell::new(file)),
                cloexec,
            },
        );
        Some(n)
    }

    pub fn get(&self, n: u64) -> Option<&Entry<T>> {
        usize::try_from(n)
            .ok()
            .and_then(|n| self.slots.get(n))
            .and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, n: u64) -> Option<&mut Entry<T>> {
        usize::try_from(n)
            .ok()
            .and_then(|n| self.slots.get_mut(n))
            .and_then(Option::as_mut)
    }

    /// Closes `n`; the entry, whose file closes when its last number has gone.
    pub fn close(&mut self, n: u64) -> Option<Entry<T>> {
        usize::try_from(n)
            .ok()
            .and_then(|n| self.slots.get_mut(n))
            .and_then(Option::take)
    }

    /// `dup` and `F_DUPFD`: the lowest free number at or above `from` sharing `n`'s file.
    pub fn dup(&mut self, n: u64, from: usize, cloexec: bool) -> Result<usize, DupError> {
        let entry = self.get(n).ok_or(DupError::BadFd)?.clone();
        let m = self.free(from).ok_or(DupError::TooMany)?;
        self.put(
            m,
            Entry {
                file: entry.file,
                cloexec,
            },
        );
        Ok(m)
    }

    /// `dup2` and `dup3`: `new` shares `old`'s file, whatever `new` was (returned, to be
    /// closed).
    pub fn dup_to(
        &mut self,
        old: u64,
        new: u64,
        cloexec: bool,
    ) -> Result<Option<Entry<T>>, DupError> {
        let entry = self.get(old).ok_or(DupError::BadFd)?.clone();
        let new = usize::try_from(new)
            .ok()
            .filter(|n| *n < self.max)
            .ok_or(DupError::BadFd)?;
        let previous = self.close(new as u64);
        self.put(
            new,
            Entry {
                file: entry.file,
                cloexec,
            },
        );
        Ok(previous)
    }

    /// The numbers in use, lowest first.
    pub fn numbers(&self) -> Vec<usize> {
        (0..self.slots.len())
            .filter(|n| self.slots[*n].is_some())
            .collect()
    }

    /// Closes every number from `first` to `last`; the entries.
    pub fn close_range(&mut self, first: u64, last: u64) -> Vec<Entry<T>> {
        self.numbers()
            .into_iter()
            .filter(|n| (*n as u64) >= first && (*n as u64) <= last)
            .filter_map(|n| self.close(n as u64))
            .collect()
    }

    /// Closes the numbers marked `O_CLOEXEC`, as `execve` does.
    pub fn close_on_exec(&mut self) -> Vec<Entry<T>> {
        let marked: Vec<usize> = self
            .numbers()
            .into_iter()
            .filter(|n| self.slots[*n].as_ref().is_some_and(|e| e.cloexec))
            .collect();
        marked
            .into_iter()
            .filter_map(|n| self.close(n as u64))
            .collect()
    }
}

/// Why a `dup` failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupError {
    /// `EBADF`.
    BadFd,
    /// `EMFILE`.
    TooMany,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t5_lowest_free_and_sharing() {
        let mut f: Fds<u32> = Fds::new(8);
        assert_eq!(f.open(10, false, 0), Some(0));
        assert_eq!(f.open(11, false, 0), Some(1));
        assert_eq!(f.open(12, true, 0), Some(2));
        assert!(f.close(1).is_some());
        assert!(f.close(1).is_none());
        assert_eq!(f.open(13, false, 0), Some(1));
        assert_eq!(f.dup(0, 0, false), Ok(3));
        *f.get(3).unwrap().file.borrow_mut() = 99;
        assert_eq!(*f.get(0).unwrap().file.borrow(), 99);
        assert_eq!(f.dup(7, 0, false), Err(DupError::BadFd));
        assert_eq!(f.dup(0, 6, true), Ok(6));
        assert!(f.get(6).unwrap().cloexec);
        let old = f.dup_to(2, 1, false).unwrap();
        assert_eq!(*old.unwrap().file.borrow(), 13);
        assert_eq!(*f.get(1).unwrap().file.borrow(), 12);
        assert_eq!(f.dup_to(2, 8, false).err(), Some(DupError::BadFd));
        assert_eq!(f.dup_to(5, 4, false).err(), Some(DupError::BadFd));
        assert!(f.dup_to(2, 7, false).unwrap().is_none());
        assert_eq!(f.numbers(), alloc::vec![0, 1, 2, 3, 6, 7]);
        assert_eq!(f.close_on_exec().len(), 2);
        assert_eq!(f.numbers(), alloc::vec![0, 1, 3, 7]);
        assert_eq!(f.close_range(1, 3).len(), 2);
        assert_eq!(f.numbers(), alloc::vec![0, 7]);
        assert!(f.get_mut(7).is_some());
        assert!(f.get(u64::MAX).is_none());
        for n in 1..7 {
            assert!(f.open(0, false, 0).is_some(), "{n}");
        }
        assert_eq!(f.open(0, false, 0), None);
        assert_eq!(f.dup(0, 0, false), Err(DupError::TooMany));
    }
}
