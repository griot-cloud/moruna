//! Generic receipt-backed namespace over the ONE existing raw Volume. No RAM scratch,
//! host filesystem, second disk journal/device owner, or application record format.
//! Data IO awaits the actual shared-owner Volume ticket state machines. The owner actor
//! polls only on real channel/timer wakeups; no blocking Device call is queued as async.
use crate::{memory::Loan, volume::Volume};
use alloc::{
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::cell::{Cell, RefCell};
pub use format::Limits;
use moruna_storage_format::{
    Metadata, Object, Scope,
    auth::{Action, Authorization, Verifier},
    namespace::{self as format, Entry, Inode, Kind, Page, Root, Value},
    wire::{Condition, Outcome, Refusal},
};
use moruna_svc::{Handle, Memory, PrivateMemory, Rights};
struct Held {
    root: Object,
    poisoned: Rc<Cell<bool>>,
    open_inodes: Rc<RefCell<Vec<u64>>>,
}
/// One registry shared by all namespace handlers attached to the SAME physical owner.
/// A lease is acquired by constructing a real handler, never by a caller-supplied id.
pub struct Registry {
    owner: Rc<RefCell<Volume>>,
    held: Vec<Held>,
    limit: usize,
    active: Rc<Cell<usize>>,
    frozen: Rc<Cell<bool>>,
}
impl Registry {
    /// Explicit bounded concurrent namespace ownership.
    pub fn new(owner: Rc<RefCell<Volume>>, limit: usize) -> Result<Rc<RefCell<Self>>, Refusal> {
        if limit == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(Rc::new(RefCell::new(Self {
            owner,
            held: Vec::new(),
            limit,
            active: Rc::new(Cell::new(0)),
            frozen: Rc::new(Cell::new(false)),
        })))
    }
    fn acquire(registry: &Rc<RefCell<Self>>, root: &Object) -> Result<Lease, Refusal> {
        let mut state = registry
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        if state.frozen.get() {
            return Err(Refusal::Unavailable);
        }
        if state.held.iter().any(|held| held.root == *root) {
            return Err(Refusal::Conflict);
        }
        if state.held.len() >= state.limit {
            return Err(Refusal::Capacity);
        }
        let poisoned = Rc::new(Cell::new(false));
        let open_inodes = Rc::new(RefCell::new(Vec::new()));
        state.held.push(Held {
            root: root.clone(),
            poisoned: poisoned.clone(),
            open_inodes: open_inodes.clone(),
        });
        Ok(Lease {
            registry: registry.clone(),
            root: root.clone(),
            poisoned,
            open_inodes,
        })
    }
}
struct Lease {
    registry: Rc<RefCell<Registry>>,
    root: Object,
    poisoned: Rc<Cell<bool>>,
    open_inodes: Rc<RefCell<Vec<u64>>>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        // Borrow contention is another healthy actor, not evidence that this lease's
        // originals are unknown. The actor retains its own active task through drain.
        let unhealthy = self
            .registry
            .try_borrow()
            .ok()
            .and_then(|registry| {
                registry
                    .owner
                    .try_borrow()
                    .ok()
                    .map(|volume| !volume.admission_open())
            })
            .unwrap_or(false);
        if unhealthy {
            self.poisoned.set(true);
        }
        if !self.poisoned.get() {
            if let Ok(mut registry) = self.registry.try_borrow_mut() {
                registry.held.retain(|held| held.root != self.root);
            } else {
                self.poisoned.set(true);
            }
        }
    }
}
struct Open {
    handle: u64,
    inode: u64,
    writable: bool,
}
/// Actual connection-owned namespace session. The dispatcher must retain this object
/// through every issued original operation and drop it only after their real completions.
pub struct Namespace {
    volume: Rc<RefCell<Volume>>,
    root: Object,
    scope: Scope,
    account: Rc<moruna_sys::Account>,
    limits: Limits,
    metadata: Metadata,
    index: Root,
    open: Vec<Open>,
    next_handle: u64,
    _lease: Lease,
    _metadata_budget: Memory,
}
struct Editor<'a> {
    volume: &'a mut Volume,
    root: &'a Object,
    account: &'a Handle,
    limits: Limits,
    remaining: u32,
}
impl Editor<'_> {
    fn work(&mut self) -> Result<(), Refusal> {
        self.remaining = self.remaining.checked_sub(1).ok_or(Refusal::Capacity)?;
        Ok(())
    }
    fn internal(&self, metadata: &Metadata) -> Result<(), Refusal> {
        if metadata.object.workspace != self.root.workspace
            || metadata.object.bucket != self.root.bucket
            || !metadata
                .object
                .key
                .starts_with(&format!("{}/objects/", self.root.key))
        {
            return Err(Refusal::Integrity);
        }
        Ok(())
    }
    async fn bytes(&mut self, metadata: &Metadata) -> Result<PrivateMemory, Refusal> {
        self.work()?;
        if metadata.object != *self.root {
            self.internal(metadata)?;
        }
        if metadata.bytes > self.limits.metadata_bytes as u64 {
            return Err(Refusal::Capacity);
        }
        let memory = Memory::create_in(metadata.bytes.max(1), self.account)
            .map_err(|_| Refusal::Capacity)?;
        let loan = Loan::new(memory, 0, metadata.bytes, true).map_err(|_| Refusal::Unavailable)?;
        match self
            .volume
            .read_async(
                &metadata.object,
                &metadata.version,
                0,
                metadata.bytes,
                Some(&loan),
                self.account,
            )
            .await?
        {
            Outcome::Read { length, .. } if length == metadata.bytes => {}
            _ => return Err(Refusal::Integrity),
        }
        let mut private = PrivateMemory::create_in(metadata.bytes.max(1), self.account)
            .map_err(|_| Refusal::Capacity)?;
        private
            .with_bytes(|bytes| {
                loan.read(0, &mut bytes[..metadata.bytes as usize])
                    .map_err(|_| Refusal::Unavailable)
            })
            .map_err(|_| Refusal::Unavailable)??;
        Ok(private)
    }
    async fn page(
        &mut self,
        metadata: &Metadata,
        kind: Kind,
        level: Option<u8>,
        maximum: Option<&str>,
    ) -> Result<Page, Refusal> {
        self.internal(metadata)?;
        let mut bytes = self.bytes(metadata).await?;
        let page = bytes
            .with_bytes(|bytes| {
                Page::decode(&bytes[..metadata.bytes as usize]).map_err(|_| Refusal::Integrity)
            })
            .map_err(|_| Refusal::Unavailable)??;
        if page.kind != kind
            || level.is_some_and(|level| page.level != level)
            || maximum.is_some_and(|maximum| page.maximum().ok() != Some(maximum))
        {
            return Err(Refusal::Integrity);
        }
        Ok(page)
    }
    fn object(&self) -> Result<Object, Refusal> {
        let mut random = [0; 16];
        moruna_svc::random(&mut random).map_err(|_| Refusal::Unavailable)?;
        Ok(Object {
            workspace: self.root.workspace.clone(),
            bucket: self.root.bucket.clone(),
            key: format!(
                "{}/objects/{}",
                self.root.key,
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
        })
    }
    async fn store(&mut self, private: PrivateMemory, length: u64) -> Result<Metadata, Refusal> {
        self.work()?;
        let object = self.object()?;
        let source =
            Loan::new(private.into_memory(), 0, length, false).map_err(|_| Refusal::Unavailable)?;
        self.volume
            .put_async(
                &object,
                &Condition::Absent,
                Some(source),
                length,
                self.account,
            )
            .await
    }
    async fn store_page(&mut self, page: &Page) -> Result<Metadata, Refusal> {
        let mut private = PrivateMemory::create_in(self.limits.metadata_bytes as u64, self.account)
            .map_err(|_| Refusal::Capacity)?;
        let length = private
            .with_bytes(|bytes| page.encode_into(bytes).map_err(|_| Refusal::Capacity))
            .map_err(|_| Refusal::Unavailable)??;
        self.store(private, length as u64).await
    }
    async fn get(
        &mut self,
        tree: Option<&Metadata>,
        kind: Kind,
        key: &str,
    ) -> Result<Option<Value>, Refusal> {
        let Some(mut metadata) = tree.cloned() else {
            return Ok(None);
        };
        let mut expected = None;
        let mut maximum = None::<String>;
        loop {
            let page = self
                .page(&metadata, kind, expected, maximum.as_deref())
                .await?;
            if page.level == 0 {
                return Ok(page
                    .entries
                    .into_iter()
                    .find(|entry| entry.key == key)
                    .map(|entry| entry.value));
            }
            let Some(entry) = page.entries.iter().find(|entry| entry.key.as_str() >= key) else {
                return Ok(None);
            };
            let Value::Child(child) = &entry.value else {
                return Err(Refusal::Integrity);
            };
            metadata = child.clone();
            expected = Some(page.level - 1);
            maximum = Some(entry.key.clone());
        }
    }
    async fn edit(
        &mut self,
        tree: Option<&Metadata>,
        kind: Kind,
        key: &str,
        value: Option<Value>,
    ) -> Result<Option<Metadata>, Refusal> {
        let (level, children) = match tree {
            Some(tree) => {
                let page = self.page(tree, kind, None, None).await?;
                let level = page.level;
                let children = alloc::boxed::Box::pin(self.edit_page(page, key, value)).await?;
                (level, children)
            }
            None => match value {
                None => return Ok(None),
                Some(value) => (
                    0,
                    vec![Page {
                        kind,
                        level: 0,
                        entries: vec![Entry {
                            key: key.to_string(),
                            value,
                        }],
                    }],
                ),
            },
        };
        if children.is_empty() {
            return Ok(None);
        }
        let mut entries = Vec::new();
        for page in children {
            let key = page.maximum().map_err(|_| Refusal::Integrity)?.to_string();
            let metadata = self.store_page(&page).await?;
            entries.push(Entry {
                key,
                value: Value::Child(metadata),
            });
        }
        if entries.len() == 1 {
            let Value::Child(metadata) = entries.remove(0).value else {
                return Err(Refusal::Integrity);
            };
            return Ok(Some(metadata));
        }
        let level = level
            .checked_add(1)
            .filter(|level| *level <= format::MAX_LEVEL)
            .ok_or(Refusal::Capacity)?;
        self.store_page(&Page {
            kind,
            level,
            entries,
        })
        .await
        .map(Some)
    }
    async fn edit_page(
        &mut self,
        mut page: Page,
        key: &str,
        value: Option<Value>,
    ) -> Result<Vec<Page>, Refusal> {
        if page.level == 0 {
            match page
                .entries
                .binary_search_by(|entry| entry.key.as_str().cmp(key))
            {
                Ok(at) => match value {
                    Some(value) => page.entries[at].value = value,
                    None => {
                        page.entries.remove(at);
                    }
                },
                Err(at) => {
                    if let Some(value) = value {
                        page.entries.insert(
                            at,
                            Entry {
                                key: key.to_string(),
                                value,
                            },
                        );
                    }
                }
            }
        } else {
            let at = page
                .entries
                .iter()
                .position(|entry| entry.key.as_str() >= key)
                .unwrap_or(page.entries.len() - 1);
            let old = page.entries.remove(at);
            let Value::Child(metadata) = old.value else {
                return Err(Refusal::Integrity);
            };
            let child = self
                .page(&metadata, page.kind, Some(page.level - 1), Some(&old.key))
                .await?;
            let children = alloc::boxed::Box::pin(self.edit_page(child, key, value)).await?;
            for (offset, child) in children.into_iter().enumerate() {
                let key = child.maximum().map_err(|_| Refusal::Integrity)?.to_string();
                let metadata = self.store_page(&child).await?;
                page.entries.insert(
                    at + offset,
                    Entry {
                        key,
                        value: Value::Child(metadata),
                    },
                );
            }
        }
        if page.entries.is_empty() {
            return Ok(Vec::new());
        }
        if page.entries.len() > format::FANOUT {
            let right = page.entries.split_off(page.entries.len() / 2);
            let other = Page {
                kind: page.kind,
                level: page.level,
                entries: right,
            };
            Ok(vec![page, other])
        } else {
            Ok(vec![page])
        }
    }
    async fn inode(&mut self, root: &Root, id: u64) -> Result<Inode, Refusal> {
        match self
            .get(Some(&root.inodes), Kind::Inodes, &format::number(id))
            .await?
        {
            Some(Value::Inode(inode)) => Ok(inode),
            None => Err(Refusal::Missing),
            _ => Err(Refusal::Integrity),
        }
    }
    async fn inode_set(
        &mut self,
        root: &mut Root,
        id: u64,
        inode: Option<Inode>,
    ) -> Result<(), Refusal> {
        root.inodes = self
            .edit(
                Some(&root.inodes),
                Kind::Inodes,
                &format::number(id),
                inode.map(Value::Inode),
            )
            .await?
            .ok_or(Refusal::Integrity)?;
        Ok(())
    }
    async fn resolve(&mut self, root: &Root, path: &str) -> Result<u64, Refusal> {
        format::path(path).map_err(|_| Refusal::Invalid)?;
        let mut id = 1;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let inode = self.inode(root, id).await?;
            if !inode.directory {
                return Err(Refusal::Invalid);
            }
            id = match self.get(inode.tree.as_ref(), Kind::Names, part).await? {
                Some(Value::Name(id)) => id,
                None => return Err(Refusal::Missing),
                _ => return Err(Refusal::Integrity),
            };
        }
        Ok(id)
    }
    async fn parent(&mut self, root: &Root, path: &str) -> Result<(u64, String), Refusal> {
        format::path(path).map_err(|_| Refusal::Invalid)?;
        if path.is_empty() {
            return Err(Refusal::Invalid);
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let id = self.resolve(root, parent).await?;
        if !self.inode(root, id).await?.directory {
            return Err(Refusal::Invalid);
        }
        Ok((id, name.to_string()))
    }
    async fn link(
        &mut self,
        root: &mut Root,
        parent: u64,
        name: &str,
        inode: Option<u64>,
    ) -> Result<(), Refusal> {
        let mut directory = self.inode(root, parent).await?;
        if !directory.directory {
            return Err(Refusal::Invalid);
        }
        directory.tree = self
            .edit(
                directory.tree.as_ref(),
                Kind::Names,
                name,
                inode.map(Value::Name),
            )
            .await?;
        self.inode_set(root, parent, Some(directory)).await
    }
}

