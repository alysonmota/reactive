use std::num::NonZeroU32;

/// Eight-byte IDs, never references into a relocatable arena. Generation zero
/// is unrepresentable; Option<SlotId> uses its niche without an extra tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SlotId {
    pub(crate) index: u32,
    pub(crate) generation: NonZeroU32,
}

impl SlotId {
    pub(crate) const fn new(index: u32, generation: u32) -> Option<Self> {
        match NonZeroU32::new(generation) {
            Some(generation) => Some(Self { index, generation }),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Vacant,
    Alive,
    Disposing,
    Retired,
}

struct Slot<T> {
    generation: u32,
    life: Life,
    value: Option<T>,
    // index + 1, so index zero is representable with an Option niche.
    next_free: Option<NonZeroU32>,
}

/// Intrusive free list: removing a slot does not allocate a free-list entry.
pub(crate) struct Arena<T> {
    slots: Vec<Slot<T>>,
    free: Option<NonZeroU32>,
    live: usize,
    #[cfg(test)]
    max_slots: u32,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: None,
            live: 0,
            #[cfg(test)]
            max_slots: u32::MAX,
        }
    }
}

impl<T> Arena<T> {
    #[cfg(feature = "diagnostics")]
    pub(crate) fn allocation_info(&self) -> (usize, usize) {
        (
            self.slots.len(),
            self.slots.capacity() * std::mem::size_of::<Slot<T>>(),
        )
    }
    #[cfg(any(test, feature = "diagnostics", feature = "verification"))]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (SlotId, &T)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.life == Life::Alive)
            .map(|(index, slot)| {
                (
                    SlotId::new(index as u32, slot.generation).expect("nonzero generation"),
                    slot.value.as_ref().expect("live arena value"),
                )
            })
    }
    pub(crate) fn insert(&mut self, value: T) -> SlotId {
        assert!(
            self.can_insert(),
            "arena index exhausted: insertion must be preflighted"
        );
        let index = if let Some(encoded) = self.free {
            let index = (encoded.get() - 1) as usize;
            let slot = &mut self.slots[index];
            self.free = slot.next_free.take();
            slot.life = Life::Alive;
            slot.value = Some(value);
            index
        } else {
            let index = self.slots.len();
            self.slots.push(Slot {
                generation: 1,
                life: Life::Alive,
                value: Some(value),
                next_free: None,
            });
            index
        };
        self.live += 1;
        SlotId::new(index as u32, self.slots[index].generation).expect("nonzero generation")
    }

    /// Runtime callers check before taking user payload ownership, while still
    /// holding the same borrow. No resource is released on this cold limit path.
    pub(crate) fn can_insert(&self) -> bool {
        #[cfg(test)]
        let limit = self.max_slots;
        #[cfg(not(test))]
        let limit = u32::MAX;
        self.free.is_some() || self.slots.len() < limit as usize
    }

    #[cfg(test)]
    pub(crate) fn set_slot_limit(&mut self, limit: u32) {
        self.max_slots = limit;
    }

    pub(crate) fn get(&self, id: SlotId) -> Option<&T> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation.get() || slot.life != Life::Alive {
            return None;
        }
        // SAFETY: Slot fields are private. insert installs Some before returning
        // an Alive slot; renew preserves it; mark_disposing makes it inaccessible;
        // remove takes T and changes life without running user code in between.
        // The checked index/generation/life prove initialized T. Its lifetime is
        // tied to &self, so safe callers cannot mutate/reallocate the arena while
        // this reference is used. No cached pointer or unchecked index escapes.
        Some(unsafe { slot.value.as_ref().unwrap_unchecked() })
    }

    pub(crate) fn get_mut(&mut self, id: SlotId) -> Option<&mut T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation == id.generation.get() && slot.life == Life::Alive {
            // SAFETY: Same Alive => Some invariant as get; &mut self additionally
            // gives exclusive access, including to the returned value.
            Some(unsafe { slot.value.as_mut().unwrap_unchecked() })
        } else {
            None
        }
    }

    /// Renews a live identity without moving its value or touching the free
    /// list. At exhaustion the caller must remove/retire through the normal
    /// path; no mutation occurs on failure.
    pub(crate) fn renew(&mut self, id: SlotId) -> Option<SlotId> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation.get() || slot.life != Life::Alive {
            return None;
        }
        let generation = slot.generation.checked_add(1)?;
        slot.generation = generation;
        Some(SlotId {
            generation: NonZeroU32::new(generation).expect("checked successor"),
            ..id
        })
    }

    /// Internal teardown only; public resolution never accepts disposing slots.
    pub(crate) fn get_teardown(&self, id: SlotId) -> Option<&T> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation.get()
            && matches!(slot.life, Life::Alive | Life::Disposing)
        {
            slot.value.as_ref()
        } else {
            None
        }
    }

    pub(crate) fn get_mut_teardown(&mut self, id: SlotId) -> Option<&mut T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation == id.generation.get()
            && matches!(slot.life, Life::Alive | Life::Disposing)
        {
            slot.value.as_mut()
        } else {
            None
        }
    }

    /// Makes the slot inaccessible before any teardown callback can run.
    pub(crate) fn mark_disposing(&mut self, id: SlotId) -> bool {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return false;
        };
        if slot.generation != id.generation.get() || slot.life != Life::Alive {
            return false;
        }
        slot.life = Life::Disposing;
        true
    }

    /// Returns ownership: caller must drop T outside any runtime borrow.
    pub(crate) fn remove(&mut self, id: SlotId) -> Option<T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation.get()
            || !matches!(slot.life, Life::Alive | Life::Disposing)
        {
            return None;
        }
        let value = slot.value.take();
        self.live -= 1;
        if let Some(generation) = slot.generation.checked_add(1) {
            slot.generation = generation;
            slot.life = Life::Vacant;
            slot.next_free = self.free;
            // The maximum index is reserved; an allocated index is <= MAX-1.
            self.free = NonZeroU32::new(id.index + 1);
        } else {
            slot.life = Life::Retired;
            slot.next_free = None;
        }
        value
    }

    pub(crate) fn len(&self) -> usize {
        self.live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_id_and_option_are_eight_bytes_and_zero_generation_is_rejected() {
        assert_eq!(std::mem::size_of::<SlotId>(), 8);
        assert_eq!(std::mem::size_of::<Option<SlotId>>(), 8);
        assert_eq!(SlotId::new(0, 0), None);
        assert_eq!(
            SlotId::new(0, u32::MAX).unwrap().generation,
            NonZeroU32::MAX
        );
        assert_eq!(std::mem::size_of::<crate::handle::HandleId>(), 16);
    }

    #[test]
    fn index_limit_still_allows_free_slot_reuse_but_never_retired_slot_reuse() {
        let mut arena = Arena::default();
        arena.set_slot_limit(1);
        let original = arena.insert(7);
        assert!(!arena.can_insert());
        arena.remove(original).unwrap();
        assert!(arena.can_insert());
        let current = arena.insert(8);
        assert_eq!(current.index, original.index);
        arena.slots[current.index as usize].generation = u32::MAX;
        let last = SlotId::new(current.index, u32::MAX).unwrap();
        assert_eq!(arena.remove(last), Some(8));
        assert!(!arena.can_insert());
        assert_eq!(arena.get(original), None);
        assert_eq!(arena.get(last), None);
    }

    #[test]
    fn model_checks_slot_initialization_aliasing_and_stale_keys_after_every_transition() {
        for seed in [1_u64, 7, 1337] {
            let mut state = seed;
            let mut arena = Arena::default();
            let mut keys = Vec::new();
            let mut expected = std::collections::HashMap::new();
            // Same transition model, bounded separately for the interpreter.
            let steps = if cfg!(miri) { 64 } else { 256 };
            for step in 0..steps {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let pick = (state >> 32) as usize;
                if keys.is_empty() || pick.is_multiple_of(4) {
                    let id = arena.insert(step);
                    keys.push(id);
                    expected.insert(id, (step, false));
                } else {
                    let id = keys[pick % keys.len()];
                    match pick % 4 {
                        1 => {
                            let old = expected.remove(&id).map(|(value, _)| value);
                            assert_eq!(arena.remove(id), old);
                        }
                        2 => {
                            let alive = expected.get(&id).is_some_and(|(_, disposing)| !disposing);
                            assert_eq!(arena.mark_disposing(id), alive);
                            if alive {
                                expected.get_mut(&id).unwrap().1 = true;
                            }
                        }
                        _ => {
                            if let Some(value) = arena.get_mut(id) {
                                *value += 1;
                                expected.get_mut(&id).unwrap().0 += 1;
                            }
                            let renewal = arena.renew(id);
                            if let Some(new) = renewal {
                                let value = expected.remove(&id).unwrap();
                                expected.insert(new, value);
                                keys.push(new);
                            }
                        }
                    }
                }
                // Audit private initialization invariant before exercising the
                // unchecked extractor: every Alive/Disposing slot contains T.
                for slot in &arena.slots {
                    assert_eq!(
                        slot.value.is_some(),
                        matches!(slot.life, Life::Alive | Life::Disposing)
                    );
                    assert_ne!(slot.generation, 0);
                }
                assert_eq!(arena.len(), expected.len());
                for &id in &keys {
                    let value = expected
                        .get(&id)
                        .filter(|(_, disposing)| !disposing)
                        .map(|(value, _)| value);
                    assert_eq!(arena.get(id), value);
                    assert_eq!(
                        arena.get_teardown(id),
                        expected.get(&id).map(|(value, _)| value)
                    );
                }
                assert!(arena.get(SlotId::new(u32::MAX, 1).unwrap()).is_none());
            }
        }
    }

    #[test]
    fn reuses_slot_but_not_generation() {
        let mut arena = Arena::default();
        let old = arena.insert(7);
        assert_eq!(arena.remove(old), Some(7));
        let new = arena.insert(9);
        assert_eq!(old.index, new.index);
        assert_ne!(old.generation, new.generation);
        assert_eq!(arena.get(old), None);
        assert_eq!(arena.get_mut(old), None);
        assert_eq!(arena.remove(old), None);
        assert_eq!(arena.get(new), Some(&9));
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn disposing_is_inaccessible_and_removable_once() {
        let mut arena = Arena::default();
        let id = arena.insert(7);
        assert!(arena.mark_disposing(id));
        assert!(!arena.mark_disposing(id));
        assert_eq!(arena.get(id), None);
        assert_eq!(arena.get_mut(id), None);
        assert_eq!(arena.remove(id), Some(7));
        assert_eq!(arena.remove(id), None);
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn exhausted_slot_is_retired() {
        let mut arena = Arena::default();
        let id = arena.insert(7);
        arena.slots[id.index as usize].generation = u32::MAX;
        let exhausted = SlotId {
            generation: NonZeroU32::MAX,
            ..id
        };
        assert_eq!(arena.remove(exhausted), Some(7));
        let new = arena.insert(9);
        assert_ne!(new.index, exhausted.index);
        assert_eq!(arena.slots[exhausted.index as usize].life, Life::Retired);
    }

    #[test]
    fn last_two_generations_are_distinct_then_slot_retires_without_wrap() {
        let mut arena = Arena::default();
        let original = arena.insert(7);
        arena.slots[original.index as usize].generation = u32::MAX - 1;
        let penultimate = SlotId {
            generation: NonZeroU32::new(u32::MAX - 1).unwrap(),
            ..original
        };
        assert_eq!(arena.remove(penultimate), Some(7));
        let last = arena.insert(8);
        assert_eq!(last.index, penultimate.index);
        assert_eq!(last.generation, NonZeroU32::MAX);
        assert_eq!(arena.get(penultimate), None);
        assert_eq!(arena.remove(last), Some(8));
        let next = arena.insert(9);
        assert_ne!(next.index, last.index);
        assert_eq!(next.generation.get(), 1);
        assert_eq!(arena.get(last), None);
        assert_eq!(arena.get(original), None);
    }

    #[test]
    fn renew_preserves_value_free_list_and_live_count_and_rejects_old_ids() {
        let mut arena = Arena::default();
        let first = arena.insert(vec![1, 2, 3]);
        let vacant = arena.insert(vec![4]);
        arena.remove(vacant).unwrap();
        let pointer = arena.get(first).unwrap().as_ptr();
        let renewed = arena.renew(first).unwrap();
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.get(renewed).unwrap().as_ptr(), pointer);
        assert!(arena.get(first).is_none());
        assert!(arena.renew(first).is_none());
        assert_eq!(arena.insert(vec![5]).index, vacant.index);
        arena.mark_disposing(renewed);
        assert!(arena.renew(renewed).is_none());
    }

    #[test]
    fn renew_at_exhaustion_leaves_slot_for_normal_retirement() {
        let mut arena = Arena::default();
        let id = arena.insert(7);
        arena.slots[id.index as usize].generation = u32::MAX;
        let exhausted = SlotId {
            generation: NonZeroU32::MAX,
            ..id
        };
        assert!(arena.renew(exhausted).is_none());
        assert_eq!(arena.get(exhausted), Some(&7));
        let value = arena.remove(exhausted).unwrap();
        let next = arena.insert(value);
        assert_ne!(next.index, exhausted.index);
        assert_eq!(arena.get(exhausted), None);
    }

    #[test]
    fn arbitrary_invalid_index_does_not_access_storage() {
        let mut arena = Arena::<u8>::default();
        let invalid = SlotId {
            index: u32::MAX,
            generation: NonZeroU32::MIN,
        };
        assert_eq!(arena.get(invalid), None);
        assert_eq!(arena.remove(invalid), None);
    }
}
