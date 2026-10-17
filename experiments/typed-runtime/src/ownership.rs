use super::*;
use std::rc::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerHandle {
    pub(super) runtime: u64,
    pub(super) key: Key,
    marker: PhantomData<Rc<()>>,
}
#[derive(Clone)]
pub struct WeakRuntime(Weak<Inner>);
impl WeakRuntime {
    pub fn upgrade(&self) -> Option<Runtime> {
        self.0.upgrade().map(Runtime)
    }
}
type Cleanup = Box<dyn FnMut(&Runtime) -> Result<()>>;
// A logical execution scope has no public handle until materialization. Its
// node generation + epoch prevent a suspended context attaching to a new run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingOwner {
    pub node: Key,
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Child {
    Physical(Key),
    Deferred(Key),
}
pub(super) struct OwnerSlot {
    generation: u32,
    value: Option<Owner>,
}
#[cfg(test)]
pub(super) fn set_generation(g: &mut Graph, key: Key, generation: u32) -> Key {
    let o = owner(g, key).unwrap();
    let parent = o.parent;
    let pos = o.position;
    let new = Key { generation, ..key };
    g.owners[key.index as usize].generation = generation;
    if let Some(parent) = parent {
        owner_mut(g, parent).unwrap().children[pos] = Child::Physical(new);
    } else {
        g.roots[pos] = new;
    }
    new
}
pub(super) struct Owner {
    parent: Option<Key>,
    position: usize,
    pub depth: usize,
    children: Vec<Child>,
    pub nodes: Vec<Key>,
    cleanups: Vec<Cleanup>,
}
impl Owner {
    pub(super) fn parent_key(&self) -> Option<Key> {
        self.parent
    }
}
pub(super) fn owner(g: &Graph, key: Key) -> Result<&Owner> {
    let slot = g
        .owners
        .get(key.index as usize)
        .ok_or(Error::InvalidHandle)?;
    if slot.generation != key.generation {
        return Err(Error::InvalidHandle);
    }
    slot.value.as_ref().ok_or(Error::InvalidHandle)
}
pub(super) fn owner_mut(g: &mut Graph, key: Key) -> Result<&mut Owner> {
    owner(g, key)?;
    Ok(g.owners[key.index as usize].value.as_mut().unwrap())
}
pub(super) fn owner_count(g: &mut Graph) -> usize {
    g.owners.iter().filter(|o| o.value.is_some()).count()
        + g.cold.iter().flatten().filter(|c| c.scope_deferred).count()
}
pub(super) fn materialize(g: &mut Graph, pending: PendingOwner) -> Result<Key> {
    header(g, pending.node).map_err(|_| Error::OwnerDisposed)?;
    let c = g.cold[pending.node.index as usize].as_ref().unwrap();
    if c.scope_epoch != pending.epoch {
        return Err(Error::OwnerDisposed);
    }
    if let Some(key) = c.execution_owner {
        owner(g, key).map_err(|_| Error::OwnerDisposed)?;
        return Ok(key);
    }
    if !c.scope_deferred {
        return Err(Error::OwnerDisposed);
    }
    let parent = c.owner;
    let position = c.scope_position;
    if owner(g, parent)?.children.get(position) != Some(&Child::Deferred(pending.node)) {
        return Err(Error::Invariant);
    }
    let key = insert_at(g, Some(parent), Some(position))?;
    let c = g.cold[pending.node.index as usize].as_mut().unwrap();
    c.execution_owner = Some(key);
    c.scope_deferred = false;
    c.scope_position = usize::MAX;
    Ok(key)
}
#[cfg(feature = "diagnostics")]
pub(super) fn capacity_bytes(g: &Graph) -> usize {
    g.owners.capacity() * std::mem::size_of::<OwnerSlot>()
        + g.owners
            .iter()
            .filter_map(|s| s.value.as_ref())
            .map(|o| {
                o.children.capacity() * std::mem::size_of::<Child>()
                    + o.nodes.capacity() * std::mem::size_of::<Key>()
                    + o.cleanups.capacity() * std::mem::size_of::<Cleanup>()
            })
            .sum::<usize>()
}
fn insert(g: &mut Graph, parent: Option<Key>) -> Result<Key> {
    insert_at(g, parent, None)
}
fn insert_at(g: &mut Graph, parent: Option<Key>, reserved: Option<usize>) -> Result<Key> {
    let depth = match parent {
        Some(p) => owner(g, p)?.depth.checked_add(1).ok_or(Error::DepthLimit)?,
        None => 0,
    };
    let index = if let Some(index) = g.owner_free.pop() {
        index
    } else {
        let index = u32::try_from(g.owners.len()).map_err(|_| Error::Exhausted)?;
        g.owners.push(OwnerSlot {
            generation: 1,
            value: None,
        });
        index
    };
    let position = match parent {
        Some(p) => reserved.unwrap_or(owner(g, p)?.children.len()),
        None => g.roots.len(),
    };
    let slot = &mut g.owners[index as usize];
    slot.value = Some(Owner {
        parent,
        position,
        depth,
        children: Vec::new(),
        nodes: Vec::new(),
        cleanups: Vec::new(),
    });
    let key = Key {
        index,
        generation: slot.generation,
    };
    if let Some(p) = parent {
        if reserved.is_some() {
            owner_mut(g, p)?.children[position] = Child::Physical(key);
        } else {
            owner_mut(g, p)?.children.push(Child::Physical(key));
        }
    } else {
        g.roots.push(key);
    }
    Ok(key)
}
fn swap_remove_key(keys: &mut Vec<Key>, key: Key) {
    if let Some(i) = keys.iter().position(|&k| k == key) {
        keys.swap_remove(i);
    }
}
fn remove_child(g: &mut Graph, parent: Key, position: usize, expected: Child) {
    let siblings = &mut owner_mut(g, parent).unwrap().children;
    assert_eq!(siblings.swap_remove(position), expected);
    let moved = siblings.get(position).copied();
    match moved {
        Some(Child::Physical(key)) => owner_mut(g, key).unwrap().position = position,
        Some(Child::Deferred(key)) => {
            g.cold[key.index as usize].as_mut().unwrap().scope_position = position
        }
        None => (),
    }
}
struct Context<'a> {
    rt: &'a Runtime,
    owner: Option<Key>,
    reader: Option<Key>,
    disposal: usize,
    pending_owner: Option<PendingOwner>,
}
impl Drop for Context<'_> {
    fn drop(&mut self) {
        self.rt.0.owner.set(self.owner);
        self.rt.0.reader.set(self.reader);
        self.rt.0.disposal.set(self.disposal);
        self.rt.0.pending_owner.set(self.pending_owner);
    }
}
struct Retired {
    owner: Owner,
    nodes: Vec<Cold>,
}

