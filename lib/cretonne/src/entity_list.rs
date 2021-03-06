//! Small lists of entity references.
//!
//! This module defines an `EntityList<T>` type which provides similar functionality to `Vec<T>`,
//! but with some important differences in the implementation:
//!
//! 1. Memory is allocated from a `ListPool<T>` instead of the global heap.
//! 2. The footprint of an entity list is 4 bytes, compared with the 24 bytes for `Vec<T>`.
//! 3. An entity list doesn't implement `Drop`, leaving it to the pool to manage memory.
//!
//! The list pool is intended to be used as a LIFO allocator. After building up a larger data
//! structure with many list references, the whole thing can be discarded quickly by clearing the
//! pool.
//!
//! # Safety
//!
//! Entity lists are not as safe to use as `Vec<T>`, but they never jeopardize Rust's memory safety
//! guarantees. These are the problems to be aware of:
//!
//! - If you lose track of an entity list, its memory won't be recycled until the pool is cleared.
//!   This can cause the pool to grow very large with leaked lists.
//! - If entity lists are used after their pool is cleared, they may contain garbage data, and
//!   modifying them may corrupt other lists in the pool.
//! - If an entity list is used with two different pool instances, both pools are likely to become
//!   corrupted.
//!
//! # Implementation
//!
//! The `EntityList` itself is designed to have the smallest possible footprint. This is important
//! because it is used inside very compact data structures like `InstructionData`. The list
//! contains only a 32-bit index into the pool's memory vector, pointing to the first element of
//! the list.
//!
//! The pool is just a single `Vec<T>` containing all of the allocated lists. Each list is
//! represented as three contiguous parts:
//!
//! 1. The number of elements in the list.
//! 2. The list elements.
//! 3. Excess capacity elements.
//!
//! The total size of the three parts is always a power of two, and the excess capacity is always
//! as small as possible. This means that shrinking a list may cause the excess capacity to shrink
//! if a smaller power-of-two size becomes available.
//!
//! Both growing and shrinking a list may cause it to be reallocated in the pool vector.
//!
//! The index stored in an `EntityList` points to part 2, the list elements. The value 0 is
//! reserved for the empty list which isn't allocated in the vector.

use std::marker::PhantomData;

use entity_map::EntityRef;

/// A small list of entity references allocated from a pool.
///
/// All of the list methods that take a pool reference must be given the same pool reference every
/// time they are called. Otherwise data structures will be corrupted.
pub struct EntityList<T: EntityRef> {
    index: u32,
    unused: PhantomData<T>,
}

/// Create an empty list.
impl<T: EntityRef> Default for EntityList<T> {
    fn default() -> Self {
        EntityList {
            index: 0,
            unused: PhantomData,
        }
    }
}

/// A memory pool for storing lists of `T`.
pub struct ListPool<T: EntityRef> {
    // The main array containing the lists.
    data: Vec<T>,

    // Heads of the free lists, one for each size class.
    free: Vec<usize>,
}

/// Lists are allocated in sizes that are powers of two, starting from 4.
/// Each power of two is assigned a size class number, so the size is `4 << SizeClass`.
type SizeClass = u8;

/// Get the size of a given size class. The size includes the length field, so the maximum list
/// length is one less than the class size.
fn sclass_size(sclass: SizeClass) -> usize {
    4 << sclass
}

/// Get the size class to use for a given list length.
/// This always leaves room for the length element in addition to the list elements.
fn sclass_for_length(len: usize) -> SizeClass {
    30 - (len as u32 | 3).leading_zeros() as SizeClass
}

/// Is `len` the minimum length in its size class?
fn is_sclass_min_length(len: usize) -> bool {
    len > 3 && len.is_power_of_two()
}

impl<T: EntityRef> ListPool<T> {
    /// Create a new list pool.
    pub fn new() -> ListPool<T> {
        ListPool {
            data: Vec::new(),
            free: Vec::new(),
        }
    }

    /// Clear the pool, forgetting about all lists that use it.
    ///
    /// This invalidates any existing entity lists that used this pool to allocate memory.
    ///
    /// The pool's memory is not released to the operating system, but kept around for faster
    /// allocation in the future.
    pub fn clear(&mut self) {
        self.data.clear();
        self.free.clear();
    }

    /// Read the length of a list field, if it exists.
    fn len_of(&self, list: &EntityList<T>) -> Option<usize> {
        let idx = list.index as usize;
        // `idx` points at the list elements. The list length is encoded in the element immediately
        // before the list elements.
        //
        // The `wrapping_sub` handles the special case 0, which is the empty list. This way, the
        // cost of the bounds check that we have to pay anyway is co-opted to handle the special
        // case of the empty list.
        self.data.get(idx.wrapping_sub(1)).map(|len| len.index())
    }

