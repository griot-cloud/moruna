//! The arena (SDD 29 f.1): the memory objects the engine's batches and windows live in,
//! within a budget, and the pool a window is granted from.
//!
//! Every byte a task reads or writes on the plane is a page of one of these objects. On the
//! kernel an object is made once and used again: a window that comes back as a record
//! becomes a batch, and a batch with no holder left goes back to the pool as the next
//! window, so a run's steady state makes no memory at all. The kernel gives 4 KiB pages:
//! OSTD 0.19 maps a user page table at its last level only, so large pages are not the
//! kernel's to give yet (SDD 29 m.1). On Linux an object a task has held is never given to
//! another (the task's descriptor of it may outlive the task), so it is retired instead.
//!
//! The budget counts what is in use, pooled or not: when no pooled object fits and the
//! budget has no room left, pooled objects are given up until it has, so memory waiting in
//! the pool never refuses a window it could have made. `Budget { asked, free }` is then the
//! same on every adapter: `free` is the budget less the objects held by batches and windows.

use alloc::{collections::BTreeMap, vec::Vec};

use crate::port::PlaneError;

/// One page.
pub const PAGE: u64 = moruna_abi::PAGE_SIZE;

/// `bytes` rounded up to whole pages; `None` past `u64`.
pub fn pages(bytes: u64) -> Option<u64> {
    bytes.checked_add(PAGE - 1).map(|b| b & !(PAGE - 1))
}

/// An object of the arena: its handle while the engine holds it (none while a slot does),
/// and its size.
struct Object<M> {
    handle: Option<M>,
    bytes: u64,
}

pub struct Arena<M> {
    budget: u64,
    made: u64,
    objects: BTreeMap<u64, Object<M>>,
    /// Objects free for the next window, by id.
    pool: Vec<u64>,
    next: u64,
}

impl<M> Arena<M> {
    pub fn new(budget: u64) -> Arena<M> {
        Arena {
            budget,
            made: 0,
            objects: BTreeMap::new(),
            pool: Vec::new(),
            next: 1,
        }
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    pub fn made(&self) -> u64 {
        self.made
    }

    /// Bytes of the objects in the pool.
    pub fn pooled(&self) -> u64 {
        self.pool
            .iter()
            .filter_map(|id| self.objects.get(id))
            .map(|o| o.bytes)
            .sum()
    }

    /// An object of at least `bytes` (whole pages) and its id, its handle taken out: the
    /// smallest pooled one that fits, or a new one from `make` while the budget allows,
    /// pooled objects too small for it given up first when the budget has no room.
    pub fn take(
        &mut self,
        bytes: u64,
        make: impl FnOnce(u64) -> Result<M, moruna_abi::Error>,
    ) -> Result<(u64, M, u64), PlaneError> {
        let size = pages(bytes)
            .filter(|s| *s > 0)
            .ok_or(PlaneError::Limit("a window of no bytes, or past u64"))?;
        let best = self
            .pool
            .iter()
            .enumerate()
            .filter_map(|(i, id)| self.objects.get(id).map(|o| (i, *id, o.bytes)))
            .filter(|(_, _, b)| *b >= size)
            .min_by_key(|(_, _, b)| *b);
        if let Some((i, id, have)) = best {
            self.pool.swap_remove(i);
            let handle = self
                .objects
                .get_mut(&id)
                .and_then(|o| o.handle.take())
                .ok_or(PlaneError::Unknown("a pooled object with no handle"))?;
            return Ok((id, handle, have));
        }
        self.reclaim(size);
        let free = self.budget.saturating_sub(self.made);
        if size > free {
            return Err(PlaneError::Budget { asked: size, free });
        }
        let handle = make(size).map_err(PlaneError::Kernel)?;
        let id = self.next;
        self.next += 1;
        self.made += size;
        self.objects.insert(
            id,
            Object {
                handle: None,
                bytes: size,
            },
        );
        Ok((id, handle, size))
    }

    /// The object `id`'s handle is the engine's again (a record or a window came back).
    pub fn hold(&mut self, id: u64, handle: M) -> Result<(), PlaneError> {
        let object = self
            .objects
            .get_mut(&id)
            .ok_or(PlaneError::Unknown("an object the arena did not make"))?;
        if object.handle.is_some() {
            return Err(PlaneError::Unknown("an object the arena already holds"));
        }
        object.handle = Some(handle);
        Ok(())
    }

    /// The handle of object `id`, which the engine holds.
    pub fn handle(&self, id: u64) -> Option<&M> {
        self.objects.get(&id).and_then(|o| o.handle.as_ref())
    }

    /// The size of object `id`.
    pub fn bytes(&self, id: u64) -> u64 {
        self.objects.get(&id).map_or(0, |o| o.bytes)
    }

    /// Object `id`, which the engine holds, goes back to the pool.
    pub fn free(&mut self, id: u64) -> Result<(), PlaneError> {
        match self.objects.get(&id) {
            Some(o) if o.handle.is_some() => {
                if !self.pool.contains(&id) {
                    self.pool.push(id);
                }
                Ok(())
            }
            Some(_) => Err(PlaneError::Unknown("an object a slot still holds")),
            None => Err(PlaneError::Unknown("an object the arena did not make")),
        }
    }

    /// Pooled objects, the largest first, are given up (their handles dropped, which closes
    /// them) until `size` fits the budget or the pool is empty.
    fn reclaim(&mut self, size: u64) {
        while self.budget.saturating_sub(self.made) < size {
            let largest = self
                .pool
                .iter()
                .enumerate()
                .max_by_key(|(_, id)| self.objects.get(id).map_or(0, |o| o.bytes))
                .map(|(i, id)| (i, *id));
            let Some((i, id)) = largest else {
                return;
            };
            self.pool.swap_remove(i);
            if let Some(object) = self.objects.remove(&id) {
                self.made -= object.bytes;
            }
        }
    }

    /// Object `id`, which the engine holds, leaves the arena: its bytes are the budget's
    /// again and its handle comes back to be closed. For an object no task may be given
    /// again (on Linux, one a task held).
    pub fn retire(&mut self, id: u64) -> Result<M, PlaneError> {
        match self.objects.get(&id) {
            Some(o) if o.handle.is_some() => {}
            Some(_) => return Err(PlaneError::Unknown("an object a slot still holds")),
            None => return Err(PlaneError::Unknown("an object the arena did not make")),
        }
        self.pool.retain(|p| *p != id);
        let object = self
            .objects
            .remove(&id)
            .ok_or(PlaneError::Unknown("an object the arena did not make"))?;
        self.made -= object.bytes;
        object
            .handle
            .ok_or(PlaneError::Unknown("an object a slot still holds"))
    }

    /// A handle `take` handed out that was never pushed goes back as it was.
    pub fn untake(&mut self, id: u64, handle: M) -> Result<(), PlaneError> {
        self.hold(id, handle)?;
        self.free(id)
    }
}