/// Real per-operation cancellation source. Native adapters observe an actual cancellation
/// Channel; observation never reuses an already issued driver operation or its loan.
pub trait Cancellation {
    /// True only when the actual owner requested cancellation; read failures stay errors.
    fn cancelled(&self) -> Result<bool, Refusal>;
}
pub use format::{Io, Opened};
impl Namespace {
    /// Acquire one actual channel-owned logical lease and mount/create its exact root on
    /// the existing physical owner. No host path, fallback device or role is selected.
    #[expect(
        clippy::too_many_arguments,
        reason = "Volume, registry, root, scope, Account, limits, creation policy and verifier are distinct authority and admission inputs."
    )]
    pub async fn open(
        volume: Rc<RefCell<Volume>>,
        registry: Rc<RefCell<Registry>>,
        root: Object,
        scope: Scope,
        account: Rc<moruna_sys::Account>,
        limits: Limits,
        create: bool,
        verifier: &dyn Verifier,
    ) -> Result<Self, Refusal> {
        limits.validate().map_err(|_| Refusal::Invalid)?;
        scope.validate().map_err(|_| Refusal::Invalid)?;
        root.validate().map_err(|_| Refusal::Invalid)?;
        format::root_object(&root).map_err(|_| Refusal::Invalid)?;
        if !Rc::ptr_eq(
            &registry
                .try_borrow()
                .map_err(|_| Refusal::Unavailable)?
                .owner,
            &volume,
        ) {
            return Err(Refusal::Denied);
        }
        let lease = Registry::acquire(&registry, &root)?;
        let reservation = (limits.metadata_bytes as u64)
            .checked_mul(3 * (u64::from(format::MAX_LEVEL) + 3))
            .and_then(|bytes| bytes.checked_add((limits.handles as u64).checked_mul(64)?))
            .ok_or(Refusal::Capacity)?;
        let metadata_budget =
            Memory::create_in(reservation, &account.0).map_err(|_| Refusal::Capacity)?;
        let result = (async {
            #[expect(
                clippy::await_holding_refcell_ref,
                reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
            )]
            let mut raw = volume.try_borrow_mut().map_err(|_| Refusal::Unavailable)?;
            let (workspace, bucket, role) = raw.identity();
            if workspace != root.workspace
                || bucket != root.bucket
                || !matches!(
                    role,
                    moruna_storage_format::Role::SingleNodeHome
                        | moruna_storage_format::Role::CheckpointMetadata
                )
            {
                return Err(Refusal::Denied);
            }
            verifier.verify(&Authorization::native(&scope, &root, Action::Read)?)?;
            let mut editor = Editor {
                volume: &mut raw,
                root: &root,
                account: &account.0,
                limits,
                remaining: limits.page_io,
            };
            let (metadata, index) = match editor.volume.current_slot(&root) {
                Ok(metadata) => {
                    let mut bytes = editor.bytes(&metadata).await?;
                    let index = bytes
                        .with_bytes(|bytes| {
                            Root::decode(&bytes[..metadata.bytes as usize])
                                .map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    (metadata, index)
                }
                Err(Refusal::Missing) if create => {
                    verifier.verify(&Authorization::native(&scope, &root, Action::Write)?)?;
                    let inode = Inode {
                        linked: true,
                        directory: true,
                        length: 0,
                        tree: None,
                    };
                    let inodes = editor
                        .store_page(&Page {
                            kind: Kind::Inodes,
                            level: 0,
                            entries: vec![Entry {
                                key: format::number(1),
                                value: Value::Inode(inode),
                            }],
                        })
                        .await?;
                    let index = Root {
                        next_inode: 2,
                        logical_bytes: 0,
                        inode_count: 1,
                        inodes,
                    };
                    let metadata = editor.publish(&index, &Condition::Absent).await?;
                    (metadata, index)
                }
                Err(error) => return Err(error),
            };
            if index.logical_bytes > limits.namespace_bytes || index.inode_count > limits.inodes {
                return Err(Refusal::Capacity);
            }
            let root_inode = editor.inode(&index, 1).await?;
            if !root_inode.linked || !root_inode.directory {
                return Err(Refusal::Integrity);
            }
            Ok((metadata, index))
        })
        .await;
        let (metadata, index) = match result {
            Ok(value) => value,
            Err(error) => {
                return Err(error);
            }
        };
        Ok(Self {
            volume,
            root,
            scope,
            account,
            limits,
            metadata,
            index,
            open: Vec::new(),
            next_handle: 1,
            _lease: lease,
            _metadata_budget: metadata_budget,
        })
    }
    fn authorize(&self, verifier: &dyn Verifier, action: Action) -> Result<(), Refusal> {
        if self._lease.poisoned.get() {
            return Err(Refusal::Unavailable);
        }
        verifier.verify(
            &Authorization::native(&self.scope, &self.root, action)?
                .with_version(&self.metadata.version)?,
        )
    }
    async fn transaction<T>(
        &mut self,
        verifier: &dyn Verifier,
        execute: impl core::ops::AsyncFnOnce(&mut Editor<'_>, &mut Root) -> Result<(T, bool), Refusal>,
    ) -> Result<T, Refusal> {
        let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
        self.authorize(verifier, Action::Publish)?;
        let mut next = self.index.clone();
        let result = (async {
            #[expect(
                clippy::await_holding_refcell_ref,
                reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
            )]
            let mut raw = self
                .volume
                .try_borrow_mut()
                .map_err(|_| Refusal::Unavailable)?;
            let mut editor = Editor {
                volume: &mut raw,
                root: &self.root,
                account: &self.account.0,
                limits: self.limits,
                remaining: self.limits.page_io,
            };
            let (value, commit) = execute(&mut editor, &mut next).await?;
            if next.logical_bytes > self.limits.namespace_bytes
                || next.inode_count > self.limits.inodes
            {
                return Err(Refusal::Capacity);
            }
            let metadata = if commit {
                Some(
                    editor
                        .publish(&next, &Condition::Match(self.metadata.version.clone()))
                        .await?,
                )
            } else {
                None
            };
            Ok((value, metadata))
        })
        .await;
        match result {
            Ok((value, metadata)) => {
                if let Some(metadata) = metadata {
                    self.metadata = metadata;
                    self.index = next;
                }
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
    fn opened(&self, handle: u64, writable: bool) -> Result<u64, Refusal> {
        self.open
            .iter()
            .find(|entry| entry.handle == handle && (!writable || entry.writable))
            .map(|entry| entry.inode)
            .ok_or(Refusal::Denied)
    }
    /// Open exact visible bytes. Creating a name is a real root transaction. An existing
    /// name under exclusive-create refuses; handle admission occurs before publication.
    pub async fn open_file(
        &mut self,
        path: &str,
        create: bool,
        exclusive: bool,
        writable: bool,
        verifier: &dyn Verifier,
    ) -> Result<Opened, Refusal> {
        if self.open.len() >= self.limits.handles {
            return Err(Refusal::Capacity);
        }
        let next_handle = self.next_handle.checked_add(1).ok_or(Refusal::Capacity)?;
        let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
        self.authorize(verifier, Action::Read)?;
        #[expect(
            clippy::await_holding_refcell_ref,
            reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
        )]
        let mut raw = self
            .volume
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let mut editor = Editor {
            volume: &mut raw,
            root: &self.root,
            account: &self.account.0,
            limits: self.limits,
            remaining: self.limits.page_io,
        };
        let found = editor.resolve(&self.index, path).await;
        let result = match found {
            Ok(id) => {
                if create && exclusive {
                    return Err(Refusal::Conflict);
                }
                let inode = editor.inode(&self.index, id).await?;
                Some((id, inode))
            }
            Err(Refusal::Missing) if create => None,
            Err(error) => return Err(error),
        };
        drop(raw);
        let (id, inode) = match result {
            Some(value) => value,
            None => {
                if !writable {
                    return Err(Refusal::Denied);
                }
                self.transaction(verifier, async |editor, index| {
                    let (parent, name) = editor.parent(index, path).await?;
                    let mut directory = editor.inode(index, parent).await?;
                    if editor
                        .get(directory.tree.as_ref(), Kind::Names, &name)
                        .await?
                        .is_some()
                    {
                        return Err(Refusal::Conflict);
                    }
                    let id = index.next_inode;
                    index.next_inode = id.checked_add(1).ok_or(Refusal::Capacity)?;
                    index.inode_count =
                        index.inode_count.checked_add(1).ok_or(Refusal::Capacity)?;
                    let inode = Inode {
                        linked: true,
                        directory: false,
                        length: 0,
                        tree: None,
                    };
                    editor.inode_set(index, id, Some(inode.clone())).await?;
                    directory.tree = editor
                        .edit(
                            directory.tree.as_ref(),
                            Kind::Names,
                            &name,
                            Some(Value::Name(id)),
                        )
                        .await?;
                    editor.inode_set(index, parent, Some(directory)).await?;
                    Ok(((id, inode), true))
                })
                .await?
            }
        };
        let handle = self.next_handle;
        self.next_handle = next_handle;
        self.open.push(Open {
            handle,
            inode: id,
            writable,
        });
        self._lease.open_inodes.borrow_mut().push(id);
        Ok(Opened {
            handle,
            inode: id,
            directory: inode.directory,
            length: inode.length,
        })
    }
    /// Close actual handle. The final open reference to an unlinked inode is removed by
    /// a real root commit before releasing it. Errors keep the handle live for retry.
    pub async fn close(&mut self, handle: u64, verifier: &dyn Verifier) -> Result<(), Refusal> {
        let id = self.opened(handle, false)?;
        let last = self.open.iter().filter(|entry| entry.inode == id).count() == 1;
        let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
        self.authorize(verifier, Action::Read)?;
        let linked = {
            #[expect(
                clippy::await_holding_refcell_ref,
                reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
            )]
            let mut raw = self
                .volume
                .try_borrow_mut()
                .map_err(|_| Refusal::Unavailable)?;
            let mut editor = Editor {
                volume: &mut raw,
                root: &self.root,
                account: &self.account.0,
                limits: self.limits,
                remaining: self.limits.page_io,
            };
            editor.inode(&self.index, id).await?.linked
        };
        if last && !linked {
            self.transaction(verifier, async |editor, index| {
                let inode = editor.inode(index, id).await?;
                if inode.linked {
                    return Ok(((), false));
                }
                index.logical_bytes = index
                    .logical_bytes
                    .checked_sub(inode.length)
                    .ok_or(Refusal::Integrity)?;
                index.inode_count = index.inode_count.checked_sub(1).ok_or(Refusal::Integrity)?;
                editor.inode_set(index, id, None).await?;
                Ok(((), true))
            })
            .await?;
        }
        self.open.retain(|entry| entry.handle != handle);
        *self._lease.open_inodes.borrow_mut() = self.open.iter().map(|entry| entry.inode).collect();
        Ok(())
    }
    /// Actual file or directory facts, freshly authorized, including unlinked open files.
    pub async fn stat(&mut self, handle: u64, verifier: &dyn Verifier) -> Result<Opened, Refusal> {
        let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
        self.authorize(verifier, Action::Read)?;
        let id = self.opened(handle, false)?;
        #[expect(
            clippy::await_holding_refcell_ref,
            reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
        )]
        let mut raw = self
            .volume
            .try_borrow_mut()
            .map_err(|_| Refusal::Unavailable)?;
        let mut editor = Editor {
            volume: &mut raw,
            root: &self.root,
            account: &self.account.0,
            limits: self.limits,
            remaining: self.limits.page_io,
        };
        let inode = editor.inode(&self.index, id).await?;
        Ok(Opened {
            handle,
            inode: id,
            directory: inode.directory,
            length: inode.length,
        })
    }
}
impl Editor<'_> {
    async fn publish(&mut self, index: &Root, condition: &Condition) -> Result<Metadata, Refusal> {
        self.work()?;
        let mut private = PrivateMemory::create_in(self.limits.metadata_bytes as u64, self.account)
            .map_err(|_| Refusal::Capacity)?;
        let length = private
            .with_bytes(|bytes| index.encode_into(bytes).map_err(|_| Refusal::Capacity))
            .map_err(|_| Refusal::Unavailable)??;
        let source = Loan::new(private.into_memory(), 0, length as u64, false)
            .map_err(|_| Refusal::Unavailable)?;
        self.volume
            .put_async(
                self.root,
                condition,
                Some(source),
                length as u64,
                self.account,
            )
            .await
    }
}
/// Actual cancellation capability. The caller cancels by closing its peer endpoint;
/// no caller-mutated shared bytes are interpreted through Rust references.
pub struct CancelChannel {
    channel: moruna_svc::Channel,
}
impl CancelChannel {
    /// Retain a real waitable Channel. This grants no namespace authority.
    pub fn new(channel: moruna_svc::Channel) -> Result<Self, Refusal> {
        let (kind, rights) = channel.0.info().map_err(|_| Refusal::Invalid)?;
        if kind != moruna_svc::ObjectType::Channel || !rights.contains(Rights::WAIT) {
            return Err(Refusal::Invalid);
        }
        Ok(Self { channel })
    }
}
impl Cancellation for CancelChannel {
    fn cancelled(&self) -> Result<bool, Refusal> {
        match moruna_svc::wait_one(
            &self.channel.0,
            moruna_svc::Signals::PEER_CLOSED,
            moruna_svc::now(),
        ) {
            Ok(signals) => Ok(signals.contains(moruna_svc::Signals::PEER_CLOSED)),
            Err(moruna_svc::Error::TimedOut) => Ok(false),
            Err(_) => Err(Refusal::Unavailable),
        }
    }
}