    /// Allocate a storage block with a size given by `sclass`.
    ///
    /// Returns the first index of an available segment of `self.data` containing
    /// `sclass_size(sclass)` elements.
    fn alloc(&mut self, sclass: SizeClass) -> usize {
        // First try the free list for this size class.
        match self.free.get(sclass as usize).cloned() {
            Some(head) if head > 0 => {
                // The free list pointers are offset by 1, using 0 to terminate the list.
                // A block on the free list has two entries: `[ 0, next ]`.
                // The 0 is where the length field would be stored for a block in use.
                // The free list heads and the next pointer point at the `next` field.
                self.free[sclass as usize] = self.data[head].index();
                head - 1
            }
            _ => {
                // Nothing on the free list. Allocate more memory.
                let offset = self.data.len();
                // We don't want to mess around with uninitialized data.
                // Just fill it up with nulls.
                self.data.resize(offset + sclass_size(sclass), T::new(0));
                offset
            }
        }
    }

    /// Free a storage block with a size given by `sclass`.
    ///
    /// This must be a block that was previously allocated by `alloc()` with the same size class.
    fn free(&mut self, block: usize, sclass: SizeClass) {
        let sclass = sclass as usize;

        // Make sure we have a free-list head for `sclass`.
        if self.free.len() <= sclass {
            self.free.resize(sclass + 1, 0);
        }

        // Make sure the length field is cleared.
        self.data[block] = T::new(0);
        // Insert the block on the free list which is a single linked list.
        self.data[block + 1] = T::new(self.free[sclass]);
        self.free[sclass] = block + 1
    }

    /// Returns two mutable slices representing the two requested blocks.
    ///
    /// The two returned slices can be longer than the blocks. Each block is located at the front
    /// of the respective slice.
    fn mut_slices(&mut self, block0: usize, block1: usize) -> (&mut [T], &mut [T]) {
        if block0 < block1 {
            let (s0, s1) = self.data.split_at_mut(block1);
            (&mut s0[block0..], s1)
        } else {
            let (s1, s0) = self.data.split_at_mut(block0);
            (s0, &mut s1[block1..])
        }
    }

    /// Reallocate a block to a different size class.
    ///
    /// Copy `elems_to_copy` elements from the old to the new block.
    fn realloc(&mut self,
               block: usize,
               from_sclass: SizeClass,
               to_sclass: SizeClass,
               elems_to_copy: usize)
               -> usize {
        assert!(elems_to_copy <= sclass_size(from_sclass));
        assert!(elems_to_copy <= sclass_size(to_sclass));
        let new_block = self.alloc(to_sclass);

        if elems_to_copy > 0 {
            let (old, new) = self.mut_slices(block, new_block);
            (&mut new[0..elems_to_copy]).copy_from_slice(&old[0..elems_to_copy]);
        }

        self.free(block, from_sclass);
        new_block
    }
}

impl<T: EntityRef> EntityList<T> {
    /// Returns `true` if the list has a length of 0.
    pub fn is_empty(&self) -> bool {
        // 0 is a magic value for the empty list. Any list in the pool array must have a positive
        // length.
        self.index == 0
    }

    /// Get the number of elements in the list.
    pub fn len(&self, pool: &ListPool<T>) -> usize {
        // Both the empty list and any invalidated old lists will return `None`.
        pool.len_of(self).unwrap_or(0)
    }

    /// Get the list as a slice.
    pub fn as_slice<'a>(&'a self, pool: &'a ListPool<T>) -> &'a [T] {
        let idx = self.index as usize;
        match pool.len_of(self) {
            None => &[],
            Some(len) => &pool.data[idx..idx + len],
        }
    }

    /// Get a single element from the list.
    pub fn get(&self, index: usize, pool: &ListPool<T>) -> Option<T> {
        self.as_slice(pool).get(index).cloned()
    }

    /// Get the list as a mutable slice.
    pub fn as_mut_slice<'a>(&'a mut self, pool: &'a mut ListPool<T>) -> &'a mut [T] {
        let idx = self.index as usize;
        match pool.len_of(self) {
            None => &mut [],
            Some(len) => &mut pool.data[idx..idx + len],
        }
    }

    /// Get a mutable reference to a single element from the list.
    pub fn get_mut<'a>(&'a mut self, index: usize, pool: &'a mut ListPool<T>) -> Option<&'a mut T> {
        self.as_mut_slice(pool).get_mut(index)
    }