// Called only with graph access, never calls user code or drops a payload.
fn detach_node(rt: &Runtime, g: &mut Graph, key: Key) -> Cold {
    let c = g.cold[key.index as usize].take().unwrap();
    if c.scope_deferred && owner(g, c.owner).is_ok() {
        remove_child(g, c.owner, c.scope_position, Child::Deferred(key));
    }
    scheduling::cancel(g, key);
    let h = &mut g.hot[key.index as usize];
    h.alive = false;
    if let Some(n) = h.generation.checked_add(1) {
        h.generation = n;
        g.free.push(key.index);
    }
    for source in &c.sources {
        unlink(g, *source, key);
    }
    for observer in &c.observers {
        if header(g, observer.key).is_ok() {
            shared_plan::local_structure_changed(rt, g, observer.key);
            g.cold[observer.key.index as usize]
                .as_mut()
                .unwrap()
                .sources[observer.source_index]
                .live = false;
            g.marks.push((observer.key, State::Dirty));
        }
    }
    drain_marks(g);
    c
}
// Mark the entire subtree unavailable before extraction or any callback.
fn detach(rt: &Runtime, g: &mut Graph, roots: &[Key]) -> Vec<Retired> {
    let mut groups = Vec::new();
    for &root in roots {
        if owner(g, root).is_err() {
            continue;
        }
        let mut ids = vec![root];
        let mut i = 0;
        while i < ids.len() {
            let o = owner(g, ids[i]).unwrap();
            ids.extend(o.children.iter().filter_map(|c| match c {
                Child::Physical(key) => Some(*key),
                Child::Deferred(_) => None,
            }));
            i += 1;
        }
        groups.push(ids);
    }
    for ids in &groups {
        for &id in ids {
            for i in 0..owner(g, id).unwrap().nodes.len() {
                let node = owner(g, id).unwrap().nodes[i];
                g.hot[node.index as usize].alive = false;
            }
        }
    }
    let mut retired = Vec::new();
    for ids in groups {
        for id in ids.into_iter().rev() {
            let slot = &mut g.owners[id.index as usize];
            let o = slot.value.take().unwrap();
            if let Some(n) = slot.generation.checked_add(1) {
                slot.generation = n;
                g.owner_free.push(id.index);
            }
            if let Some(p) = o.parent {
                remove_child(g, p, o.position, Child::Physical(id));
            } else {
                assert_eq!(g.roots.swap_remove(o.position), id);
                if let Some(moved) = g.roots.get(o.position).copied() {
                    owner_mut(g, moved).unwrap().position = o.position;
                }
            }
            let nodes = o
                .nodes
                .iter()
                .map(|&node| detach_node(rt, g, node))
                .collect();
            retired.push(Retired { owner: o, nodes });
        }
    }
    retired
}