fn slice(loan: &Loan, offset: u64, length: u64, writable: bool) -> Result<Loan, Refusal> {
    if offset
        .checked_add(length)
        .is_none_or(|end| end > loan.len())
    {
        return Err(Refusal::Invalid);
    }
    let mut rights = Rights::READ | Rights::MAP | Rights::TRANSFER;
    if writable {
        rights = rights | Rights::WRITE;
    }
    let memory = loan.duplicate(rights).map_err(|_| Refusal::Invalid)?;
    Loan::new(
        memory,
        loan.offset().checked_add(offset).ok_or(Refusal::Invalid)?,
        length,
        writable,
    )
    .map_err(|_| Refusal::Invalid)
}
impl Editor<'_> {
    async fn block(&mut self, source: Option<&Metadata>) -> Result<PrivateMemory, Refusal> {
        let mut private = PrivateMemory::create_in(format::DATA_BYTES, self.account)
            .map_err(|_| Refusal::Capacity)?;
        match source {
            None => private
                .with_bytes(|bytes| bytes[..format::DATA_BYTES as usize].fill(0))
                .map_err(|_| Refusal::Unavailable)?,
            Some(source) => {
                self.work()?;
                self.internal(source)?;
                if source.bytes != format::DATA_BYTES {
                    return Err(Refusal::Integrity);
                }
                let memory = Memory::create_in(format::DATA_BYTES, self.account)
                    .map_err(|_| Refusal::Capacity)?;
                let loan = Loan::new(memory, 0, format::DATA_BYTES, true)
                    .map_err(|_| Refusal::Unavailable)?;
                self.volume
                    .read_async(
                        &source.object,
                        &source.version,
                        0,
                        format::DATA_BYTES,
                        Some(&loan),
                        self.account,
                    )
                    .await?;
                private
                    .with_bytes(|bytes| {
                        loan.read(0, &mut bytes[..format::DATA_BYTES as usize])
                            .map_err(|_| Refusal::Unavailable)
                    })
                    .map_err(|_| Refusal::Unavailable)??;
            }
        }
        Ok(private)
    }
    // Remove a whole suffix with only the one boundary path rewritten. A many-GB
    // truncate must not enumerate/rewrite every removed data page.
    async fn before(
        &mut self,
        tree: Option<&Metadata>,
        kind: Kind,
        cutoff: &str,
    ) -> Result<Option<Metadata>, Refusal> {
        let Some(tree) = tree else {
            return Ok(None);
        };
        let mut page = self.page(tree, kind, None, None).await?;
        if page.maximum().map_err(|_| Refusal::Integrity)? < cutoff {
            return Ok(Some(tree.clone()));
        }
        if page.level == 0 {
            page.entries.retain(|entry| entry.key.as_str() < cutoff);
        } else {
            let mut kept = Vec::new();
            for entry in page.entries {
                if entry.key.as_str() < cutoff {
                    kept.push(entry);
                    continue;
                }
                let Value::Child(child) = entry.value else {
                    return Err(Refusal::Integrity);
                };
                let validated = self
                    .page(&child, kind, Some(page.level - 1), Some(&entry.key))
                    .await?;
                drop(validated);
                if let Some(child) =
                    alloc::boxed::Box::pin(self.before(Some(&child), kind, cutoff)).await?
                {
                    let child_page = self.page(&child, kind, Some(page.level - 1), None).await?;
                    kept.push(Entry {
                        key: child_page
                            .maximum()
                            .map_err(|_| Refusal::Integrity)?
                            .to_string(),
                        value: Value::Child(child),
                    });
                }
                break;
            }
            page.entries = kept;
        }
        if page.entries.is_empty() {
            Ok(None)
        } else {
            self.store_page(&page).await.map(Some)
        }
    }
}
impl Namespace {
    /// Read actual durable bytes and zero holes, with true EOF short-read count. Only the
    /// completed prefix is exposed. Cancellation never releases the active driver's loan.
    pub async fn read_at(
        &mut self,
        handle: u64,
        offset: u64,
        destination: &Loan,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        let mut done = 0;
        let result = (async {
            let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
            self.authorize(verifier, Action::Read)?;
            let id = self.opened(handle, false)?;
            if destination.len() > self.limits.transfer_bytes {
                return Err(Refusal::Capacity);
            }
            #[expect(
                clippy::await_holding_refcell_ref,
                reason = "The mutable volume is an exclusive original-I/O lease; competing tasks use try_borrow_mut and refuse rather than reenter."
            )]
            let mut raw = self
                .volume
                .try_borrow_mut()
                .map_err(|_| Refusal::Unavailable)?;
            let mut editor = Editor {
                volume: &mut raw,
                root: &self.root,
                account: &self.account.0,
                limits: self.limits,
                remaining: self.limits.page_io,
            };
            let inode = editor.inode(&self.index, id).await?;
            if inode.directory {
                return Err(Refusal::Invalid);
            }
            let length = destination.len().min(inode.length.saturating_sub(offset));
            let zeros = [0; 4096];
            while done < length {
                if cancel.cancelled()? {
                    return Ok(false);
                }
                let position = offset.checked_add(done).ok_or(Refusal::Invalid)?;
                let within = position % format::DATA_BYTES;
                let count = (length - done).min(format::DATA_BYTES - within);
                match editor
                    .get(
                        inode.tree.as_ref(),
                        Kind::Data,
                        &format::number(position / format::DATA_BYTES),
                    )
                    .await?
                {
                    Some(Value::Data(source)) => {
                        editor.work()?;
                        editor.internal(&source)?;
                        let loan = slice(destination, done, count, true)?;
                        match editor
                            .volume
                            .read_async(
                                &source.object,
                                &source.version,
                                within,
                                count,
                                Some(&loan),
                                editor.account,
                            )
                            .await?
                        {
                            Outcome::Read { length, .. } if length == count => {}
                            _ => return Err(Refusal::Integrity),
                        }
                    }
                    None => {
                        let mut at = 0;
                        while at < count {
                            let n = (count - at).min(zeros.len() as u64) as usize;
                            destination
                                .write(done + at, &zeros[..n])
                                .map_err(|_| Refusal::Unavailable)?;
                            at += n as u64;
                        }
                    }
                    _ => return Err(Refusal::Integrity),
                }
                done += count;
            }
            Ok(true)
        })
        .await;
        match result {
            Ok(true) => Io::Done(done),
            Ok(false) => Io::Cancelled(done),
            Err(error) => Io::Failed(done, error),
        }
    }
    /// COW only affected fixed-size data pages and tree paths, then publish ONE root. No
    /// write bytes become visible until that actual receipt. Short/unacknowledged writes
    /// fail the transaction, leaving only unreachable chunks for the sole owner to reclaim.
    pub async fn write_at(
        &mut self,
        handle: u64,
        offset: u64,
        source: &Loan,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        let id = match self.opened(handle, true) {
            Ok(id) => id,
            Err(error) => return Io::Failed(0, error),
        };
        let length = source.len();
        let limits = self.limits;
        if length > limits.transfer_bytes {
            return Io::Failed(0, Refusal::Capacity);
        }
        let Some(end) = offset
            .checked_add(length)
            .filter(|end| *end <= limits.file_bytes)
        else {
            return Io::Failed(0, Refusal::Capacity);
        };
        let result = self
            .transaction(verifier, async |editor, index| {
                let mut inode = editor.inode(index, id).await?;
                if inode.directory {
                    return Err(Refusal::Invalid);
                }
                if length == 0 {
                    return Ok((Io::Done(0), false));
                }
                let new_length = inode.length.max(end);
                let logical = index
                    .logical_bytes
                    .checked_sub(inode.length)
                    .and_then(|bytes| bytes.checked_add(new_length))
                    .filter(|bytes| *bytes <= limits.namespace_bytes)
                    .ok_or(Refusal::Capacity)?;
                let mut done = 0;
                while done < length {
                    if cancel.cancelled()? {
                        return Ok((Io::Cancelled(0), false));
                    }
                    let position = offset + done;
                    let within = position % format::DATA_BYTES;
                    let count = (length - done).min(format::DATA_BYTES - within);
                    let key = format::number(position / format::DATA_BYTES);
                    let old = match editor.get(inode.tree.as_ref(), Kind::Data, &key).await? {
                        Some(Value::Data(data)) => Some(data),
                        None => None,
                        _ => return Err(Refusal::Integrity),
                    };
                    let mut block = editor.block(old.as_ref()).await?;
                    block
                        .with_bytes(|bytes| {
                            source
                                .read(done, &mut bytes[within as usize..(within + count) as usize])
                                .map_err(|_| Refusal::Unavailable)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    let metadata = editor.store(block, format::DATA_BYTES).await?;
                    if metadata.bytes != format::DATA_BYTES {
                        return Err(Refusal::Integrity);
                    }
                    inode.tree = editor
                        .edit(
                            inode.tree.as_ref(),
                            Kind::Data,
                            &key,
                            Some(Value::Data(metadata)),
                        )
                        .await?;
                    done += count;
                }
                if cancel.cancelled()? {
                    return Ok((Io::Cancelled(0), false));
                }
                inode.length = new_length;
                index.logical_bytes = logical;
                editor.inode_set(index, id, Some(inode)).await?;
                Ok((Io::Done(length), true))
            })
            .await;
        match result {
            Ok(result) => result,
            Err(error) => Io::Failed(0, error),
        }
    }
    /// Grow creates zero holes. Shrink zeros its boundary page and drops a whole tree
    /// suffix in O(depth) metadata IO, not O(file bytes); no stale truncated bytes revive.
    pub async fn truncate(
        &mut self,
        handle: u64,
        length: u64,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        let id = match self.opened(handle, true) {
            Ok(id) => id,
            Err(error) => return Io::Failed(0, error),
        };
        let limits = self.limits;
        if length > limits.file_bytes {
            return Io::Failed(0, Refusal::Capacity);
        }
        let result = self
            .transaction(verifier, async |editor, index| {
                if cancel.cancelled()? {
                    return Ok((Io::Cancelled(0), false));
                }
                let mut inode = editor.inode(index, id).await?;
                if inode.directory {
                    return Err(Refusal::Invalid);
                }
                index.logical_bytes = index
                    .logical_bytes
                    .checked_sub(inode.length)
                    .and_then(|bytes| bytes.checked_add(length))
                    .filter(|bytes| *bytes <= limits.namespace_bytes)
                    .ok_or(Refusal::Capacity)?;
                if length < inode.length {
                    if !length.is_multiple_of(format::DATA_BYTES) {
                        let key = format::number(length / format::DATA_BYTES);
                        match editor.get(inode.tree.as_ref(), Kind::Data, &key).await? {
                            Some(Value::Data(data)) => {
                                let mut block = editor.block(Some(&data)).await?;
                                block
                                    .with_bytes(|bytes| {
                                        bytes[(length % format::DATA_BYTES) as usize
                                            ..format::DATA_BYTES as usize]
                                            .fill(0)
                                    })
                                    .map_err(|_| Refusal::Unavailable)?;
                                let data = editor.store(block, format::DATA_BYTES).await?;
                                inode.tree = editor
                                    .edit(
                                        inode.tree.as_ref(),
                                        Kind::Data,
                                        &key,
                                        Some(Value::Data(data)),
                                    )
                                    .await?;
                            }
                            None => {}
                            _ => return Err(Refusal::Integrity),
                        }
                    }
                    inode.tree = editor
                        .before(
                            inode.tree.as_ref(),
                            Kind::Data,
                            &format::number(length.div_ceil(format::DATA_BYTES)),
                        )
                        .await?;
                }
                if cancel.cancelled()? {
                    return Ok((Io::Cancelled(0), false));
                }
                inode.length = length;
                editor.inode_set(index, id, Some(inode)).await?;
                Ok((Io::Done(0), true))
            })
            .await;
        match result {
            Ok(result) => result,
            Err(error) => Io::Failed(0, error),
        }
    }
    /// Persist file data/metadata through an ACTUAL device barrier. Mutations are already
    /// receipt-durable-before-ack; this still issues flush and propagates its real result.
    pub async fn sync_file(
        &mut self,
        handle: u64,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        self.sync(Some(handle), cancel, verifier).await
    }
    /// Persist directory/root visibility through the same actual physical barrier. The
    /// supplied handle must name a directory, not a fake always-success directory fsync.
    pub async fn sync_directory(
        &mut self,
        handle: u64,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        match self.stat(handle, verifier).await {
            Ok(facts) if facts.directory => self.sync(Some(handle), cancel, verifier).await,
            Ok(_) => Io::Failed(0, Refusal::Invalid),
            Err(error) => Io::Failed(0, error),
        }
    }
    #[expect(
        clippy::await_holding_refcell_ref,
        reason = "The volume borrow retains the exclusive owner through original sync I/O; contention is refused with Unavailable."
    )]
    async fn sync(
        &mut self,
        handle: Option<u64>,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Io {
        let result = (async {
            let _active = Active::enter(&self._lease.registry, &self._lease.poisoned)?;
            self.authorize(verifier, Action::Read)?;
            if let Some(handle) = handle {
                self.opened(handle, false)?;
            }
            if cancel.cancelled()? {
                return Ok(false);
            }
            self.volume
                .try_borrow_mut()
                .map_err(|_| Refusal::Unavailable)?
                .sync_async()
                .await?;
            Ok(true)
        })
        .await;
        match result {
            Ok(true) => Io::Done(0),
            Ok(false) => Io::Cancelled(0),
            Err(error) => Io::Failed(0, error),
        }
    }
}
impl Registry {
    /// True when an actual uncertain operation prevented safe lease retirement. This is
    /// diagnostic only; it never authorizes clearing a lease or reusing original IO memory.
    pub fn is_poisoned(&self, root: &Object) -> bool {
        self.held
            .iter()
            .any(|held| held.root == *root && held.poisoned.get())
    }
}
impl Editor<'_> {
    async fn ancestors(&mut self, index: &Root, path: &str, id: u64) -> Result<bool, Refusal> {
        let mut current = 1;
        if current == id {
            return Ok(true);
        }
        for name in path.split('/').filter(|part| !part.is_empty()) {
            let inode = self.inode(index, current).await?;
            if !inode.directory {
                return Err(Refusal::Invalid);
            }
            current = match self.get(inode.tree.as_ref(), Kind::Names, name).await? {
                Some(Value::Name(id)) => id,
                None => return Err(Refusal::Missing),
                _ => return Err(Refusal::Integrity),
            };
            if current == id {
                return Ok(true);
            }
        }
        Ok(false)
    }
    async fn unlink_inode(
        &mut self,
        index: &mut Root,
        id: u64,
        held: &[u64],
    ) -> Result<(), Refusal> {
        let mut inode = self.inode(index, id).await?;
        if held.contains(&id) {
            inode.linked = false;
            self.inode_set(index, id, Some(inode)).await
        } else {
            index.logical_bytes = index
                .logical_bytes
                .checked_sub(inode.length)
                .ok_or(Refusal::Integrity)?;
            index.inode_count = index.inode_count.checked_sub(1).ok_or(Refusal::Integrity)?;
            self.inode_set(index, id, None).await
        }
    }
}
impl Namespace {
    /// Last ACTUAL root publication acknowledgement, never an uncommitted index.
    pub fn receipt(&self) -> &Metadata {
        &self.metadata
    }
    /// Create one directory. Its actual inode/name changes commit together in one root.
    pub async fn create_directory(
        &mut self,
        path: &str,
        verifier: &dyn Verifier,
    ) -> Result<(), Refusal> {
        self.transaction(verifier, async |editor, index| {
            let (parent, name) = editor.parent(index, path).await?;
            let directory = editor.inode(index, parent).await?;
            if editor
                .get(directory.tree.as_ref(), Kind::Names, &name)
                .await?
                .is_some()
            {
                return Err(Refusal::Conflict);
            }
            let id = index.next_inode;
            index.next_inode = id.checked_add(1).ok_or(Refusal::Capacity)?;
            index.inode_count = index.inode_count.checked_add(1).ok_or(Refusal::Capacity)?;
            editor
                .inode_set(
                    index,
                    id,
                    Some(Inode {
                        linked: true,
                        directory: true,
                        length: 0,
                        tree: None,
                    }),
                )
                .await?;
            editor.link(index, parent, &name, Some(id)).await?;
            Ok(((), true))
        })
        .await
    }
    /// Atomic file/whole-subtree NOREPLACE/EXCHANGE/compatible replacement. Open handles
    /// retain stable inode identity. No pair of independent root writes imitates exchange.
    pub async fn rename(
        &mut self,
        from: &str,
        to: &str,
        mode: format::Rename,
        verifier: &dyn Verifier,
    ) -> Result<(), Refusal> {
        let held = self
            .open
            .iter()
            .map(|entry| entry.inode)
            .collect::<Vec<_>>();
        self.transaction(verifier, async |editor, index| {
            let (source_parent, source_name) = editor.parent(index, from).await?;
            let (target_parent, target_name) = editor.parent(index, to).await?;
            let source = editor.resolve(index, from).await?;
            let source_inode = editor.inode(index, source).await?;
            if from == to {
                return if mode == format::Rename::NoReplace {
                    Err(Refusal::Conflict)
                } else {
                    Ok(((), false))
                };
            }
            let destination = match editor.resolve(index, to).await {
                Ok(id) => Some((id, editor.inode(index, id).await?)),
                Err(Refusal::Missing) => None,
                Err(error) => return Err(error),
            };
            if mode == format::Rename::NoReplace && destination.is_some() {
                return Err(Refusal::Conflict);
            }
            if mode == format::Rename::Exchange && destination.is_none() {
                return Err(Refusal::Missing);
            }
            let to_parent_path = to.rsplit_once('/').map_or("", |(parent, _)| parent);
            if source_inode.directory && editor.ancestors(index, to_parent_path, source).await? {
                return Err(Refusal::Invalid);
            }
            if let Some((id, inode)) = &destination {
                if mode == format::Rename::Exchange {
                    let from_parent_path = from.rsplit_once('/').map_or("", |(parent, _)| parent);
                    if inode.directory && editor.ancestors(index, from_parent_path, *id).await? {
                        return Err(Refusal::Invalid);
                    }
                } else if inode.directory != source_inode.directory
                    || inode.directory && inode.tree.is_some()
                {
                    return Err(Refusal::Conflict);
                }
            }
            if mode == format::Rename::Exchange {
                let (target, _) = destination.ok_or(Refusal::Missing)?;
                editor
                    .link(index, source_parent, &source_name, Some(target))
                    .await?;
                editor
                    .link(index, target_parent, &target_name, Some(source))
                    .await?;
            } else {
                editor
                    .link(index, source_parent, &source_name, None)
                    .await?;
                editor
                    .link(index, target_parent, &target_name, Some(source))
                    .await?;
                if let Some((target, _)) = destination {
                    editor.unlink_inode(index, target, &held).await?;
                }
            }
            Ok(((), true))
        })
        .await
    }
    /// Unlink an exact file/empty directory. Open bytes remain part of the durable root
    /// until their actual final close; they are not eligible for physical reclamation.
    pub async fn remove(
        &mut self,
        path: &str,
        directory: bool,
        verifier: &dyn Verifier,
    ) -> Result<(), Refusal> {
        let held = self
            .open
            .iter()
            .map(|entry| entry.inode)
            .collect::<Vec<_>>();
        self.transaction(verifier, async |editor, index| {
            let (parent, name) = editor.parent(index, path).await?;
            let id = editor.resolve(index, path).await?;
            let inode = editor.inode(index, id).await?;
            if inode.directory != directory || directory && inode.tree.is_some() {
                return Err(Refusal::Conflict);
            }
            editor.link(index, parent, &name, None).await?;
            editor.unlink_inode(index, id, &held).await?;
            Ok(((), true))
        })
        .await
    }
}