    /// Removes all elements from the list.
    ///
    /// The memory used by the list is put back in the pool.
    pub fn clear(&mut self, pool: &mut ListPool<T>) {
        let idx = self.index as usize;
        match pool.len_of(self) {
            None => assert_eq!(idx, 0, "Invalid pool"),
            Some(len) => pool.free(idx - 1, sclass_for_length(len)),
        }
        // Switch back to the empty list representation which has no storage.
        self.index = 0;
    }

    /// Appends an element to the back of the list.
    pub fn push(&mut self, element: T, pool: &mut ListPool<T>) {
        let idx = self.index as usize;
        match pool.len_of(self) {
            None => {
                // This is an empty list. Allocate a block and set length=1.
                assert_eq!(idx, 0, "Invalid pool");
                let block = pool.alloc(sclass_for_length(1));
                pool.data[block] = T::new(1);
                pool.data[block + 1] = element;
                self.index = (block + 1) as u32;
            }
            Some(len) => {
                // Do we need to reallocate?
                let new_len = len + 1;
                let block;
                if is_sclass_min_length(new_len) {
                    // Reallocate, preserving length + all old elements.
                    let sclass = sclass_for_length(len);
                    block = pool.realloc(idx - 1, sclass, sclass + 1, len + 1);
                    self.index = (block + 1) as u32;
                } else {
                    block = idx - 1;
                }
                pool.data[block + new_len] = element;
                pool.data[block] = T::new(new_len);
            }
        }
    }

    /// Appends multiple elements to the back of the list.
    pub fn extend<I>(&mut self, elements: I, pool: &mut ListPool<T>)
        where I: IntoIterator<Item = T>
    {
        // TODO: use `size_hint()` to reduce reallocations.
        for x in elements {
            self.push(x, pool);
        }
    }

    /// Inserts an element as position `index` in the list, shifting all elements after it to the
    /// right.
    pub fn insert(&mut self, index: usize, element: T, pool: &mut ListPool<T>) {
        // Increase size by 1.
        self.push(element, pool);

        // Move tail elements.
        let seq = self.as_mut_slice(pool);
        if index < seq.len() {
            let tail = &mut seq[index..];
            for i in (1..tail.len()).rev() {
                tail[i] = tail[i - 1];
            }
            tail[0] = element;
        } else {
            assert_eq!(index, seq.len());
        }
    }