impl Runtime {
    pub fn downgrade(&self) -> WeakRuntime {
        WeakRuntime(Rc::downgrade(&self.0))
    }
    pub(super) fn owner_handle(&self, key: Key) -> OwnerHandle {
        OwnerHandle {
            runtime: self.0.id,
            key,
            marker: PhantomData,
        }
    }
    fn resolve_owner(&self, h: OwnerHandle) -> Result<Key> {
        if h.runtime != self.0.id {
            return Err(Error::WrongRuntime);
        }
        self.graph(|g| owner(g, h.key).map(|_| h.key))
    }
    pub fn create_root(&self) -> Result<OwnerHandle> {
        self.check(CREATE)?;
        self.check_current_owner()?;
        self.graph(|g| insert(g, None))
            .map(|key| self.owner_handle(key))
    }
    pub fn create_owner(&self, parent: OwnerHandle) -> Result<OwnerHandle> {
        self.check(CREATE)?;
        self.check_current_owner()?;
        let parent = self.resolve_owner(parent)?;
        self.graph(|g| insert(g, Some(parent)))
            .map(|key| self.owner_handle(key))
    }
    pub fn current_owner(&self) -> Result<Option<OwnerHandle>> {
        self.check(READ)?;
        self.check_current_owner()?;
        self.current_owner_key()
            .map(|key| key.map(|key| self.owner_handle(key)))
    }
    pub(super) fn current_owner_key(&self) -> Result<Option<Key>> {
        if let Some(pending) = self.0.pending_owner.get() {
            return self.graph(|g| materialize(g, pending)).map(Some);
        }
        Ok(self.0.owner.get())
    }
    pub fn validate_owner(&self, h: OwnerHandle) -> Result<()> {
        self.check(READ)?;
        self.resolve_owner(h).map(|_| ())
    }
    pub fn with_owner<R>(
        &self,
        h: OwnerHandle,
        f: impl FnOnce(&Runtime) -> Result<R>,
    ) -> Result<R> {
        self.boundary(|| self.with_owner_inner(h, f))
    }
    fn with_owner_inner<R>(
        &self,
        h: OwnerHandle,
        f: impl FnOnce(&Runtime) -> Result<R>,
    ) -> Result<R> {
        let mut f = Protected(self, Some(f));
        self.check(READ)?;
        let key = self.resolve_owner(h)?;
        let _context = Context {
            rt: self,
            owner: self.0.owner.replace(Some(key)),
            reader: self.0.reader.get(),
            disposal: self.0.disposal.get(),
            pending_owner: self.0.pending_owner.replace(None),
        };
        self.user(|| f.1.take().unwrap()(self))
    }
    pub fn on_cleanup(&self, f: impl FnMut(&Runtime) -> Result<()> + 'static) -> Result<()> {
        let mut callback = Protected(self, Some(Box::new(f) as Cleanup));
        self.check(CREATE)?;
        let key = self.current_owner_key()?.ok_or(Error::MissingOwner)?;
        self.graph(|g| {
            owner_mut(g, key)
                .map_err(|_| Error::OwnerDisposed)?
                .cleanups
                .push(callback.1.take().unwrap());
            Ok(())
        })
    }
    pub fn dispose_owner(&self, h: OwnerHandle) -> Result<()> {
        self.boundary(|| self.dispose_owner_inner(h))
    }
    fn dispose_owner_inner(&self, h: OwnerHandle) -> Result<()> {
        if CAPS.get() & DISPOSE == 0 {
            return Err(capability_error(DISPOSE));
        }
        if h.runtime != self.0.id {
            return Err(Error::WrongRuntime);
        }
        self.next_disposal_depth()?;
        // Repeated disposal of an invalidated owner is harmless.
        if self.resolve_owner(h).is_err() {
            return Ok(());
        }
        let retired = self.graph(|g| detach(self, g, &[h.key]));
        self.drain_owners(retired)
    }
    pub(super) fn dispose_node(&self, key: Key) -> Result<()> {
        let scope = self.graph(|g| g.cold[key.index as usize].as_ref().unwrap().execution_owner);
        // Detach the node and its execution subtree before first cleanup.
        let (retired, c) = self.graph(|g| {
            g.hot[key.index as usize].alive = false;
            let retired = scope.map(|s| detach(self, g, &[s])).unwrap_or_default();
            let c = detach_node(self, g, key);
            if let Ok(o) = owner_mut(g, c.owner) {
                swap_remove_key(&mut o.nodes, key);
            }
            (retired, c)
        });
        let mut panic = None;
        let result = catch_unwind(AssertUnwindSafe(|| self.drain_owners(retired)));
        let error = match result {
            Ok(result) => result.err(),
            Err(p) => {
                panic = Some(p);
                None
            }
        };
        capture_drop_with(|| self.retire(Owned(c, Some(self))), &mut panic);
        if let Some(p) = panic {
            resume_unwind(p);
        }
        error.map_or(Ok(()), Err)
    }
    pub(super) fn prepare_execution(&self, key: Key) -> Result<bool> {
        self.next_disposal_depth()?;
        let (old, parent, epoch) = self.graph(|g| {
            let c = g.cold[key.index as usize].as_ref().unwrap();
            let (old, parent, epoch) = (c.execution_owner, c.owner, c.scope_epoch);
            if old.is_none() {
                // Same depth-overflow preflight as allocating a physical owner.
                owner(g, parent)?
                    .depth
                    .checked_add(1)
                    .ok_or(Error::DepthLimit)?;
                let c = g.cold[key.index as usize].as_mut().unwrap();
                if !c.scope_deferred {
                    let position = owner(g, parent)?.children.len();
                    owner_mut(g, parent)?.children.push(Child::Deferred(key));
                    let c = g.cold[key.index as usize].as_mut().unwrap();
                    c.scope_deferred = true;
                    c.scope_position = position;
                }
            }
            Ok((old, parent, epoch))
        })?;
        if old.is_none() {
            self.0.owner.set(None);
            self.0
                .pending_owner
                .set(Some(PendingOwner { node: key, epoch }));
            return Ok(true);
        }
        // Empty physical scopes renew generations but retain their buffers and
        // index. Handles from the preceding execution must still become stale.
        let renewed = self.graph(|g| {
            if let Some(old) = old {
                let o = owner(g, old)?;
                if o.nodes.is_empty()
                    && o.children.is_empty()
                    && o.cleanups.is_empty()
                    && let Some(n) = old.generation.checked_add(1)
                {
                    let new = Key {
                        generation: n,
                        ..old
                    };
                    g.owners[old.index as usize].generation = n;
                    let position = owner(g, new)?.position;
                    let p = owner_mut(g, parent)?;
                    p.children[position] = Child::Physical(new);
                    return Ok(Some(new));
                }
            }
            Ok(None)
        })?;
        let new = if let Some(new) = renewed {
            new
        } else {
            if let Some(old) = old {
                // Retire the backlink before invoking cleanups: even an Err or
                // panic must not leave a live node pointing at a dead owner.
                self.graph(|g| g.cold[key.index as usize].as_mut().unwrap().execution_owner = None);
                let retired = self.graph(|g| detach(self, g, &[old]));
                self.drain_owners(retired)?;
            }
            if self.graph(|g| header(g, key).is_err()) {
                return Ok(false);
            }
            self.check(READ)?;
            let epoch = self.graph(|g| {
                owner(g, parent)?
                    .depth
                    .checked_add(1)
                    .ok_or(Error::DepthLimit)?;
                let position = owner(g, parent)?.children.len();
                owner_mut(g, parent)?.children.push(Child::Deferred(key));
                let c = g.cold[key.index as usize].as_mut().unwrap();
                c.scope_deferred = true;
                c.scope_position = position;
                Ok(c.scope_epoch)
            })?;
            self.0.owner.set(None);
            self.0
                .pending_owner
                .set(Some(PendingOwner { node: key, epoch }));
            return Ok(true);
        };
        self.graph(|g| g.cold[key.index as usize].as_mut().unwrap().execution_owner = Some(new));
        self.0.owner.set(Some(new));
        self.0.pending_owner.set(None);
        Ok(true)
    }
    pub(super) fn shutdown_owners(&self) -> Result<()> {
        let retired = self.graph(|g| {
            let roots = g.roots.iter().rev().copied().collect::<Vec<_>>();
            detach(self, g, &roots)
        });
        self.drain_owners(retired)
    }
    fn drain_owners(&self, retired: Vec<Retired>) -> Result<()> {
        let depth = self.next_disposal_depth()?;
        let _context = Context {
            rt: self,
            owner: self.0.owner.replace(None),
            reader: self.0.reader.replace(None),
            disposal: self.0.disposal.replace(depth),
            pending_owner: self.0.pending_owner.replace(None),
        };
        let _caps = Caps::enter(READ | WRITE | DISPOSE);
        let _reader = ReaderRuntime::enter(None);
        let mut panic = None;
        let mut error = None;
        for mut r in retired {
            while let Some(mut callback) = r.owner.cleanups.pop() {
                let _callback = shared_plan::Callback::enter(self);
                match catch_unwind(AssertUnwindSafe(|| callback(self))) {
                    Ok(Ok(())) => (),
                    Ok(Err(e)) => {
                        self.record_teardown_failure(e);
                        if error.is_none() {
                            error = Some(e);
                        }
                    }
                    Err(p) => {
                        self.record_teardown_failure(Error::Panic);
                        keep_panic(p, &mut panic);
                    }
                }
                capture_drop_with(|| self.retire(callback), &mut panic);
            }
            for c in r.nodes {
                capture_drop_with(|| self.retire(Owned(c, Some(self))), &mut panic);
            }
        }
        if let Some(p) = panic {
            resume_unwind(p);
        }
        error.map_or(Ok(()), Err)
    }
    fn check_current_owner(&self) -> Result<()> {
        if let Some(pending) = self.0.pending_owner.get() {
            return self.graph(|g| {
                header(g, pending.node).map_err(|_| Error::OwnerDisposed)?;
                let c = g.cold[pending.node.index as usize].as_ref().unwrap();
                if c.scope_epoch != pending.epoch {
                    return Err(Error::OwnerDisposed);
                }
                if let Some(key) = c.execution_owner {
                    owner(g, key).map(|_| ()).map_err(|_| Error::OwnerDisposed)
                } else if c.scope_deferred {
                    Ok(())
                } else {
                    Err(Error::OwnerDisposed)
                }
            });
        }
        if let Some(key) = self.0.owner.get() {
            self.graph(|g| owner(g, key).map(|_| ()).map_err(|_| Error::OwnerDisposed))?;
        }
        Ok(())
    }
    pub fn owner_depth(&self, h: OwnerHandle) -> Result<usize> {
        self.check(READ)?;
        let key = self.resolve_owner(h)?;
        self.graph(|g| Ok(owner(g, key)?.depth))
    }
}
fn keep_panic(p: Panic, first: &mut Option<Panic>) {
    if first.is_none() {
        *first = Some(p);
    } else {
        discard_panic(p);
    }
}
fn capture_drop_with(f: impl FnOnce(), first: &mut Option<Panic>) {
    if let Err(p) = catch_unwind(AssertUnwindSafe(f)) {
        keep_panic(p, first);
    }
}