struct Active {
    count: Rc<Cell<usize>>,
    owner: Rc<RefCell<Volume>>,
    poison: Rc<Cell<bool>>,
}
impl Active {
    fn enter(registry: &Rc<RefCell<Registry>>, poison: &Rc<Cell<bool>>) -> Result<Self, Refusal> {
        let state = registry.try_borrow().map_err(|_| Refusal::Unavailable)?;
        if state.frozen.get() {
            return Err(Refusal::Unavailable);
        }
        let next = state.active.get().checked_add(1).ok_or(Refusal::Capacity)?;
        state.active.set(next);
        Ok(Self {
            count: state.active.clone(),
            owner: state.owner.clone(),
            poison: poison.clone(),
        })
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        if self
            .owner
            .try_borrow()
            .ok()
            .is_some_and(|volume| !volume.admission_open())
        {
            self.poison.set(true);
        }
        self.count.set(self.count.get() - 1);
    }
}
struct FreezeState {
    registry: Rc<RefCell<Registry>>,
    frozen: Rc<Cell<bool>>,
}
impl Drop for FreezeState {
    fn drop(&mut self) {
        self.frozen.set(false);
    }
}
/// Actual mutation/original-IO barrier retained through live-root walk, bank copy,
/// publication and old-bank drain. Clones retain the SAME owner barrier, not a wire token.
#[derive(Clone)]
pub struct Frozen {
    state: Rc<FreezeState>,
}
impl Registry {
    /// Refuse unless every original namespace operation has drained and no uncertain
    /// lease exists. The shared physical owner must ALSO gate non-namespace operations.
    pub fn freeze(registry: &Rc<RefCell<Self>>) -> Result<Frozen, Refusal> {
        let state = registry.try_borrow().map_err(|_| Refusal::Unavailable)?;
        if state.frozen.get()
            || state.active.get() != 0
            || state.held.iter().any(|held| held.poisoned.get())
        {
            return Err(Refusal::Unavailable);
        }
        state.frozen.set(true);
        Ok(Frozen {
            state: Rc::new(FreezeState {
                registry: registry.clone(),
                frozen: state.frozen.clone(),
            }),
        })
    }
}
enum LiveFrame {
    Page {
        metadata: Metadata,
        kind: Kind,
        level: Option<u8>,
        maximum: Option<String>,
    },
    Data(Metadata),
}
/// Pre-admitted, sealed exact-metadata liveness set. Only trusted collection from actual
/// persisted namespace roots can populate it; caller/wire GC root lists are not accepted.
pub struct LiveSet {
    reader: crate::volume::ReservedRead,
    frames: Vec<LiveFrame>,
    identity: Rc<()>,
    owner: Rc<RefCell<Volume>>,
    _account: Rc<moruna_sys::Account>,
    remaining: u32,
    fingerprints: Vec<[u8; 32]>,
    max_objects: usize,
    max_roots: usize,
    frozen: Option<Frozen>,
    _budget: Memory,
}
fn metadata_fingerprint(metadata: &Metadata) -> Result<[u8; 32], Refusal> {
    format::metadata_fingerprint(metadata).map_err(|_| Refusal::Integrity)
}
impl LiveSet {
    /// Reserve actual fixed set/temporary-root capacity BEFORE frozen admission. The
    /// physical owner derives ceilings from its real journal/index budget, not a client.
    pub(crate) fn prepare(
        owner: Rc<RefCell<Volume>>,
        account: Rc<moruna_sys::Account>,
        limits: Limits,
        max_objects: usize,
        max_roots: usize,
    ) -> Result<Self, Refusal> {
        if max_objects == 0 || max_roots == 0 || max_roots > max_objects {
            return Err(Refusal::Invalid);
        }
        let mut reader = owner
            .try_borrow()
            .map_err(|_| Refusal::Busy)?
            .prepare_read(&account.0, limits.metadata_bytes)?;
        reader.set_io_budget(limits.page_io)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(2 * format::FANOUT * (usize::from(format::MAX_LEVEL) + 2))
            .map_err(|_| Refusal::Capacity)?;
        let identity = owner
            .try_borrow()
            .map_err(|_| Refusal::Unavailable)?
            .owner_identity();
        let bytes = (max_objects as u64)
            .checked_mul(64)
            .and_then(|bytes| bytes.checked_add((max_roots as u64).checked_mul(2048)?))
            .ok_or(Refusal::Capacity)?;
        let budget = Memory::create_in(bytes, &account.0).map_err(|_| Refusal::Capacity)?;
        let mut fingerprints = Vec::new();
        fingerprints
            .try_reserve_exact(max_objects)
            .map_err(|_| Refusal::Capacity)?;
        Ok(Self {
            reader,
            frames,
            identity,
            owner,
            _account: account,
            remaining: limits.page_io,
            fingerprints,
            max_objects,
            max_roots,
            frozen: None,
            _budget: budget,
        })
    }
    /// Collect from the sole owner's committed namespace-root enumeration. All outside
    /// reserved-prefix objects remain the compactor's unconditional responsibility.
    #[expect(
        clippy::await_holding_refcell_ref,
        reason = "Frozen liveness reads retain the actual volume owner through original I/O; competing borrows return Busy and the freeze barrier stays held."
    )]
    pub(crate) async fn collect(&mut self, frozen: Frozen) -> Result<(), Refusal> {
        if self.frozen.is_some() || !Rc::ptr_eq(&self.owner, &frozen.state.registry.borrow().owner)
        {
            return Err(Refusal::Denied);
        }
        let roots = self
            .owner
            .try_borrow()
            .map_err(|_| Refusal::Unavailable)?
            .namespace_roots(self.max_roots)?;
        for root in roots {
            if self
                .owner
                .try_borrow()
                .map_err(|_| Refusal::Busy)?
                .current_slot(&root.object)?
                != root
            {
                return Err(Refusal::Conflict);
            }
            self.remaining = self.remaining.checked_sub(1).ok_or(Refusal::Capacity)?;
            self.owner
                .try_borrow_mut()
                .map_err(|_| Refusal::Busy)?
                .read_reserved_async(&root.object, &root.version, &mut self.reader)
                .await?;
            let index = self
                .reader
                .with_bytes(|bytes| Root::decode(bytes).map_err(|_| Refusal::Integrity))??;
            self.mark(&root)?;
            let held = frozen
                .state
                .registry
                .borrow()
                .held
                .iter()
                .find(|held| held.root == root.object)
                .map(|held| held.open_inodes.borrow().clone())
                .unwrap_or_default();
            self.frames.clear();
            self.push(LiveFrame::Page {
                metadata: index.inodes,
                kind: Kind::Inodes,
                level: None,
                maximum: None,
            })?;
            while let Some(frame) = self.frames.pop() {
                let metadata = match &frame {
                    LiveFrame::Page { metadata, .. } | LiveFrame::Data(metadata) => metadata,
                };
                if metadata.object.workspace != root.object.workspace
                    || metadata.object.bucket != root.object.bucket
                    || !metadata
                        .object
                        .key
                        .starts_with(&format!("{}/objects/", root.object.key))
                {
                    return Err(Refusal::Integrity);
                }
                if self
                    .owner
                    .try_borrow()
                    .map_err(|_| Refusal::Busy)?
                    .current_slot(&metadata.object)?
                    != *metadata
                {
                    return Err(Refusal::Conflict);
                }
                self.mark(metadata)?;
                match frame {
                    LiveFrame::Data(metadata) => {
                        if metadata.bytes != format::DATA_BYTES {
                            return Err(Refusal::Integrity);
                        }
                    }
                    LiveFrame::Page {
                        metadata,
                        kind,
                        level,
                        maximum,
                    } => {
                        self.remaining = self.remaining.checked_sub(1).ok_or(Refusal::Capacity)?;
                        self.owner
                            .try_borrow_mut()
                            .map_err(|_| Refusal::Busy)?
                            .read_reserved_async(
                                &metadata.object,
                                &metadata.version,
                                &mut self.reader,
                            )
                            .await?;
                        let page = self.reader.with_bytes(|bytes| {
                            Page::decode(bytes).map_err(|_| Refusal::Integrity)
                        })??;
                        if page.kind != kind
                            || level.is_some_and(|level| level != page.level)
                            || maximum.as_ref().is_some_and(|maximum| {
                                page.maximum().ok() != Some(maximum.as_str())
                            })
                        {
                            return Err(Refusal::Integrity);
                        }
                        for entry in page.entries.into_iter().rev() {
                            match entry.value {
                                Value::Child(child) => self.push(LiveFrame::Page {
                                    metadata: child,
                                    kind,
                                    level: Some(page.level - 1),
                                    maximum: Some(entry.key),
                                })?,
                                Value::Inode(inode) => {
                                    let id = format::parse_number(&entry.key)
                                        .map_err(|_| Refusal::Integrity)?;
                                    if !inode.linked && !held.contains(&id) {
                                        continue;
                                    }
                                    if let Some(tree) = inode.tree {
                                        self.push(LiveFrame::Page {
                                            metadata: tree,
                                            kind: if inode.directory {
                                                Kind::Names
                                            } else {
                                                Kind::Data
                                            },
                                            level: None,
                                            maximum: None,
                                        })?;
                                    }
                                }
                                Value::Data(data) => self.push(LiveFrame::Data(data))?,
                                Value::Name(_) => {}
                            }
                        }
                    }
                }
            }
        }
        self.fingerprints.sort_unstable();
        self.fingerprints.dedup();
        self.frozen = Some(frozen);
        Ok(())
    }
    fn push(&mut self, frame: LiveFrame) -> Result<(), Refusal> {
        if self.frames.len() >= 2 * format::FANOUT * (usize::from(format::MAX_LEVEL) + 2) {
            return Err(Refusal::Capacity);
        }
        self.frames.push(frame);
        Ok(())
    }
    fn mark(&mut self, metadata: &Metadata) -> Result<(), Refusal> {
        if self.fingerprints.len() >= self.max_objects {
            return Err(Refusal::Capacity);
        }
        self.fingerprints.push(metadata_fingerprint(metadata)?);
        Ok(())
    }
    /// Validate an already-mutably-borrowed Volume without reborrowing the Rc. The
    /// token is minted only by the actual mounted Volume, never a caller name or bool.
    pub(crate) fn matches_volume(&self, volume: &Volume) -> bool {
        self.frozen.is_some() && Rc::ptr_eq(&self.identity, &volume.owner_identity())
    }
    /// Exact pinned-metadata membership. Unencodable identity is an ERROR, never false
    /// (which could otherwise silently erase a real referenced object during compaction).
    pub(crate) fn contains(&self, metadata: &Metadata) -> Result<bool, Refusal> {
        if self.frozen.is_none() {
            return Err(Refusal::Unavailable);
        }
        Ok(self
            .fingerprints
            .binary_search(&metadata_fingerprint(metadata)?)
            .is_ok())
    }
}
impl Namespace {
    /// Execute a bounded ordered native batch over the SAME actual namespace. These are
    /// genuine awaited Volume operations; a linked successor is never submitted after a
    /// short/failed/cancelled predecessor. No completion is invented for skipped steps.
    pub async fn execute(
        &mut self,
        request: format::Request,
        payload: Option<Loan>,
        cancel: &dyn Cancellation,
        verifier: &dyn Verifier,
    ) -> Result<format::Response, Refusal> {
        use format::{Command, Outcome as ResultValue};
        request.validate().map_err(|_| Refusal::Invalid)?;
        let (bytes, writable) = request.payload().map_err(|_| Refusal::Invalid)?;
        if bytes > self.limits.transfer_bytes
            || payload.as_ref().map(Loan::len).unwrap_or(0) != bytes
        {
            return Err(Refusal::Capacity);
        }
        let mut outcomes = Vec::new();
        outcomes
            .try_reserve_exact(request.steps.len())
            .map_err(|_| Refusal::Capacity)?;
        let mut blocked = false;
        for step in request.steps {
            if blocked {
                outcomes.push(ResultValue::Skipped);
                blocked = step.link;
                continue;
            }
            if cancel.cancelled()? {
                outcomes.push(ResultValue::Cancelled);
                blocked = step.link;
                continue;
            }
            let outcome = match &step.command {
                Command::Open {
                    path,
                    create,
                    exclusive,
                    writable,
                } => match self
                    .open_file(path, *create, *exclusive, *writable, verifier)
                    .await
                {
                    Ok(facts) => ResultValue::Opened(facts),
                    Err(error) => ResultValue::Refused(error),
                },
                Command::Close { handle } => match self.close(*handle, verifier).await {
                    Ok(()) => ResultValue::Done,
                    Err(error) => ResultValue::Refused(error),
                },
                Command::Stat { handle } => match self.stat(*handle, verifier).await {
                    Ok(facts) => ResultValue::Opened(facts),
                    Err(error) => ResultValue::Refused(error),
                },
                Command::ReadAt {
                    handle,
                    offset,
                    length,
                    payload_offset,
                }
                | Command::WriteAt {
                    handle,
                    offset,
                    length,
                    payload_offset,
                } => {
                    let empty = if bytes == 0 {
                        Some(
                            Loan::new(
                                Memory::create_in(1, &self.account.0)
                                    .map_err(|_| Refusal::Capacity)?,
                                0,
                                0,
                                writable,
                            )
                            .map_err(|_| Refusal::Invalid)?,
                        )
                    } else {
                        None
                    };
                    let original = payload
                        .as_ref()
                        .or(empty.as_ref())
                        .ok_or(Refusal::Invalid)?;
                    // Actual origin/rights checked by the native Server before Loan creation.
                    let loan = slice(original, *payload_offset, *length, writable)?;
                    ResultValue::Io(if matches!(&step.command, Command::ReadAt { .. }) {
                        self.read_at(*handle, *offset, &loan, cancel, verifier)
                            .await
                    } else {
                        self.write_at(*handle, *offset, &loan, cancel, verifier)
                            .await
                    })
                }
                Command::Truncate { handle, length } => {
                    ResultValue::Io(self.truncate(*handle, *length, cancel, verifier).await)
                }
                Command::SyncFile { handle } => {
                    ResultValue::Io(self.sync_file(*handle, cancel, verifier).await)
                }
                Command::SyncDirectory { handle } => {
                    ResultValue::Io(self.sync_directory(*handle, cancel, verifier).await)
                }
                Command::CreateDirectory { path } => {
                    match self.create_directory(path, verifier).await {
                        Ok(()) => ResultValue::Done,
                        Err(error) => ResultValue::Refused(error),
                    }
                }
                Command::Rename { from, to, mode } => {
                    match self.rename(from, to, *mode, verifier).await {
                        Ok(()) => ResultValue::Done,
                        Err(error) => ResultValue::Refused(error),
                    }
                }
                Command::Remove { path, directory } => {
                    match self.remove(path, *directory, verifier).await {
                        Ok(()) => ResultValue::Done,
                        Err(error) => ResultValue::Refused(error),
                    }
                }
            };
            let success = match (&step.command, &outcome) {
                (
                    Command::ReadAt { length, .. } | Command::WriteAt { length, .. },
                    ResultValue::Io(Io::Done(bytes)),
                ) => bytes == length,
                (_, ResultValue::Io(Io::Done(_)) | ResultValue::Done | ResultValue::Opened(_)) => {
                    true
                }
                _ => false,
            };
            blocked = step.link && !success;
            outcomes.push(outcome);
        }
        Ok(format::Response {
            id: request.id,
            receipt: self.metadata.clone(),
            outcomes,
        })
    }
}