    /// Removes the element at position `index` from the list.
    pub fn remove(&mut self, index: usize, pool: &mut ListPool<T>) {
        let len;
        {
            let seq = self.as_mut_slice(pool);
            len = seq.len();
            assert!(index < len);

            // Copy elements down.
            for i in index..len - 1 {
                seq[i] = seq[i + 1];
            }
        }

        // Check if we deleted the last element.
        if len == 1 {
            self.clear(pool);
            return;
        }

        // Do we need to reallocate to a smaller size class?
        let mut block = self.index as usize - 1;
        if is_sclass_min_length(len) {
            let sclass = sclass_for_length(len);
            block = pool.realloc(block, sclass, sclass - 1, len);
            self.index = (block + 1) as u32;
        }

        // Finally adjust the length.
        pool.data[block] = T::new(len - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{sclass_size, sclass_for_length};
    use ir::Inst;
    use entity_map::EntityRef;

    #[test]
    fn size_classes() {
        assert_eq!(sclass_size(0), 4);
        assert_eq!(sclass_for_length(0), 0);
        assert_eq!(sclass_for_length(1), 0);
        assert_eq!(sclass_for_length(2), 0);
        assert_eq!(sclass_for_length(3), 0);
        assert_eq!(sclass_for_length(4), 1);
        assert_eq!(sclass_for_length(7), 1);
        assert_eq!(sclass_for_length(8), 2);
        assert_eq!(sclass_size(1), 8);
        for l in 0..300 {
            assert!(sclass_size(sclass_for_length(l)) >= l + 1);
        }
    }

    #[test]
    fn block_allocator() {
        let mut pool = ListPool::<Inst>::new();
        let b1 = pool.alloc(0);
        let b2 = pool.alloc(1);
        let b3 = pool.alloc(0);
        assert_ne!(b1, b2);
        assert_ne!(b1, b3);
        assert_ne!(b2, b3);
        pool.free(b2, 1);
        let b2a = pool.alloc(1);
        let b2b = pool.alloc(1);
        assert_ne!(b2a, b2b);
        // One of these should reuse the freed block.
        assert!(b2a == b2 || b2b == b2);

        // Check the free lists for a size class smaller than the largest seen so far.
        pool.free(b1, 0);
        pool.free(b3, 0);
        let b1a = pool.alloc(0);
        let b3a = pool.alloc(0);
        assert_ne!(b1a, b3a);
        assert!(b1a == b1 || b1a == b3);
        assert!(b3a == b1 || b3a == b3);
    }

    #[test]
    fn empty_list() {
        let pool = &mut ListPool::<Inst>::new();
        let mut list = EntityList::<Inst>::default();
        {
            let ilist = &list;
            assert!(ilist.is_empty());
            assert_eq!(ilist.len(pool), 0);
            assert_eq!(ilist.as_slice(pool), &[]);
            assert_eq!(ilist.get(0, pool), None);
            assert_eq!(ilist.get(100, pool), None);
        }
        assert_eq!(list.as_mut_slice(pool), &[]);
        assert_eq!(list.get_mut(0, pool), None);
        assert_eq!(list.get_mut(100, pool), None);

        list.clear(pool);
        assert!(list.is_empty());
        assert_eq!(list.len(pool), 0);
        assert_eq!(list.as_slice(pool), &[]);
    }

    #[test]
    fn push() {
        let pool = &mut ListPool::<Inst>::new();
        let mut list = EntityList::<Inst>::default();

        let i1 = Inst::new(1);
        let i2 = Inst::new(2);
        let i3 = Inst::new(3);
        let i4 = Inst::new(4);

        list.push(i1, pool);
        assert_eq!(list.len(pool), 1);
        assert!(!list.is_empty());
        assert_eq!(list.as_slice(pool), &[i1]);
        assert_eq!(list.get(0, pool), Some(i1));
        assert_eq!(list.get(1, pool), None);

        list.push(i2, pool);
        assert_eq!(list.len(pool), 2);
        assert!(!list.is_empty());
        assert_eq!(list.as_slice(pool), &[i1, i2]);
        assert_eq!(list.get(0, pool), Some(i1));
        assert_eq!(list.get(1, pool), Some(i2));
        assert_eq!(list.get(2, pool), None);

        list.push(i3, pool);
        assert_eq!(list.len(pool), 3);
        assert!(!list.is_empty());
        assert_eq!(list.as_slice(pool), &[i1, i2, i3]);
        assert_eq!(list.get(0, pool), Some(i1));
        assert_eq!(list.get(1, pool), Some(i2));
        assert_eq!(list.get(2, pool), Some(i3));
        assert_eq!(list.get(3, pool), None);

        // This triggers a reallocation.
        list.push(i4, pool);
        assert_eq!(list.len(pool), 4);
        assert!(!list.is_empty());
        assert_eq!(list.as_slice(pool), &[i1, i2, i3, i4]);
        assert_eq!(list.get(0, pool), Some(i1));
        assert_eq!(list.get(1, pool), Some(i2));
        assert_eq!(list.get(2, pool), Some(i3));
        assert_eq!(list.get(3, pool), Some(i4));
        assert_eq!(list.get(4, pool), None);

        list.extend([i1, i1, i2, i2, i3, i3, i4, i4].iter().cloned(), pool);
        assert_eq!(list.len(pool), 12);
        assert_eq!(list.as_slice(pool),
                   &[i1, i2, i3, i4, i1, i1, i2, i2, i3, i3, i4, i4]);
    }

    #[test]
    fn insert_remove() {
        let pool = &mut ListPool::<Inst>::new();
        let mut list = EntityList::<Inst>::default();

        let i1 = Inst::new(1);
        let i2 = Inst::new(2);
        let i3 = Inst::new(3);
        let i4 = Inst::new(4);

        list.insert(0, i4, pool);
        assert_eq!(list.as_slice(pool), &[i4]);

        list.insert(0, i3, pool);
        assert_eq!(list.as_slice(pool), &[i3, i4]);

        list.insert(2, i2, pool);
        assert_eq!(list.as_slice(pool), &[i3, i4, i2]);

        list.insert(2, i1, pool);
        assert_eq!(list.as_slice(pool), &[i3, i4, i1, i2]);

        list.remove(3, pool);
        assert_eq!(list.as_slice(pool), &[i3, i4, i1]);

        list.remove(2, pool);
        assert_eq!(list.as_slice(pool), &[i3, i4]);

        list.remove(0, pool);
        assert_eq!(list.as_slice(pool), &[i4]);

        list.remove(0, pool);
        assert_eq!(list.as_slice(pool), &[]);
        assert!(list.is_empty());
    }
}