// Final Rc release, unlike explicit shutdown, does not invoke cleanups.
pub(super) fn release_owners(g: &mut Graph) {
    let _caps = Caps::enter(0);
    let mut first = None;
    for slot in &mut g.owners {
        if let Some(mut o) = slot.value.take() {
            while let Some(c) = o.cleanups.pop() {
                capture_drop(c, &mut first);
            }
        }
    }
    if let Some(p) = first {
        resume_unwind(p);
    }
}

pub(super) fn verify(g: &Graph) -> Result<Vec<bool>> {
    let mut nodes = vec![false; g.hot.len()];
    for (position, &root) in g.roots.iter().enumerate() {
        let root = owner(g, root)?;
        if root.parent.is_some() || root.position != position {
            return Err(Error::Invariant);
        }
    }
    for (i, slot) in g.owners.iter().enumerate() {
        if let Some(o) = &slot.value {
            let key = Key {
                index: i as u32,
                generation: slot.generation,
            };
            if let Some(p) = o.parent {
                let p = owner(g, p)?;
                if p.depth.checked_add(1) != Some(o.depth)
                    || p.children.get(o.position) != Some(&Child::Physical(key))
                {
                    return Err(Error::Invariant);
                }
            } else if g.roots.get(o.position) != Some(&key) {
                return Err(Error::Invariant);
            }
            for (position, &child) in o.children.iter().enumerate() {
                match child {
                    Child::Physical(child) => {
                        if owner(g, child)?.parent != Some(key)
                            || owner(g, child)?.position != position
                        {
                            return Err(Error::Invariant);
                        }
                    }
                    Child::Deferred(child) => {
                        header(g, child)?;
                        let c = g.cold[child.index as usize]
                            .as_ref()
                            .ok_or(Error::Invariant)?;
                        if c.owner != key || !c.scope_deferred || c.scope_position != position {
                            return Err(Error::Invariant);
                        }
                    }
                }
            }
            for &node in &o.nodes {
                header(g, node)?;
                if nodes[node.index as usize]
                    || g.cold[node.index as usize]
                        .as_ref()
                        .ok_or(Error::Invariant)?
                        .owner
                        != key
                {
                    return Err(Error::Invariant);
                }
                nodes[node.index as usize] = true;
            }
        }
    }
    Ok(nodes)
}
